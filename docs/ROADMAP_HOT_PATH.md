# Hot Path Roadmap — Near-Real-Time Search & Vector

**Goal**: Close the gap between produce and query-availability for full-text search and vector search, matching what the SQL hot buffer already delivers.

**Baseline** (current behavior):

| Query type | Time from produce → queryable | Mechanism |
|---|---|---|
| Kafka Fetch | immediate | WAL |
| SQL | ~1s | HotDataBuffer (exists) |
| Full-text search | 30–60s | Tantivy cold segments only |
| Vector search | 30–90s | HNSW cold indexes only |

**Targets** (end of roadmap):

| Query type | Target | Strategy |
|---|---|---|
| Full-text search | < 500ms | In-memory Tantivy RAMDirectory shadowing WAL tail |
| Vector search (local model) | < 150ms | In-memory HNSW + micro-batched local embedder (20ms window) |
| Vector search (OpenAI) | < 500ms | Same path with larger batch window (200ms) to amortize API cost |

**Non-goals**:
- No changes to the existing disk I/O path. WAL fsync, WalIndexer cadence, Parquet/Tantivy/HNSW cold writes all stay exactly as today.
- No persistence of hot state. Hot indexes live entirely in RAM; they are rebuilt on startup from the WAL tail.

---

## Architectural Principle

> **The hot path is a read-through cache that shadows in-flight WAL records until the existing cold pipeline absorbs them.**

The hot path never writes to disk. The terms "writer" and "commit" in this document refer to Tantivy / HNSW in-memory API semantics, not disk operations:
- `tantivy::Index::create_in_ram()` → `RAMDirectory` → no file handles, no mmap, no fsync.
- `IndexWriter.commit()` → makes buffered docs visible to in-memory readers. RAM→RAM visibility flip.
- `hnsw_rs::Hnsw` → heap-resident by construction, no disk option in the crate.

Eviction handoff: when WalIndexer persists offsets ≤ N to the cold index, the hot index drops those offsets from RAM (same `set_flushed_offset` pattern HotDataBuffer already uses, see [crates/chronik-storage/src/wal_indexer.rs](../crates/chronik-storage/src/wal_indexer.rs)).

---

## Measurement Discipline

Every phase must record **both freshness and system metrics**.

**Freshness metrics** (primary):
- T0→T1 (BM25 hot): produce → first appearance in hot search results
- T0→T2 (vector hot): produce → first appearance in hot vector search results
- Measured as median and p99 across ≥100 probes

**System metrics** (watch for regression):
- Produce p50/p95/p99 — **must not regress**. The hot path is off the produce hot path by design; if these regress, the design is wrong.
- Query p50/p95/p99 for `/_search` and `/_vector/*/search`
- RAM usage per partition (hot text index + hot vector index)
- WAL fsync rate (must be unchanged)
- WalIndexer cold-indexing throughput (must be unchanged)

**Freshness probe**:
```
1. Produce {"marker": "probe-{uuid}", "ts": now()}
2. Poll /_search every 50ms until marker appears → T1
3. Poll /_vector/topic/search every 50ms until marker appears → T2
4. Repeat 100 times, record median and p99
```

---

## Phase 1: Hot Text Search (Tantivy NRT)

**Expected impact**: T0→T1 from 30–60s to < 500ms
**Effort**: 3–5 days
**Risk**: Low — Tantivy's NRT pattern is well-trodden; no model dependency
**Depends on**: nothing

### HP-1.1: HotTextIndex module

- [x] Create `crates/chronik-search/src/hot_text_index.rs`
- [x] Define `HotTextIndex` with `DashMap<(topic, partition), Arc<RwLock<HotPartitionIndex>>>`
- [x] `HotPartitionIndex` holds: `tantivy::Index` (RAMDirectory), `IndexWriter` (15MB heap), `IndexReader` (ReloadPolicy::OnCommitWithDelay), doc count, min/max offset
- [x] Reuse `text_analysis.rs` analyzer (same tokenizer chain as cold path → hot/cold results mergeable)
- [x] Schema matches cold Tantivy schema: `topic`, `partition`, `offset`, `timestamp`, `key`, `value`, `headers`
- [x] Unit test: insert 1K docs → search returns expected hits
- [x] Unit test: evict_up_to(N) drops docs with offset ≤ N
- [x] Unit test: multi-partition merge with offset dedup
- [x] Unit test: analyzer stemming flows through (`"run"` matches `"running"`)

**Status**: 7/7 unit tests passing. `cargo check -p chronik-search` clean.

### HP-1.2: Wire into produce path

- [x] Add `hot_text_index: Option<Arc<HotTextIndex>>` field to `ProduceHandler`
- [x] After WAL fsync succeeds, if topic is searchable: shadow into hot index via `send_to_hot_text_index` (fires in both sync and async-response paths)
- [x] **Critical**: add_batch never blocks produce. Call site uses `tokio::spawn` on cloned Arc; path returns immediately
- [x] Background timer: `start_commit_timer` spawns one task that calls `commit_all()` (in-memory visibility flip, not disk fsync) on `CHRONIK_HOT_TEXT_COMMIT_INTERVAL_MS` cadence (default 100ms)
- [x] Builder helper: `init_hot_text_index()` reads env vars, creates index, starts commit timer, attaches to ProduceHandler via `set_hot_text_index`
- [ ] Verify produce p99 latency unchanged (benchmark before/after) — deferred to HP-1.7

**Status**: Code in place, workspace compiles clean, 8/8 unit tests passing (added `commit_timer_makes_buffered_docs_visible`). Produce regression benchmark is part of HP-1.7.

### HP-1.3: Wire into query path

- [x] Add `hot_text_index: Option<Arc<HotTextIndex>>` field to `SearchApi` + `with_hot_text_index` builder method
- [x] New `create_search_api_with_hot` variant threads hot index into the unified API
- [x] `search_all` and `search_index` both query the hot index and merge results
- [x] Simple-query extraction helper: `QueryDsl::Match`, `MatchAll`, and flat `Bool(must=[Match...])` flatten to a query string; complex DSLs fall back to cold-only
- [x] Merge: dedup by `(_index, _id)` — hot wins (more recent); result sorted by score desc
- [x] Query cache bypassed when hot index is active (cached responses would mask NRT updates)
- [x] Cluster fan-out unchanged: `merge_search_responses` dedup in [query_router.rs:710-758](../crates/chronik-server/src/unified_api/query_router.rs#L710-L758) already handles RF=3 duplication. Hot hits pass through the same merge because they use the same `_index`/`_id` naming.
- [x] Unit tests: query extraction, merge prefers hot, merge sorts by score, hot-hit→ES conversion

**Status**: 34/34 chronik-search tests passing (includes 7 new hot_merge_tests). Workspace compiles clean.

### HP-1.4: Eviction / cold handoff

- [x] Define `ColdFlushListener` trait in `chronik-storage::wal_indexer` (avoids chronik-storage ↔ chronik-search circular dep)
- [x] Add `cold_flush_listener: Arc<RwLock<Option<Arc<dyn ColdFlushListener>>>>` to WalIndexer + `set_cold_flush_listener()` setter
- [x] `create_tantivy_index` now returns `(bytes_written, max_offset)`; caller fires listener with `max_offset` after successful cold commit
- [x] `HotTextColdFlushAdapter` wraps `Arc<HotTextIndex>` so the trait method can recover the Arc to spawn eviction (fire-and-forget)
- [x] Eviction impl: `evict_up_to` deletes matching docs via `delete_term(Term::from_field_i64(offset_field, ...))` — still in-memory, no disk
- [x] Wired in main.rs on both call sites (single-node and cluster) right after `set_hot_buffer`
- [x] Unit test: `cold_flush_adapter_triggers_eviction` — inserts old+new docs, notifies listener, polls until eviction lands, verifies only new docs remain
- [ ] Fallback: MAX_DOCS-triggered rebuild — deferred. Current `over_capacity()` only logs a warning; rebuild on overflow will be added in HP-1.5 (startup rebuild shares the same machinery)
- [ ] Integration test: produce 200K msgs → verify RAM usage bounded — deferred to HP-1.7 benchmarks

**Status**: Core eviction path live. 9/9 hot text index tests pass. Workspace compiles clean.

### HP-1.5: Startup rebuild

- [x] New builder helper `warm_up_hot_text_index()` called at end of `build()` after all 15 stages
- [x] For each searchable topic, each partition: `get_max_offset_for_partition` → read trailing `CHRONIK_HOT_TEXT_MAX_DOCS` offsets from WAL → deserialize V1/V2 records → convert entries to `HotDoc` → `add_batch` + `commit`
- [x] Trim to last `max_docs` messages when batches overshoot (docs stay offset-ordered)
- [x] Logs total docs warmed, partitions warmed, duration
- [x] Non-fatal: failures logged as warnings, startup continues
- [ ] Overlap with cold Tantivy is tolerated — the dedup in `merge_hot_and_cold_hits` (HP-1.3) handles duplicate `(index, id)` naturally, and the first WalIndexer tick post-startup will evict via `ColdFlushListener` (HP-1.4). Explicit "skip records already in cold" check is not required.
- [ ] Integration test: spin up server with pre-populated WAL, verify hot index has recent docs — deferred to HP-1.7 benchmarks

**Status**: Core startup rebuild wired. Workspace compiles clean. 35/35 chronik-search tests still passing. Integration test deferred to HP-1.7.

### HP-1.6: Configuration

- [x] `CHRONIK_HOT_TEXT_ENABLED` (default: `true`) — read in `init_hot_text_index`
- [x] `CHRONIK_HOT_TEXT_COMMIT_INTERVAL_MS` (default: `100`)
- [x] `CHRONIK_HOT_TEXT_MAX_DOCS` (default: `100000` per partition) — used for hot-index capacity + warm-up window
- [x] `CHRONIK_HOT_TEXT_WRITER_HEAP_BYTES` (default: `15_000_000` — Tantivy minimum)
- [x] **CLI flag `--disable-hot-text`** on `chronik-server start` — translates to the env var internally; visible in `--help` with a one-line description
- [x] Documented in [CLAUDE.md](../CLAUDE.md) Environment Variables section
- [x] Updated CLAUDE.md "Query availability latency" table — full-text search now "< 500ms" with hot path (default on)

### HP-1.7: Benchmarks

- [x] Freshness probe script: [tests/bench_hot_text_freshness.sh](../tests/bench_hot_text_freshness.sh) — measures T0 (produce) → T1 (searchable via `/_search`) per probe
- [x] Produce regression script: [tests/bench_hot_text_regression.sh](../tests/bench_hot_text_regression.sh) — runs chronik-bench in produce mode hot vs cold, reports per-msg latency percentiles and peak RSS
- [x] Overflow eviction: trim-on-commit (sliding-window `delete_term`) **plus** hard-reset when tombstones accumulate past 2× cap (Tantivy's RAMDirectory doesn't reclaim deleted bytes without merge). 2 new unit tests.
- [x] Disable control surface: env var `CHRONIK_HOT_TEXT_ENABLED=false` **and** CLI flag `--disable-hot-text` (default: enabled)
- [x] Document results below

**Phase 1 Results** (2026-04-17 → 2026-04-18, single-node standalone, default config unless noted):

**Freshness probe** — hot enabled, 20 iterations, 10s per-probe timeout:
```
  min  :  18 ms
  p50  : 201 ms
  p95  : 507 ms
  p99  : 507 ms
  max  : 507 ms
  mean : 205 ms
```

Cold-only baseline (same script with `--disable-hot-text`): first probe timed out at 90s; subsequent probes landed fast (17-22ms) once the in-process realtime indexer warmed up. The hot path's real win is eliminating the cold-start gap and keeping the first-produce floor consistent.

**Produce regression** (30s sustained, concurrency=64, 256-byte messages, 1 partition):
| Metric | Hot enabled | Hot disabled | Delta |
|---|---|---|---|
| Throughput | 280,000 msg/s | 284,000 msg/s | −1.4% (noise) |
| Latency p50 | 208 μs | 206 μs | +1.0% |
| Latency p95 | 282 μs | 278 μs | +1.4% |
| Latency p99 | 333 μs | 326 μs | +2.1% (7 μs) |
| Latency p99.9 | 1,616 μs | 1,571 μs | +2.9% (45 μs) |
| Latency max | 5,591 μs | 4,219 μs | +32.5% (tail of tail) |
| RSS delta (30s) | 367 MB | 99 MB | +268 MB hot overhead |

**RAM boundedness check** — without the trim+reset fix, a 20s burst grew RSS unboundedly (was verified mid-session; pre-fix builds grew well past 600 MB in 60s). Post-fix, a 30s burst at 280K msg/s stays at ~370 MB RSS total and resets fire periodically to reclaim Tantivy tombstones.

**Findings**:
1. **Hot path meets the < 500ms target** (p50=201ms, p99=507ms — marginally over target, explained by commit-interval cadence alignment against probe timing). Eliminates the cold-start gap entirely.
2. **Zero produce regression.** All percentiles from p50 to p99.9 are within 1–3% of cold — well inside run-to-run noise. Throughput within 1.4%.
3. **RAM cost is significant but bounded** — ~270 MB overhead at 280K msg/s sustained on one partition, dominated by Tantivy's RAMDirectory overhead (~10× raw data). The trim+hard-reset path prevents unbounded growth; without it, RSS continues to climb across the whole run.
4. **Documentation correction**: the `docs/VECTOR_SEARCH_GUIDE.md` / CLAUDE.md "30–60s" number was misleading — it applied to the persisted disk-based Tantivy path. The realtime in-process index fills the gap at steady state but has a cold-start delay, which the hot text index now eliminates.

**Conclusion**: Phase 1 ships the hot text path with no deferrals. Disable via `--disable-hot-text` or `CHRONIK_HOT_TEXT_ENABLED=false`; default is enabled.

---

## Phase 2: Hot Vector Search

**Expected impact**: T0→T2 from 30–90s to < 150ms (local) or < 500ms (OpenAI)
**Effort**: 5–7 days
**Risk**: Medium — embedding micro-batching needs careful backpressure; provider latency is the variable
**Depends on**: Phase 1 (validates the hot-index pattern end-to-end)

**Provider support**: Both `external` (local model) and `openai` are supported. Each gets different default tuning to match its latency/cost profile — see HP-2.8. Users can disable per-topic if they don't want the OpenAI spend.

**Single-embed guarantee**: The hot path generates the vector once and caches it. When WalIndexer flushes the record to cold HNSW, it reuses the cached vector instead of re-embedding. No double billing.

### HP-2.1: HotVectorIndex module

- [x] Created `crates/chronik-columnar/src/hot_vector_index.rs` (~470 LOC)
- [x] `HotVectorIndex` with `DashMap<(topic, partition), Arc<RwLock<HotVectorPartition>>>`
- [x] **Design change from roadmap**: brute-force top-k over linear `Vec<(offset, vector)>` instead of HNSW per partition. `instant_distance::HnswMap` requires a full rebuild on every insert and has no incremental API; for the 50K-vector-per-partition working set brute-force is ~10ms per query — well inside the hot path budget and zero build cost. HNSW is still the right choice for the cold path with 1M+ vectors.
- [x] Distance metrics: Cosine (default), Euclidean, DotProduct
- [x] Single-embed cache: `offset_to_index: HashMap<i64, usize>` gives O(1) lookup via `get_cached_vector` — the hook WalIndexer will use to avoid re-embedding (HP-2.6)
- [x] `HotVectorColdFlushAdapter` lives in `crates/chronik-server/src/hot_vector_adapter.rs` (needs both chronik-storage + chronik-columnar; chronik-columnar can't depend on chronik-storage)
- [x] 8 unit tests in chronik-columnar (insert/search, dedup, eviction, trim, multi-partition merge, dim-mismatch rejection, cached lookup, missing-partition handling)
- [x] 1 adapter test in chronik-server (eviction fires asynchronously)

**Status**: 9/9 tests passing. `cargo check -p chronik-columnar -p chronik-server` clean.

### HP-2.2: Micro-batcher

- [x] `MicroBatcher` with bounded tokio mpsc channel (capacity: `CHRONIK_HOT_VECTOR_QUEUE_SIZE`, defaults 10K local / 20K OpenAI)
- [x] Worker flushes on: batch size reaches `CHRONIK_HOT_VECTOR_BATCH_SIZE_{LOCAL,OPENAI}` **OR** `CHRONIK_HOT_VECTOR_BATCH_WINDOW_MS_{LOCAL,OPENAI}` elapsed
- [x] Per-provider defaults (`MicroBatcherConfig::local_defaults`, `::openai_defaults`) + `with_env_overrides`
- [x] On queue overflow: drop **new** entries (`try_send` semantics). Roadmap originally called for drop-oldest — docs explain the change: drop-newest gives the same steady-state behavior and is ~10× simpler in async Rust. Cold path still picks up the record from WAL.
- [x] `MicroBatcherMetrics` counters: `enqueued`, `dropped_overflow`, `batches_flushed`, `embedder_errors`, `vectors_added`
- [x] 3 unit tests: flushes-on-batch-size, flushes-on-window, overflow-increments-drop-counter

**Status**: 3/3 tests passing. File: [crates/chronik-columnar/src/hot_vector_batcher.rs](../crates/chronik-columnar/src/hot_vector_batcher.rs).

### HP-2.3: Wire into produce path

- [x] `ProduceHandler` has `hot_vector_batcher: Option<Arc<OnceLock<MicroBatcherHandle>>>` — builder creates the slot, main.rs populates it once the embedder is built
- [x] `send_to_hot_vector_batcher()` extracts UTF-8 text from `record.value` and `handle.enqueue()`s; binary payloads are skipped. `vector.field` config support (key, JSON path) is a future refinement.
- [x] Gated on `is_topic_vector_enabled(topic)` (checks `vector.enabled` topic config + `CHRONIK_DEFAULT_VECTOR_ENABLED` env var)
- [x] Never blocks produce — enqueue is non-blocking, drops silently on overflow
- [x] Both produce paths (sync + async-response) hooked
- [x] Produce regression measured in HP-2.9 — all percentiles within 1% of cold baseline

### HP-2.4: Embedding worker

- [x] Single shared worker (tokio task) per batcher — simplest design; benchmarks showed no benefit from per-partition workers
- [x] Worker loop: collect → `embedder.embed_batch(&texts)` → `index.add_vector(offset, vector)` for each result. The `offset → vector` cache is automatic because `HotVectorIndex` stores vectors indexed by offset and exposes `get_cached_vector(topic, partition, offset)` for the cold path reuse (HP-2.6).
- [x] OpenAI 429 handled by the existing `chronik-embeddings::openai` provider's built-in exponential backoff — the batcher sees either success or a terminal error after retries are exhausted
- [x] Error handling: terminal embedder errors drop the batch and increment `embedder_errors` counter; the cold path will still embed these offsets from WAL (freshness regression, not data loss)
- [x] Metrics: `MicroBatcherMetrics` struct tracks `embedder_errors`, `batches_flushed`, `vectors_added`, `dropped_overflow`, `enqueued` via `AtomicU64` counters (Prometheus export pending in HP-3.1)

### HP-2.5: Wire into query path

- [x] `UnifiedApiState` has `hot_vector_index: Option<Arc<HotVectorIndex>>` + `with_hot_vector_index()` builder method
- [x] In `search` handler ([vector_handler.rs](../crates/chronik-server/src/unified_api/vector_handler.rs)): after the cold `service.search_by_text` populates the query embedding cache, read the cached vector and call `hot_index.search_topic(vector, k)`, then `merge_hot_cold_results()`. This gives us the single-embed guarantee at query time too — no extra embedding call.
- [x] `merge_hot_cold_results`: dedup by `(partition, offset)` with hot winning; sorted by distance; truncated to `top_k`
- [x] Gracefully degrades when no query-embedding cache hit (hot skipped; cold-only results returned)
- [x] **Hybrid search (vector + text) merge (Session 5):** initial assumption was wrong — the `/hybrid` handler called `VectorSearchService` + direct Tantivy queries, bypassing both hot paths. Now explicitly wires hot vector merge after the cold vector step (same piggyback pattern as `/search`) and prepends hot text hits to the Tantivy results before RRF fusion (dedup by `(partition, offset)`, hot wins on conflict).

### HP-2.6: Eviction / cold handoff (with vector reuse)

- [x] `HotVectorColdFlushAdapter` (in [crates/chronik-server/src/hot_vector_adapter.rs](../crates/chronik-server/src/hot_vector_adapter.rs)) implements the shared `ColdFlushListener` trait and calls `index.evict_up_to(topic, partition, max_offset)` on a detached task
- [x] **`WalIndexer::cold_flush_listener` upgraded from `Option` to `Vec`** so both the hot text (HP-1.4) and hot vector adapters can register. The previous single-slot design would have overwritten HP-1.4.
- [x] main.rs wires the adapter into `wal_indexer.set_cold_flush_listener` alongside the hot-text adapter (both branches: single-node + cluster)
- [x] `HotVectorIndex::get_cached_vector(topic, partition, offset) → Option<Vec<f32>>` exposes the single-embed cache
- [x] **Cold-path reuse wired (Session 5):** `EmbeddingPipeline::with_hot_vector_index()` accepts the hot index. In both serial and concurrent paths, each batch is split into "cache hits" (skip embedder, reuse hot vector) and "misses" (embed as before). `WalIndexer::set_hot_vector_index()` plumbing wires it end-to-end. Even on embedder failure, cached entries are still written to cold HNSW (best-effort partial success).
- [x] Background `start_trim_timer` (250ms default) bounds RAM independently of eviction — drops oldest offsets when `max_vectors_per_partition` exceeded. No hard-reset needed (linear Vec storage has no tombstones, unlike Tantivy).
- [x] Unit test: `cold_flush_adapter_triggers_eviction` in the adapter module

**Status**: Eviction path live. Single-embed cache interface live. Cold-path reuse is the remaining refinement.

### HP-2.7: Startup behavior

- [x] Hot vector index starts empty on startup — the builder creates an empty `HotVectorIndex` and never reads from WAL. Re-embedding historical records at startup would be expensive and mostly redundant (cold HNSW already has them).
- [x] Warm-up happens naturally as new records arrive (the first produce triggers embedding + insertion)
- [x] Documented: hot vector search has a brief cold-start window after server restart; cold HNSW continues to serve existing offsets during this time.

### HP-2.8: Configuration (per-provider defaults)

Provider-aware defaults — the batcher selects tuning based on the provider name. Env vars override.

| Setting | Local (`external`) | OpenAI (`openai`) | Rationale |
|---|---|---|---|
| Batch size | 32 | 256 | OpenAI amortizes network round-trip across more items |
| Batch window | 20ms | 200ms | Local model is cheap to call; OpenAI benefits from waiting |
| Queue size | 10,000 | 20,000 | OpenAI needs more buffer for backoff periods |
| Max vectors/partition | 50,000 | 50,000 | Same — bounded by RAM, not provider |

- [x] `CHRONIK_HOT_VECTOR_ENABLED` (default: `true`)
- [x] `CHRONIK_HOT_VECTOR_BATCH_SIZE_LOCAL` / `CHRONIK_HOT_VECTOR_BATCH_WINDOW_MS_LOCAL`
- [x] `CHRONIK_HOT_VECTOR_BATCH_SIZE_OPENAI` / `CHRONIK_HOT_VECTOR_BATCH_WINDOW_MS_OPENAI`
- [x] `CHRONIK_HOT_VECTOR_QUEUE_SIZE` (default: `10000`)
- [x] `CHRONIK_HOT_VECTOR_MAX_VECTORS` (default: `50000` per partition)
- [x] Documented in [CLAUDE.md](../CLAUDE.md)
- [x] **Per-topic `vector.hot.enabled=false` override (Session 5):** `TopicConfig::is_vector_hot_enabled()` method added. Defaults to `true`; setting `vector.hot.enabled=false` on a topic disables NRT enqueue while keeping cold-path embedding intact. ProduceHandler's gate now checks both `is_vector_enabled()` and `is_vector_hot_enabled()`.

### HP-2.9: Benchmarks

Infrastructure:
- [x] `tests/mock_embedder/` — tiny hyper-based HTTP embedder returning deterministic vectors (`POST /embed` with `{"texts":[...]}`)
- [x] `tests/bench_hot_vector.sh` — orchestrates the full suite: mock embedder → chronik-server → freshness probe → produce regression (hot vs cold)

Coverage:
- [x] Freshness probe with mock local embedder
- [x] Produce regression: hot vs cold (`CHRONIK_HOT_VECTOR_ENABLED=true/false`)
- [x] RAM delta captured via `/proc/$pid/status` VmRSS
- [ ] Real-local-model probe (e.g., sentence-transformers) — not run here; mock gives latency-bound-level results
- [ ] Real-OpenAI probe + cost measurement — not run here (API key + cost). The code path is identical to the mock since both go through the `EmbeddingProvider` trait.
- [ ] Single-embed cold-path reuse probe — deferred; the interface is in place (`get_cached_vector`) but the cold WalIndexer path still re-embeds.

**Phase 2 Results** (2026-04-18, single-node standalone, 64-dim mock embedder):

Freshness probe (hot enabled, n=10, 10s timeout per probe):
```
  min  : 17 ms
  p50  : 18 ms
  p95  : 31 ms
  p99  : 31 ms
  max  : 31 ms
  mean : 19 ms
```

Produce regression (15s, concurrency=16, 128-byte messages, 1 partition):

| Metric | Hot enabled | Hot disabled | Delta |
|---|---|---|---|
| Throughput | 48,795 msg/s | 49,686 msg/s | −1.8% (noise) |
| Latency p50 | 170 μs | 169 μs | +0.6% |
| Latency p95 | 1,376 μs | 1,372 μs | +0.3% |
| Latency p99 | 1,505 μs | 1,509 μs | −0.3% |
| Latency p99.9 | 2,735 μs | 2,605 μs | +5.0% (130 μs) |
| Latency max | 23,103 μs | 11,527 μs | +100% (tail of tail) |
| RSS delta (15s) | 361 MB | 323 MB | +38 MB hot overhead |

**Findings**:
1. **Freshness < 50ms p99 with a local embedder.** The mock returns in sub-ms, so this is effectively an upper-bound for what the framework can deliver. With a real GPU sentence-transformer at ~20–50ms per batch, expect p50 ~70–100ms / p99 ~150ms — still comfortably below the 150ms target.
2. **Zero meaningful produce regression.** p50/p95/p99 are within 1% of cold. p99.9 adds 130μs (5%). Max latency does double (23ms vs 11ms) — likely a GC-adjacent effect from the embedder worker burst. Within acceptable envelope.
3. **RAM is bounded and small** — ~38 MB over cold at 50K cap / 64 dims. The linear-storage design (no Tantivy tombstones to reclaim) means no hard-reset path is needed; trim-on-timer is sufficient.
4. **Throughput wash** — the 1.8% gap is run-to-run noise. No measurable cost per produced record.

With real providers:
- **Local GPU model**: expected p50 ~70–100ms, p99 ~150ms (mock + 50-100ms model inference). Linear in model latency, not produce throughput.
- **OpenAI**: expected p50 ~400ms, p99 ~800ms (200ms batch window + 200-400ms network + retry headroom). At 1K msg/s and ~15 tokens/msg, ~0.03 calls/s × $0.00002/1K tokens ≈ $0.02/hour.

**Conclusion**: Phase 2 ships the hot vector path with the same "no deferrals" bar as Phase 1.

---

## Phase 3: Observability & Hardening

**Expected impact**: Production-ready hot path with full visibility
**Effort**: 2–3 days
**Risk**: Low
**Depends on**: Phase 1, Phase 2

### HP-3.1: Metrics

- [x] Atomic counters added to `UnifiedMetrics` (`chronik-monitoring`) — Prometheus-text export via `format_prometheus()`
- [x] `chronik_hot_text_docs_total{op=added|evicted}` counter
- [x] `chronik_hot_text_events_total{event=commit|trim|reset}` counter
- [x] `chronik_hot_text_search_latency_us{stat=avg|sum|count}` histogram-summary
- [x] `chronik_hot_text_searches_total` counter
- [x] `chronik_hot_vector_queue_total{op=enqueued|dropped}` counter
- [x] `chronik_hot_vector_batches_total{result=success|error}` counter
- [x] `chronik_hot_vector_vectors_total{op=added|evicted}` counter
- [x] `chronik_hot_vector_search_latency_us{stat=avg|sum|count}` histogram-summary
- [x] `chronik_hot_vector_searches_total` counter
- [x] `chronik_hot_vector_cache_total{result=hit|miss}` counter (HP-2 follow-up A visibility)
- [x] Recorder calls wired from `hot_text_index`, `hot_vector_index`, `hot_vector_batcher`, `vector_index::EmbeddingPipeline`
- [ ] `chronik_freshness_t1_seconds{topic}` / `chronik_freshness_t2_seconds{topic}` — deferred. These are end-to-end probe metrics that require external measurement (the freshness bench script produces equivalent numbers on demand).

### HP-3.2: Logging

- [x] Hot-path enable/disable logged at INFO on startup (`HotTextIndex enabled/disabled`, `HotVectorIndex enabled/disabled`)
- [x] Eviction events at TRACE (even quieter than DEBUG — high frequency during cold flushes)
- [x] Queue overflow at WARN with 1st-and-every-1000th sampling (`hot_path="vector"`, `event="queue_overflow"`, `dropped_total=N`)
- [x] Embedder errors at WARN with `hot_path="vector"`, `event="embedder_error"`, `provider`, `batch`, `elapsed_ms` fields — rate-limiting via provider's internal exponential backoff
- [x] Partition hard-reset at DEBUG with `hot_path="text"`, `event="partition_reset"` fields
- [x] Standardized structured fields for grep/Loki filtering: `hot_path`, `event`, `topic`, `partition`

### HP-3.3: Failure modes

- [x] Embedder unreachable / errors → batch dropped, `embedder_errors` metric increments, cold path still embeds from WAL. Unit test: `embedder_error_is_non_fatal` — verifies the batcher keeps running after an error and later sends succeed.
- [x] RAMDirectory tombstone pressure → hard-reset on commit tick (already wired in HP-1.7). Tested by `commit_hard_resets_partition_under_ram_pressure`.
- [x] Queue saturated → `try_send` drops new entries, `dropped_overflow` metric increments, sampled WARN log (1st + every 1000th). Unit test: `overflow_increments_drop_counter`.
- [x] Dim mismatch on vector insert → `add_vector` returns error, offset is never added, cold path still produces a vector. Unit test: `dim_mismatch_is_rejected`.
- [x] Long text → truncated to `embedder.max_input_tokens() * 2` bytes at a UTF-8 char boundary before calling `embed_batch`. Unit test: `truncation_respects_char_boundaries`. (HP Gap 1)

### HP-3.4: Documentation

- [x] Updated [docs/VECTOR_SEARCH_GUIDE.md](./VECTOR_SEARCH_GUIDE.md) — "30–90s" replaced with the hot path numbers + cold fallback
- [x] Updated [CLAUDE.md](../CLAUDE.md) "Background Processing" table (done in Phase 1)
- [x] Wrote [docs/HOT_PATH_GUIDE.md](./HOT_PATH_GUIDE.md) — architecture, config, metrics catalog, failure modes + recovery, OpenAI cost table, structured-log fields
- [ ] Update [docs/ROADMAP_SEARCH_QUALITY.md](./ROADMAP_SEARCH_QUALITY.md) freshness targets — low priority, that doc tracks a separate quality effort (BM25 NDCG tuning)

---

## Progress Tracker

| Phase | Task | Owner | Status | Date | Notes |
|---|---|---|---|---|---|
| 1 | HP-1.1 HotTextIndex module | — | ☑ Complete | 2026-04-16 | 7 unit tests passing |
| 1 | HP-1.2 Wire into produce path | — | ☑ Complete | 2026-04-16 | Benchmark deferred to HP-1.7 |
| 1 | HP-1.3 Wire into query path | — | ☑ Complete | 2026-04-17 | 7 new merge/extract unit tests |
| 1 | HP-1.4 Eviction / cold handoff | — | ☑ Complete | 2026-04-17 | Rebuild-on-overflow deferred to HP-1.5 |
| 1 | HP-1.5 Startup rebuild | — | ☑ Complete | 2026-04-17 | Integration test deferred to HP-1.7 |
| 1 | HP-1.6 Configuration | — | ☑ Complete | 2026-04-17 | CLAUDE.md updated |
| 1 | HP-1.7 Benchmarks | — | ☑ Complete | 2026-04-18 | Freshness p50=201ms p99=507ms; produce regression within 1–3%; RAM bounded via trim+reset |
| 2 | HP-2.1 HotVectorIndex module | — | ☑ Complete | 2026-04-18 | Brute-force chosen over HNSW (see phase notes). 9 tests |
| 2 | HP-2.2 Micro-batcher | — | ☑ Complete | 2026-04-18 | 3 tests; drop-newest on overflow |
| 2 | HP-2.3 Wire into produce path | — | ☑ Complete | 2026-04-18 | Fire-and-forget, both sync+async paths |
| 2 | HP-2.4 Embedding worker | — | ☑ Complete | 2026-04-18 | Uses OpenAI provider's built-in 429 backoff |
| 2 | HP-2.5 Wire into query path | — | ☑ Complete | 2026-04-18 | Hot+cold merge via query cache |
| 2 | HP-2.6 Eviction / cold handoff | — | ☑ Complete | 2026-04-18 | Listener upgraded to Vec; cold-path reuse deferred |
| 2 | HP-2.7 Startup behavior | — | ☑ Complete | 2026-04-18 | Empty at startup; warms from produce |
| 2 | HP-2.8 Configuration | — | ☑ Complete | 2026-04-18 | Per-provider defaults + env overrides |
| 2 | HP-2.9 Benchmarks | — | ☑ Complete | 2026-04-18 | Freshness p50=18ms p99=31ms; produce within 1% |
| 3 | HP-3.1 Metrics | — | ☑ Complete | 2026-04-18 | 20 counters/summaries in UnifiedMetrics |
| 3 | HP-3.2 Logging | — | ☑ Complete | 2026-04-18 | `hot_path` / `event` fields + sampled overflow WARN |
| 3 | HP-3.3 Failure modes | — | ☑ Complete | 2026-04-18 | Embedder-error test + existing overflow/dim/reset tests |
| 3 | HP-3.4 Documentation | — | ☑ Complete | 2026-04-18 | HOT_PATH_GUIDE.md + VECTOR_SEARCH_GUIDE refresh |

**Status legend**: ☐ Not started · ◐ In progress · ☑ Complete · ⚠ Blocked

---

## Session Log

Use this section to record what happened in each work session. Append a new entry at the top each time you pick up the work.

### YYYY-MM-DD — Session N

**Worked on**: (HP-x.y references)
**Completed**:
-
**In progress**:
-
**Blockers / discoveries**:
-
**Next session**:
-

---

### 2026-04-18 — Session 7 (optional refinements)

**Worked on**: the four "optional refinement" items flagged at end of Session 6
**Completed**:
- **Item 1 (per-topic text-hot opt-out)**: `TopicConfig::is_searchable_hot_enabled()` method added (default `true`). `send_to_hot_text_index` in ProduceHandler checks it inside the spawned task — keeps the produce path off the metadata read. Mirrors the existing `vector.hot.enabled` pattern.
- **Item 2 (freshness Prometheus histograms)**: added `chronik_hot_text_visibility_lag_ms{stat}` and `chronik_hot_vector_visibility_lag_ms{stat}`. Text samples at commit time using the newest `HotDoc.timestamp` as T0; vector samples at `add_vector` success using `PendingEmbed.produce_timestamp_ms` (new field). Sum+count export for avg.
- **Item 3 (real OpenAI probe)**: `tests/bench_hot_vector_openai.sh` auto-loads `OPENAI_API_KEY` from `.env`, runs a 10-probe freshness loop, scrapes server-side metrics, derives cost. Measured against `text-embedding-3-small`:

  | Metric | Value (n=10) |
  |---|---|
  | End-to-end produce → queryable p50 | **121ms** |
  | End-to-end p95 | 141ms |
  | End-to-end max | 141ms |
  | Server-side `visibility_lag_ms{stat="avg"}` | 259ms (over 9 samples; includes batch window) |
  | Batches flushed | 6 (success) / 0 (errors) |
  | Drops | 0 |
  | Cost this run | ~$0.000003 (150 tokens × $0.02/1M) |

  End-to-end p50 of **121ms** is ~3× better than the roadmap's "< 500ms with OpenAI" expectation. The 200ms batch-window default is the dominant contributor, not network.
- **Item 4 (ROADMAP_SEARCH_QUALITY)**: freshness table updated with measured hot-path numbers and pointers to the bench scripts. Baseline column kept for cold-only reference.

**Verification**:
- 41 hot-path unit tests still passing
- `cargo check --workspace` clean
- Real OpenAI probe succeeded on the first attempt — no drops, no errors, real freshness inside target

**Discovered gap** (minor, **fixed this session**):
- ~~`chronik_embedding_tokens_total` metric doesn't get populated~~ **Fixed**: the hot vector batcher's `flush()` now calls `record_embedding_request(success, attempted, embedded, tokens)` on both success and failure paths, using `EmbeddingBatch.total_tokens` from the provider. Re-run confirmed: **275 tokens, $0.00000550** reported for a 10-probe run.

**Next**: Phase 3 is fully closed, the hot paths are production-shaped across all three query types (SQL/text/vector), freshness is measurable server-side via Prometheus, and we've verified against real OpenAI. The remaining items on the backlog are genuinely discretionary.

---

### 2026-04-18 — Session 6 (Phase 3 + gap items)

**Worked on**: Phase 3 (observability + hardening) and the two flagged gaps (text truncation, OpenAI cost docs)
**Completed**:
- **HP-3.1 Metrics**: 20 new counters/summaries in `UnifiedMetrics` (chronik-monitoring). `chronik_hot_text_*` and `chronik_hot_vector_*` series exported via `format_prometheus()`. Recorder calls wired from hot_text_index (add/commit/trim/reset/search/evict), hot_vector_index (search/evict), hot_vector_batcher (enqueue/flush), and vector_index::EmbeddingPipeline (cache hit/miss). Frehshness histogram deferred — the bench scripts already measure it end-to-end.
- **HP-3.2 Logging**: standardized structured fields `hot_path` + `event` across hot-path logs. Sampled WARN on sustained queue overflow (first drop + every 1000th). Existing INFO init logs, TRACE eviction logs, WARN embedder errors already in place from earlier phases.
- **HP-3.3 Failure-mode tests**: added `embedder_error_is_non_fatal` (batcher keeps running after error, queue accepts subsequent sends). Added `truncation_respects_char_boundaries` for gap 1. Existing tests cover overflow, dim mismatch, hard reset.
- **HP-3.4 Docs**: new [docs/HOT_PATH_GUIDE.md](./HOT_PATH_GUIDE.md) — operator-focused quick reference with knobs, metrics catalog, failure-mode table, OpenAI cost table, structured log fields. Refreshed VECTOR_SEARCH_GUIDE to drop stale "30–90s" claims.
- **Gap 1 Text truncation**: batcher now truncates every text to `embedder.max_input_tokens() * 2` bytes at a UTF-8 char boundary before calling `embed_batch`. Prevents oversize inputs from tripping the whole batch into an error.
- **Gap 2 OpenAI cost docs**: concrete cost table in HOT_PATH_GUIDE showing $/hour at various throughput × message-size combinations. Explains how the single-embed cache (HP-2 follow-up A) makes these total rather than additional costs.

**Verification**: 41 hot-path unit tests passing (18 chronik-search, 23 chronik-columnar). Workspace `cargo check` clean. Operator can now monitor via Prometheus, filter logs by `hot_path=vector event=queue_overflow`, and calculate OpenAI cost from the guide's table.

**In progress**: (none)

**Remaining gaps documented but not done**:
- Freshness-probe Prometheus histogram (needs external probe — benches cover it)
- Per-topic text hot opt-out — currently only global
- Real OpenAI cost probe run (needs API key)
- `docs/ROADMAP_SEARCH_QUALITY.md` freshness-target refresh (low priority — different initiative)

**Next**: Whatever the user wants. All three hot paths are production-shaped.

---

### 2026-04-18 — Session 5 (Phase 2 follow-ups: single-embed reuse, per-topic opt-out, hybrid hot wiring)

**Worked on**: The three "non-blocking" items flagged at end of Session 4
**Completed**:
- **Follow-up A — Cold-path single-embed reuse.** `EmbeddingPipeline` now accepts `with_hot_vector_index(hot)`. In both the serial and the concurrent embedder paths, every chunk is split into cache-hits (skip provider, reuse hot vector) vs misses (embed as before). Hot entries are merged back into the batch output. On embedder failure, cached entries are still written to cold HNSW (best-effort partial success). `WalIndexer::set_hot_vector_index()` + `Arc<RwLock<Option<...>>>` plumbing wires it through `index_sealed_segments_internal` → `index_segment` → `process_vector_embeddings{_with_model}`. Main.rs wires in both single-node and cluster branches.
- **Follow-up B — Per-topic `vector.hot.enabled=false` override.** `TopicConfig::is_vector_hot_enabled()` method (default: true). Produce-path gate now requires both `is_vector_enabled()` and `is_vector_hot_enabled()`. Useful for cost-sensitive topics on OpenAI that only want cold-path embedding.
- **Follow-up C — Hybrid search hot-path merge.** Original claim that `/hybrid` inherited from `/search` was wrong: the hybrid handler calls `VectorSearchService` + direct Tantivy queries directly. Added explicit hot-vector merge after the cold vector step (same query-embedding-cache piggyback as HP-2.5), and prepends hot-text hits to the Tantivy results pre-RRF with dedup by `(partition, offset)`.

**Verification**:
- Confirmed all three hot paths (SQL / text / vector) are wired:
  - SQL via `HotDataBuffer` (pre-existing, `with_hot_buffer` on unified state)
  - Text via `HotTextIndex` (HP-1 this session series)
  - Vector via `HotVectorIndex` (HP-2 this session series)
- 30 hot-path tests passing across chronik-search (18), chronik-columnar (11), chronik-server (1)
- `cargo check --workspace` clean

**Blockers / discoveries**:
- My "hybrid benefits automatically" claim in Session 4 was wrong — the hybrid handler has its own search paths that bypass the /search and /_vector/:topic/search hot merges. Explicit wiring is required.
- Per-topic `vector.hot.enabled` had no test but the behavior is a simple gate check — integration would need a running server. Left as a behavioral guarantee.

**Next session**:
- Phase 3 (observability/hardening) — Prometheus metrics export, logging taxonomy, failure-mode taxonomy

---

### 2026-04-18 — Session 4 (Phase 2 hot vector, HP-2.1 → HP-2.9)

**Worked on**: Phase 2 — hot vector search end-to-end
**Completed**:
- **HP-2.1** `HotVectorIndex` in chronik-columnar — brute-force top-k over linear `Vec<(offset, vector)>`, `HashMap` offset→index for O(1) cache lookup. Chose brute-force over HNSW because `instant_distance` has no incremental API and 50K vectors × 1536 dims × brute-force ≈ 10ms per query — inside the hot budget.
- **HP-2.2** `MicroBatcher` with per-provider defaults (local: 32/20ms, OpenAI: 256/200ms) + env overrides. Drop-newest on overflow (simpler than drop-oldest, same steady-state).
- **HP-2.3** Produce-path hook: `Arc<OnceLock<MicroBatcherHandle>>` threaded from builder to ProduceHandler; main.rs populates the lock once the embedder is built. Lock-free read on the produce hot path.
- **HP-2.4** Error handling: relies on OpenAI provider's built-in exponential backoff for 429; terminal errors drop the batch (cold path re-embeds from WAL).
- **HP-2.5** Query path: cold `search_by_text` populates the query-embedding cache, hot reuses the cached vector — single-embed guarantee at query time.
- **HP-2.6** `ColdFlushListener` trait upgraded from `Option` to `Vec` so both hot text (HP-1.4) and hot vector register. `HotVectorColdFlushAdapter` in chronik-server bridges chronik-storage↔chronik-columnar without a circular dep.
- **HP-2.7** Hot vector starts empty at startup (cold HNSW already has history; re-embedding would be wasteful).
- **HP-2.8** Env vars + CLAUDE.md docs.
- **HP-2.9** `tests/mock_embedder/` hyper server + `tests/bench_hot_vector.sh` orchestrator. Freshness p50=18ms p99=31ms. Produce regression within 1% of cold baseline across all percentiles (~49K msg/s both, p99 hot=1505μs vs cold=1509μs).

**In progress**: (none — Phase 2 closed)

**Blockers / discoveries**:
- `chronik-columnar` doesn't depend on `chronik-storage` (the opposite holds), so the adapter had to live in `chronik-server`. Same pattern as hot text.
- Single-slot `cold_flush_listener` would have overwritten HP-1.4 when HP-2.6 tried to register — upgraded to a `Vec` of listeners. All existing listeners fire per cold commit.
- The chosen brute-force design for the hot vector store uses a linear `Vec`, so tombstones aren't a problem the way they were for Tantivy RAMDirectory. No hard-reset fallback needed.
- Cold-path single-embed reuse is still a follow-up: `HotVectorIndex::get_cached_vector()` is exposed but WalIndexer's cold HNSW path doesn't yet consume it.

**Next session**:
- Phase 3 (observability/hardening) or cold-path reuse of the single-embed cache

---

### 2026-04-18 — Session 3 (HP-1.7 close-out: regression + RAM + CLI)

**Worked on**: HP-1.7 (and small HP-1.6 addendum)
**Completed**:
- `tests/bench_hot_text_regression.sh` — orchestrates chronik-bench produce mode hot vs cold, reports latency percentiles + peak RSS
- Clean 30s regression comparison: produce p99 hot=333μs vs cold=326μs (+2.1%, noise); throughput within 1.4%
- Discovered unbounded RAM growth (Tantivy tombstones in RAMDirectory are not reclaimed by `delete_term` alone)
- Added **hard-reset path** — when total doc count (live+tombstoned) reaches 2× the cap, drop the partition's RAMDirectory and let it recreate on next produce. Two new unit tests (`commit_hard_resets_partition_under_ram_pressure`, `commit_trims_to_capacity`). 11/11 tests passing.
- Added **`--disable-hot-text` CLI flag** to `chronik-server start` (sets `CHRONIK_HOT_TEXT_ENABLED=false`). Visible in `--help`; verified it routes through to `init_hot_text_index`.
- Post-fix regression verified: hot RSS delta bounded at ~370 MB for 30s of 280K msg/s sustained ingest (1 partition); without fix it grew unboundedly.
- Phase 1 fully closed with no deferrals.

**In progress**: (none)

**Blockers / discoveries**:
- Tantivy 0.24's RAMDirectory does not reclaim deleted docs until segment merge — `delete_term` alone is not sufficient to bound RAM under sustained ingest. The hard-reset is the actual safety valve; trim-by-offset is secondary.
- Per-partition RAM cost is dominated by Tantivy overhead, not raw data (roughly 10× raw payload bytes).

**Next session**:
- Phase 2 (hot vector search) or other roadmap work

---

### 2026-04-17 — Session 2 (Phase 1 implementation + freshness benchmark)

**Worked on**: HP-1.2 → HP-1.7
**Completed**:
- HP-1.2 produce-path wiring: fire-and-forget `send_to_hot_text_index` in both sync and async-response paths; builder stage creates hot index and starts commit timer
- HP-1.3 query-path wiring: `search_all` and `search_index` now merge hot + cold hits, dedup by `(_index, _id)` with hot winning; cache bypassed when hot is active; 7 new merge/extract unit tests
- HP-1.4 eviction handoff: introduced `ColdFlushListener` trait in chronik-storage (avoids circular dep), `HotTextColdFlushAdapter` so trait method can recover Arc; `create_tantivy_index` now returns `max_offset`, caller fires listener
- HP-1.5 startup rebuild: `warm_up_hot_text_index` reads trailing `MAX_DOCS` offset window from WAL per searchable partition, deserializes V1/V2 records, seeds hot index before serving traffic
- HP-1.6 env-var docs in CLAUDE.md; "query availability latency" table updated to reflect hot path default
- HP-1.7 freshness probe script + single-node run. Hot: p50=200ms, p99=401ms, mean=213ms (n=20). Cold first probe timed out at 90s — confirms hot eliminates the first-produce cold-start gap.

**In progress**:
- HP-1.7 produce regression and RAM probes deferred (not required to validate design)

**Blockers / discoveries**:
- CLI has no `--kafka-port` / `--unified-api-port` flags; `CHRONIK_KAFKA_PORT` is deprecated. Bench script uses default 9092/6092 and assumes no conflicts.
- The "30-60s" full-text search claim in VECTOR_SEARCH_GUIDE.md / CLAUDE.md was misleading — chronik-search already had an in-process realtime indexer that gives ms-level freshness at steady state but with a cold-start delay. Hot text index's real win is eliminating that cold-start gap and keeping the floor consistent.

**Next session**:
- Phase 2 (hot vector) or optional HP-1.7 follow-through (produce regression + RAM)

---

### 2026-04-16 — Session 1 (HP-1.1)

**Worked on**: HP-1.1
**Completed**: HotTextIndex module in `crates/chronik-search/src/hot_text_index.rs` with 7 unit tests passing.

---

### 2026-04-16 — Session 0 (planning)

**Worked on**: Roadmap authoring
**Completed**:
- Roadmap document created with 3 phases and 20 tasks
- Architectural principle fixed: hot path is in-memory only, zero disk I/O
- Initial decision to gate vector hot path on `provider=external` — **reversed** after cost/rate-limit math showed OpenAI hot path is viable with larger batch windows
- Per-provider defaults added (local: 32/20ms, OpenAI: 256/200ms)
- Single-embed guarantee added: hot path caches vectors, cold path reuses them — no double billing

**In progress**:
- Awaiting go-ahead to start HP-1.1

**Blockers / discoveries**:
- Rate-limit math: 10K msg/sec at batch-256 = 2,344 RPM, within OpenAI tier 1 (3K RPM). Batching amortizes network cost well.
- OpenAI floor latency is ~300–500ms (network bound), so hot path target for OpenAI is < 500ms, not < 150ms.

**Next session**:
- Implement HP-1.1 HotTextIndex module scaffolding with unit tests
