# Hot Path Operator Guide

Chronik ships three in-memory "hot paths" that shadow the WAL tail so queries don't wait for the background WalIndexer tick (~30s) before newly-produced data is searchable:

| Query type | Hot mechanism | Target freshness | Default |
|---|---|---|---|
| SQL (`/_sql`) | `HotDataBuffer` — Arrow MemTable of recent WAL records | sub-second | **on** |
| Full-text search (`/_search`) | `HotTextIndex` — in-memory Tantivy RAMDirectory | < 500ms | **on** |
| Vector search (`/_vector/:topic/search`) | `HotVectorIndex` — brute-force + `MicroBatcher` | < 150ms (local model) / < 500ms (OpenAI) | **on** |

All hot paths are **fire-and-forget from the produce thread** — none of them block Kafka produce. If the hot path falls behind or fails, the cold path (WalIndexer → Parquet/Tantivy/HNSW) still ingests every record from WAL. You get a temporary freshness regression, not data loss.

---

## Quick ops mental model

```
Producer → WAL (fsync) → response (~2-10ms)
              │
              ├──(fire-and-forget)──▶ Hot text index  (RAM, sub-second visibility)
              ├──(fire-and-forget)──▶ Hot vector batcher → embedder → Hot vector index
              ├──(background read)──▶ Hot SQL buffer (refreshes every ~1s)
              │
              └──(WalIndexer tick, 30s)──▶ Cold Tantivy / Parquet / HNSW
                                                   │
                                                   └─(cold flush listener)──▶ evict from hot indexes
```

Read path merges hot + cold, dedupping by `(topic, partition, offset)` with **hot winning on conflict** (hot data is more recent than cold).

---

## Enabling / disabling

### Globally (server-level)

| Env var | Default | Effect |
|---|---|---|
| `CHRONIK_HOT_BUFFER_ENABLED` | `true` | Hot SQL buffer |
| `CHRONIK_HOT_TEXT_ENABLED` | `true` | Hot text index |
| `CHRONIK_HOT_VECTOR_ENABLED` | `true` | Hot vector index |

Also available as a CLI flag:
```bash
chronik-server start --disable-hot-text   # same as CHRONIK_HOT_TEXT_ENABLED=false
```

### Per-topic

| Topic config | Default | Effect |
|---|---|---|
| `vector.hot.enabled` | `true` | Disable NRT enqueue for one topic (cost-sensitive OpenAI topics, etc.) — cold-path embedding still runs. Requires `vector.enabled=true`. |

Topic-level opt-out for text/SQL isn't exposed yet — disable globally if you don't want it.

---

## Sizing knobs

### Hot text index
```bash
CHRONIK_HOT_TEXT_COMMIT_INTERVAL_MS=100   # visibility flip cadence
CHRONIK_HOT_TEXT_MAX_DOCS=100000          # soft cap per (topic, partition)
CHRONIK_HOT_TEXT_WRITER_HEAP_BYTES=15000000  # Tantivy writer heap (15MB minimum)
```

A partition that exceeds 2× `MAX_DOCS` total docs (live + tombstones) is hard-reset — the RAMDirectory is dropped and recreated fresh on the next produce. This is how we reclaim memory that Tantivy's `delete_term` can't free (tombstones accumulate in segments until merge).

### Hot vector index (per provider)

| Knob | Local default | OpenAI default |
|---|---|---|
| Batch size | 32 | 256 |
| Batch window | 20ms | 200ms |
| Queue capacity | 10,000 | 20,000 |
| Max vectors / partition | 50,000 | 50,000 |

Env overrides:
```bash
CHRONIK_HOT_VECTOR_BATCH_SIZE_LOCAL=32
CHRONIK_HOT_VECTOR_BATCH_WINDOW_MS_LOCAL=20
CHRONIK_HOT_VECTOR_BATCH_SIZE_OPENAI=256
CHRONIK_HOT_VECTOR_BATCH_WINDOW_MS_OPENAI=200
CHRONIK_HOT_VECTOR_QUEUE_SIZE=10000
CHRONIK_HOT_VECTOR_MAX_VECTORS=50000
```

OpenAI defaults are deliberately looser: bigger batches amortize the network round-trip; the longer queue absorbs rate-limit backoff periods.

### Hot SQL buffer
```bash
CHRONIK_HOT_BUFFER_MAX_RECORDS=100000    # per partition
CHRONIK_HOT_BUFFER_REFRESH_MS=1000       # how often the MemTable rebuilds
```

---

## Metrics

All hot-path metrics export as `chronik_hot_*` in the Prometheus endpoint. Key signals:

### Throughput / volume
- `chronik_hot_text_docs_total{op}` — `added` / `evicted`
- `chronik_hot_vector_queue_total{op}` — `enqueued` / `dropped`
- `chronik_hot_vector_vectors_total{op}` — `added` / `evicted`
- `chronik_hot_vector_batches_total{result}` — `success` / `error`

### Health
- `chronik_hot_text_events_total{event}` — `commit` / `trim` / `reset`. A rising `reset` rate means the hot text index is being repeatedly dumped — lower produce rate, raise `MAX_DOCS`, or accept the reset churn.
- `chronik_hot_vector_queue_total{op="dropped"}` — non-zero means the embedder can't keep up with produce. Either raise queue capacity, switch to a faster provider, or lower batch window (local only — OpenAI batching is throughput-bound).
- `chronik_hot_vector_batches_total{result="error"}` — sustained non-zero means the embedder is returning errors past its retry budget. Check `chronik_embeddings` logs.
- `chronik_hot_vector_cache_total{result}` — `hit` / `miss`. Higher hit rate = higher embedding-cost savings. A cold start shows all misses until the hot path warms up (~1 min).

### Latency
- `chronik_hot_text_search_latency_us{stat}` — `avg` / `sum` / `count`
- `chronik_hot_vector_search_latency_us{stat}` — same

---

## Failure modes & recovery

| Symptom | Likely cause | What to do |
|---|---|---|
| Freshness drifts above target on vector | Embedder is slow / hitting rate limits / unreachable | Check `chronik_hot_vector_batches_total{result="error"}` and `chronik_hot_vector_queue_total{op="dropped"}`. Cold path is still embedding — wait for next WalIndexer tick or switch to a local provider. |
| Hot text RSS growing unboundedly | `MAX_DOCS` set too high, or produce rate exceeds trim/commit cadence | Metric: rising `reset` count means it's self-correcting. Lower `MAX_DOCS` to force smaller RAMDirectories. |
| Cold path still embedding everything | Topic has `vector.hot.enabled=false`, or hot vector disabled globally, or embedder wasn't configured | Check `chronik_hot_vector_cache_total{result="hit"}` — zero means the hot path isn't priming cold. |
| "hot vector batcher queue full" WARN in logs | Sustained embedder backpressure | Counter: `chronik_hot_vector_queue_total{op="dropped"}`. Re-tune batch/window, or accept that cold path will fill the gap. |
| Queries slower right after server restart | Hot text warms from WAL tail; hot vector starts empty | Wait 30s-1min. Hot text warmup runs once at startup (`CHRONIK_HOT_TEXT_MAX_DOCS` worth of WAL tail); hot vector warms naturally from new produces. |

The three hot paths degrade independently. Disabling one (via env var or CLI) doesn't affect the others. The cold path is always on.

---

## Debugging

Structured log fields all hot-path events use:
- `hot_path = "text" | "vector"` — discriminator
- `event = "partition_reset" | "queue_overflow" | "embedder_error"` — for grep/Loki filters
- `topic`, `partition` — for partitioned systems

Enable per-module debug for deeper visibility:
```bash
RUST_LOG="info,chronik_search::hot_text_index=debug,chronik_columnar::hot_vector_batcher=debug"
```

Benchmarks live at:
- [tests/bench_hot_text_freshness.sh](../tests/bench_hot_text_freshness.sh)
- [tests/bench_hot_text_regression.sh](../tests/bench_hot_text_regression.sh)
- [tests/bench_hot_vector.sh](../tests/bench_hot_vector.sh)

---

## OpenAI cost estimate

Applies only when `vector.embedding.provider=openai`. With `text-embedding-3-small` at the current rate of $0.02 per 1M input tokens, and assuming ~1 token per 4 bytes of English text:

| Throughput | Avg msg size | Tokens/sec | $/hour | $/month (24×7) |
|---|---|---|---|---|
| 100 msg/s | 100 B (log line) | 2,500 | $0.0002 | $0.13 |
| 1,000 msg/s | 100 B | 25,000 | $0.0018 | $1.30 |
| 1,000 msg/s | 500 B (JSON doc) | 125,000 | $0.0090 | $6.48 |
| 10,000 msg/s | 500 B | 1,250,000 | $0.090 | $64.80 |
| 10,000 msg/s | 5 KB (long text) | 12,500,000 | $0.900 | $648 |

The single-embed cache (HP-2 follow-up A) means the cold pipeline reuses vectors the hot path already embedded — at steady state with the hot path enabled, these are your **total** embedding costs, not hot-path-additional costs.

Watch `chronik_hot_vector_cache_total{result="hit"}` vs `{result="miss"}` in Grafana to see the cost savings (each `miss` was an embedder call; each `hit` was free). A hot-path partial miss (queue overflow, embedder timeout) produces a cache miss when the cold pipeline later catches up — that's the only way you pay to re-embed the same record.

**Per-topic opt-out** to cap cost on specific topics:
```bash
# via kafka-topics.sh topic config
vector.hot.enabled=false
```
Cold-path embedding still runs at the WalIndexer tick cadence (~30s), so you get eventual indexing without the hot-path freshness SLA.

---

## What hot paths do NOT cover

- **Exactly-once**: hot paths are best-effort. The cold path is still source-of-truth; if you need to know "is every produced record queryable", check high-watermark reported by Kafka.
- **Long-text inputs** to the embedder: currently we pass `record.value` as-is. Providers with an input-token limit will reject the batch; that batch is dropped and the cold path re-embeds the truncated version later. Truncation in the producer path is on the roadmap.
- **Cross-partition semantic dedup** (two different offsets with identical text get two distinct vectors).
- **Per-topic text hot opt-out** — only global for now.

See [ROADMAP_HOT_PATH.md](./ROADMAP_HOT_PATH.md) for the full design history and measured benchmarks.
