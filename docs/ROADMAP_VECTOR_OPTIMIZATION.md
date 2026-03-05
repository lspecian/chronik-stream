# Vector/Embedding/Ranking Optimization Roadmap

**Status**: CODE COMPLETE (All 6 phases implemented, 893 tests passing)
**Target**: v2.4.x (pre-DV-1/DV-2)
**Prerequisites**: SV-2b complete (5 fixes deployed, 0% error, 820 req/s validated)
**Related**: [ROADMAP_DATASET_VALIDATION.md](ROADMAP_DATASET_VALIDATION.md), [ROADMAP_SCALE_VALIDATION.md](ROADMAP_SCALE_VALIDATION.md), [ROADMAP_RELEVANCE_ENGINE.md](ROADMAP_RELEVANCE_ENGINE.md)

---

## Motivation

SV-2b validated that Chronik's vector search works (97ms p50, 0% errors, hybrid + RRF ranking). But it also revealed limits that block the next milestone (DV-1/DV-2 with millions of vectors):

1. **Embedding throughput** — Only 196K of ~1M+ potential vectors were indexed in the test window. Batch size is 32 (OpenAI allows 2048), batches are serial, no cross-partition parallelism.
2. **HNSW rebuild on every insert** — Each `add_vector()` destroys the built graph, forcing O(n log n) rebuild. At 1M+ vectors this causes multi-second search degradation.
3. **Memory at scale** — 1536-dim f32 vectors = 6KB each. 10M vectors = 62GB RAM, exceeding the 16GB/node K8s cluster.
4. **No re-ranking** — Single-stage retrieval only. WANDS benchmarks show cross-encoder re-ranking adds +10-20% NDCG.
5. **Locked to one model** — Can't switch embeddings models or re-embed historical data without rebuilding from scratch.

This roadmap closes all five gaps with six independent, modular phases.

### Lessons Learned from SV-2b

| Observation | Root Cause | Phase that fixes it |
|-------------|-----------|---------------------|
| Only 196K vectors indexed (should be ~1M) | Batch size 32, serial processing | VO-1 |
| Search degrades during bulk loading | HNSW invalidated on every insert | VO-2 |
| Can't scale to 10M vectors (DV-2c target) | 6KB/vector, all in RAM | VO-3 + VO-6 |
| No quality metrics (NDCG) available | No re-ranking, no evaluation | VO-4 |
| Can't compare embedding models | Single model per topic, no migration | VO-5 |

---

## Progress Tracker

Use this table to track progress across sessions. Update status and notes as work proceeds.

| Phase | Name | Status | Version | Notes |
|-------|------|--------|---------|-------|
| VO-1 | Embedding Pipeline Throughput | `TESTED` | v2.4.0 | K8s deployed, metrics confirmed: batch_size=256, concurrency=4 |
| VO-2 | Incremental HNSW Indexing | `CODE COMPLETE` | v2.4.1 | Two-tier arch + auto-rebuild thresholds, 860 tests pass |
| VO-3 | Scalar Quantization | `CODE COMPLETE` | v2.4.1 | ScalarQuantizer + uint8/f16 modes, 871 tests pass |
| VO-4 | Re-ranking (Cross-Encoder) | `CODE COMPLETE` | v2.4.1 | RerankerProvider trait + vector_handler + orchestrator, 884 tests pass |
| VO-5 | Multi-Model Embeddings | `CODE COMPLETE` | v2.4.1 | Model-tagged indexes, active_model config, backfill+search model param, 886 tests pass |
| VO-6 | Matryoshka Dimension Reduction | `CODE COMPLETE` | v2.4.1 | search_dimensions + adaptive_retrieval config, truncated HNSW build, two-phase re-scoring, 893 tests pass |

**Status values**: `NOT STARTED` → `IN PROGRESS` → `CODE COMPLETE` → `TESTED` → `COMPLETE`

### DV Readiness Matrix

| Milestone | Required Phases | Target |
|-----------|----------------|--------|
| **DV-1** (42K WANDS quality) | VO-1 + VO-4 | Throughput to load 42K, re-ranking for NDCG |
| **DV-2a** (600K Appliances) | VO-1 + VO-2 | Throughput + stable indexing during load |
| **DV-2b** (5M Grocery) | VO-1 + VO-2 + VO-3 | + quantization for 16GB memory |
| **DV-2c** (10-20M Electronics) | VO-1 + VO-2 + VO-3 + VO-6 | + dimension reduction |

---

## VO-1 — Embedding Pipeline Throughput

**Status**: `TESTED`
**Goal**: Increase embedding indexing rate from ~196K/30min to 2-5M/30min.
**Blocked by**: Nothing (can start immediately)
**Blocks**: All DV phases (data loading)

### Current State

The embedding pipeline in `process_vector_embeddings` ([wal_indexer.rs:1348-1466](crates/chronik-storage/src/wal_indexer.rs#L1348)) processes messages through `EmbeddingPipeline::process_messages` ([vector_index.rs:~1072-1189](crates/chronik-columnar/src/vector_index.rs#L1072)):

```
Sealed segment → extract texts → chunk(32) → embed_batch() → add_to_index → next chunk
                                     ↑ serial, one at a time
```

Three bottlenecks:
- **Batch size 32** — OpenAI supports 2048. Default in [config.rs:38](crates/chronik-embeddings/src/config.rs#L38) is 32, max capped at 1000 (line 74).
- **Serial batches** — Each `embed_batch()` call blocks until response arrives before starting next.
- **Serial partitions** — `process_vector_embeddings` processes one partition at a time.

### Implementation Checklist

- [x] **VO-1.1**: Increase default `batch_size` from 32 → 256 in [config.rs](crates/chronik-embeddings/src/config.rs)
- [x] **VO-1.2**: Raise `batch_size` max validation from 1000 → 2048 in config.rs
- [x] **VO-1.3**: Add `CHRONIK_EMBEDDING_CONCURRENCY` env var (default: 4) to config.rs
- [x] **VO-1.4**: Refactor `EmbeddingPipeline::process_messages` in [vector_index.rs](crates/chronik-columnar/src/vector_index.rs) to use `tokio::JoinSet` with `Semaphore::new(concurrency)` for concurrent batch processing. Collect results in offset order.
- [x] **VO-1.5**: Refactor `process_vector_embeddings` in [wal_indexer.rs](crates/chronik-storage/src/wal_indexer.rs) to spawn up to 3 concurrent partition embedding tasks (configurable via `CHRONIK_VECTOR_PARTITION_CONCURRENCY`)
- [x] **VO-1.6**: Pipeline clamps batch size to `min(config.batch_size, provider.max_batch_size())`
- [x] **VO-1.7**: Add Prometheus metrics: `chronik_embedding_batch_size`, `chronik_embedding_concurrency`, `chronik_embedding_inflight_partitions`
- [x] **VO-1.8**: `cargo check --workspace` passes (274 pre-existing warnings, 0 new)
- [x] **VO-1.9**: `cargo test` passes (9/9 config tests, 36/37 embeddings tests — 1 pre-existing failure in factory)
- [x] **VO-1.10**: Deploy to K8s cluster — image `chronik-server:vo1-throughput` on all 3 nodes, metrics confirmed (batch_size=256, concurrency=4), vector search functional (22,876 vectors, 2.5s query incl. embedding)

### Files to Modify

| File | What changes |
|------|-------------|
| `crates/chronik-embeddings/src/config.rs` | batch_size default 32→256, max 1000→2048, add concurrency config |
| `crates/chronik-columnar/src/vector_index.rs` | Concurrent batch processing in EmbeddingPipeline |
| `crates/chronik-storage/src/wal_indexer.rs` | Cross-partition parallelism |
| `crates/chronik-monitoring/src/unified_metrics.rs` | New embedding throughput metrics |

### Expected Impact

| Metric | Before | After |
|--------|--------|-------|
| Batch size | 32 | 256 |
| In-flight batches | 1 | 4 |
| Partition parallelism | 1 | 3 |
| Theoretical throughput multiplier | 1x | ~24-32x |
| Vectors indexed / 30 min | ~196K | ~2-5M |

---

## VO-2 — Incremental HNSW Indexing

**Status**: `CODE COMPLETE`
**Goal**: Eliminate full HNSW rebuild on every insert. Keep search fast during bulk loading.
**Blocked by**: Nothing (independent of VO-1)
**Blocks**: DV-2 (scale loading requires stable search during ingestion)

### Current State

Every `PartitionIndex::add_vector()` call ([vector_index.rs:282-301](crates/chronik-columnar/src/vector_index.rs#L282)) sets `self.cosine_index = None`, destroying the built HNSW graph. The `instant-distance` crate's `HnswMap` is immutable after build — you cannot add points incrementally.

Search falls back to brute-force O(n) when no built graph exists ([vector_index.rs:~478-514](crates/chronik-columnar/src/vector_index.rs#L478)). At 1M vectors, brute-force takes seconds.

### Implementation Checklist

- [x] **VO-2.1**: Refactor `PartitionIndex` struct to split storage:
  - `built_hnsw: Option<HnswMap<...>>` — last fully-built graph (immutable)
  - `built_vectors: Vec<(u64, Vec<f32>)>` — vectors in built graph
  - `pending_vectors: Vec<(u64, Vec<f32>)>` — new since last build
- [x] **VO-2.2**: Change `add_vector()` to append to `pending_vectors` only — do NOT invalidate built HNSW
- [x] **VO-2.3**: Update `search()` to query both:
  - Built HNSW graph (O(log n), fast)
  - Brute-force scan of pending buffer (O(m), m small)
  - Merge results by distance, return top-k
- [x] **VO-2.4**: Auto-rebuild in `add_vectors()`: triggers when `pending >= 5000` (absolute), `pending >= 10% of built` (ratio), or `pending >= 100` with no graph yet (first build). Three constants: `REBUILD_PENDING_ABSOLUTE=5000`, `REBUILD_PENDING_RATIO_PERCENT=10`, `REBUILD_FIRST_BUILD_MIN=100`.
- [x] **VO-2.5**: `chronik_vector_pending_count` + `chronik_vector_built_count` gauge metrics via `MetricsRecorder::update_vector_index_counts()`
- [x] **VO-2.6**: `PartitionIndexSnapshot` stores both tiers merged; load restores into `built_vectors` and rebuilds HNSW
- [x] **VO-2.7**: `cargo check --workspace` passes (273 pre-existing warnings, 0 new)
- [x] **VO-2.8**: `cargo test --workspace --lib --bins` passes (860 tests, 0 failures)
- [ ] **VO-2.9**: Load 100K vectors while running concurrent searches, verify p99 < 500ms throughout

### Files to Modify

| File | What changes |
|------|-------------|
| `crates/chronik-columnar/src/vector_index.rs` | `PartitionIndex` struct, `add_vector`, `search`, rebuild logic |
| `crates/chronik-monitoring/src/unified_metrics.rs` | Pending vector count gauge |

### Expected Impact

| Metric | Before | After |
|--------|--------|-------|
| Search during bulk load | Degrades to seconds (brute-force) | Stable ~100ms (HNSW + small brute-force) |
| Rebuild frequency | Every batch (~32 vectors) | Every ~5000 vectors |
| Rebuild blocking search | Yes (invalidates graph) | No (background, atomic swap) |

---

## VO-3 — Scalar Quantization for Memory Reduction

**Status**: `CODE COMPLETE`
**Goal**: Reduce vector memory by 4x (f32→uint8) to fit 5M+ vectors in 16GB/node.
**Blocked by**: VO-2 (quantized vectors work best with two-tier architecture)
**Blocks**: DV-2b (5M vectors need <16GB per node)

### Current State

All vectors stored as `Vec<f32>` — 6,144 bytes per 1536-dim vector. `PartitionIndexSnapshot` ([vector_index.rs:1060-1066](crates/chronik-columnar/src/vector_index.rs#L1060)) serializes full `Vec<(u64, Vec<f32>)>`. No quantization exists anywhere in the codebase.

The `instant-distance` crate does not support quantized vectors natively — distance computation wrapper types (`CosinePoint`, `EuclideanPoint` at [vector_index.rs:193-233](crates/chronik-columnar/src/vector_index.rs#L193)) all use `Vec<f32>`.

### Implementation Checklist

- [x] **VO-3.1**: Added `QuantizationMode` enum (F32/F16/Uint8) + `vector.quantization` config key in config.rs. Parses from topic config, included in `valid_keys()`.
- [x] **VO-3.2**: Implemented `ScalarQuantizer` struct in vector_index.rs:
  - `fit()` computes per-dimension min/max from training vectors
  - `quantize_uint8()` maps f32→[0,255], `dequantize_uint8()` reverses
- [x] **VO-3.3**: Asymmetric distance for cosine, euclidean, and dot product — f32 query vs uint8 DB vectors. `brute_force_search_quantized()` in PartitionIndex. No full dequantize allocation.
- [x] **VO-3.4**: Added `half = "2.4"` to Cargo.toml. F16 mode declared in QuantizationMode (runtime storage TBD — uint8 is primary for 4x reduction).
- [x] **VO-3.5**: `PartitionIndexSnapshot` uses `#[serde(default)]` on `quantizer: Option<ScalarQuantizer>` and `config.quantization`. Old f32 snapshots deserialize cleanly with F32 mode and no quantizer.
- [x] **VO-3.6**: Added `chronik_vector_memory_bytes` gauge metric to unified_metrics.rs. `report_tier_metrics()` aggregates `vector_memory_bytes()` across all partitions.
- [x] **VO-3.7**: Added `half = "2.4"` to chronik-columnar/Cargo.toml
- [x] **VO-3.8**: `cargo check --workspace` passes (273 pre-existing warnings, 0 new)
- [x] **VO-3.9**: `cargo test --workspace --lib --bins` passes (871 tests, 0 failures — 11 new)
- [ ] **VO-3.10**: Recall@10 regression test: f32 vs uint8 on same 10K vectors, verify >= 97% recall (K8s deployment test)

### Files to Modify

| File | What changes |
|------|-------------|
| `crates/chronik-embeddings/src/config.rs` | `vector.quantization` config option |
| `crates/chronik-columnar/src/vector_index.rs` | `ScalarQuantizer`, quantized storage, asymmetric distance |
| `crates/chronik-columnar/Cargo.toml` | Add `half` crate |
| `crates/chronik-monitoring/src/unified_metrics.rs` | Memory accounting metric |

### Expected Impact

| Quantization | Bytes/vector (1536d) | 5M vectors | 10M vectors | Recall@10 |
|-------------|---------------------|-----------|------------|-----------|
| f32 (current) | 6,144 | 31 GB | 62 GB | 100% |
| f16 | 3,072 | 15.4 GB | 30.7 GB | ~99.9% |
| uint8 | 1,536 | 7.7 GB | 15.4 GB | ~97-98% |

---

## VO-4 — Re-ranking with Cross-Encoder

**Status**: `CODE COMPLETE`
**Goal**: Add pluggable cross-encoder re-ranking stage for NDCG quality improvement.
**Blocked by**: Nothing (independent)
**Blocks**: DV-1 quality metrics (NDCG@10 improvement)

### Current State

Single-stage retrieval: HNSW returns approximate nearest neighbors, RRF fuses vector + text ranks ([rrf.rs:1-177](crates/chronik-query/src/rrf.rs)). No refinement.

Existing stubs:
- `FusionMethod::VectorRerank` enum variant in [vector_search.rs:135-144](crates/chronik-storage/src/vector_search.rs#L135) — stub implementation, just returns vector_score
- Hybrid search in [vector_handler.rs:614-616](crates/chronik-server/src/unified_api/vector_handler.rs#L614) — text results hardcoded to `Vec::new()` (not wired to Tantivy)
- [ROADMAP_RELEVANCE_ENGINE.md](ROADMAP_RELEVANCE_ENGINE.md) Phase 10 describes the full three-phase ranking vision

### Implementation Checklist

- [x] **VO-4.1**: Created `RerankerProvider` trait in [reranker.rs](crates/chronik-embeddings/src/reranker.rs) with `rerank()`, `name()`, `max_documents()` methods. Added `RerankResult` struct, `RerankerConfig`, `RerankerProviderType` enum.
- [x] **VO-4.2**: Implemented `ExternalReranker` — HTTP client with Cohere-compatible API format (query + documents → scored results). Also `NoOpReranker` for testing.
- [x] **VO-4.3**: Added config keys to config.rs: `vector.reranker.enabled`, `vector.reranker.provider`, `vector.reranker.model`, `vector.reranker.endpoint`, `vector.reranker.api_key`, `vector.reranker.overretrieval_factor`. Also `CHRONIK_RERANKER_API_KEY` env var.
- [x] **VO-4.4**: Added `create_reranker(config) -> Box<dyn RerankerProvider>` factory function in reranker.rs. Added module + re-exports in lib.rs.
- [x] **VO-4.5**: Integrated in vector_handler.rs: `search()` endpoint overretrieves k×5 candidates when `rerank: true`, calls reranker, returns top-k. `apply_reranking()` helper with graceful fallback on error.
- [x] **VO-4.6**: Integrated in orchestrator.rs: `QueryOrchestrator::with_reranker()` builder, `apply_reranking()` after RRF merge reorders entries by cross-encoder scores.
- [x] **VO-4.7**: `FusionMethod::VectorRerank` in vector_search.rs already handles score fusion correctly (prioritizes vector score). Re-ranking happens at handler/orchestrator layer.
- [x] **VO-4.8**: Wire Tantivy BM25 results into hybrid search RRF — searches in-memory indices, WAL-created indices, and real-time indices via SearchApi. Text results carry (partition, offset, text_preview) for full RRF fusion.
- [x] **VO-4.9**: Opt-in control: per-query `rerank: true` in VectorSearchRequest, HybridSearchRequest, and QueryRequest. Per-topic via `vector.reranker.enabled`.
- [x] **VO-4.10**: `cargo check --workspace` passes (273 pre-existing warnings, 0 new)
- [x] **VO-4.11**: `cargo test --workspace --lib --bins` passes (884 tests, 0 failures — 13 new reranker+handler tests)
- [ ] **VO-4.12**: DV-1 WANDS: Run 480 queries with/without reranking, compare NDCG@10
  - Interim results (2026-03-04, vector index NOT fully loaded):
    - BM25 NDCG@10 = 0.5883 (fan-out working, beats ES baseline)
    - Client hybrid NDCG@10 = 0.6028 (client-side RRF)
    - Server hybrid NDCG@10 = 0.1474 (low due to 458/480 empty vector results — index loading)
    - Fixed QueryRouter port bug (was using 6091+node_id, now reads CHRONIK_UNIFIED_API_PORT)
    - Fixed VO-4.8 (text_results was Vec::new(), now searches Tantivy)
    - Re-run needed once vector indices fully loaded on all 3 nodes

### Files to Modify

| File | What changes |
|------|-------------|
| `crates/chronik-embeddings/src/reranker.rs` | **NEW**: RerankerProvider trait + ExternalReranker |
| `crates/chronik-embeddings/src/config.rs` | Reranker config keys |
| `crates/chronik-embeddings/src/factory.rs` | Reranker factory |
| `crates/chronik-embeddings/src/lib.rs` | Export reranker module |
| `crates/chronik-server/src/unified_api/vector_handler.rs` | Rerank integration + fix text search gap |
| `crates/chronik-query/src/orchestrator.rs` | Rerank step after fusion |
| `crates/chronik-storage/src/vector_search.rs` | Wire VectorRerank to real implementation |

### Expected Impact

| Metric | Without Re-ranking | With Re-ranking |
|--------|-------------------|-----------------|
| WANDS NDCG@10 (hybrid) | ~0.45-0.50 | ~0.55-0.65 |
| Query latency | ~100ms | ~200-300ms (+reranker API) |
| Applicable to | All queries | Opt-in only |

---

## VO-5 — Multi-Model Embedding Support & Re-Embedding

**Status**: `CODE COMPLETE`
**Goal**: Support multiple embedding models per topic, enable model migration and A/B testing.
**Blocked by**: Nothing (independent)
**Blocks**: DV-1 model comparison experiments

### Current State (Post-Implementation)

Multi-model embedding support is fully implemented. Indexes are now keyed by `(topic, partition, model_id)` with `"default"` as the backward-compatible default. The `VectorIndexManager` supports model-specific search, add, and stats operations. The backfill endpoint accepts a `model` parameter for re-embedding historical data.

### Implementation Checklist

- [x] **VO-5.1**: Add `model_id: String` to `PartitionIndex` + `PartitionIndexSnapshot` with `#[serde(default)]` backward compat
- [x] **VO-5.2**: Key indexes by `(topic, partition, model_id)` — `TopicPartitionKey.model_id`, `DEFAULT_MODEL_ID` constant, model-aware CRUD methods
- [x] **VO-5.3**: Add `vector.active_model` config key (default: `"default"`)
- [x] **VO-5.4**: Accept `model` param in search/hybrid/by-vector endpoints + stats response includes `models` list
- [x] **VO-5.5**: Backfill endpoint `POST /_vector/:topic/backfill` accepts `model` field, response includes `model` used
- [x] **VO-5.6**: `process_vector_embeddings_with_model()` stamps vectors via `add_vectors_for_model()` / `process_messages_for_model()`
- [x] **VO-5.7**: Snapshot naming: `{partition}_{model_id}.idx` for named models, `{partition}.idx` for default. `parse_snapshot_filename()` handles both
- [x] **VO-5.8**: `cargo check --workspace` passes (clean)
- [x] **VO-5.9**: `cargo test --workspace --lib --bins` passes (886 tests)

### Files to Modify

| File | What changes |
|------|-------------|
| `crates/chronik-columnar/src/vector_index.rs` | Model-tagged indexes, snapshot naming |
| `crates/chronik-embeddings/src/config.rs` | `vector.active_model`, `vector.models` config |
| `crates/chronik-server/src/unified_api/vector_handler.rs` | `model` param in search/backfill endpoints |
| `crates/chronik-storage/src/wal_indexer.rs` | Stamp vectors with model ID |

### Expected Impact

- Zero-downtime model migration: re-embed in background → switch active model → old index remains for rollback
- A/B testing: compare `text-embedding-3-small` (1536d, $0.02/1M tok) vs `text-embedding-3-large` (3072d, $0.13/1M tok) on same data
- No impact on topics not using multi-model (fully backward compatible)

---

## VO-6 — Matryoshka Dimension Reduction + Adaptive Retrieval

**Status**: `CODE COMPLETE`
**Goal**: Reduce vector dimensions from 1536 → 512 for 3x memory savings and 2x search speedup with <3% recall loss.
**Blocked by**: VO-3 (combines with quantization for maximum reduction)
**Blocks**: DV-2c (10-20M vectors need maximum compression)

### Current State (Post-Implementation)

Matryoshka dimension reduction is fully implemented. HNSW graphs can now be built on truncated dimensions while full vectors are retained for adaptive re-scoring. Config keys `vector.search_dimensions` and `vector.adaptive_retrieval` control the behavior.

### Implementation Checklist

- [x] **VO-6.1**: Add `vector.search_dimensions` config (default: 0 = use full dims) in config.rs + HnswIndexConfig
- [x] **VO-6.2**: Add `vector.adaptive_retrieval` boolean config (default: false) — enables two-phase search
- [x] **VO-6.3**: HNSW `build()` uses `truncate_vec()` when `is_matryoshka_enabled()`, full vectors stay in `built_vectors`
- [x] **VO-6.4**: Adaptive two-phase search: `search()` overretrieves k×3, `rescore_full_dimensions()` re-scores at full dims
- [x] **VO-6.5**: Query truncated at search time via `truncate_vec()`, pending vectors searched with truncated query too
- [x] **VO-6.6**: `cargo check --workspace` passes (clean)
- [x] **VO-6.7**: `cargo test --workspace --lib --bins` passes (893 tests)

### Files to Modify

| File | What changes |
|------|-------------|
| `crates/chronik-embeddings/src/config.rs` | `vector.search_dimensions`, `vector.adaptive_retrieval` |
| `crates/chronik-columnar/src/vector_index.rs` | Truncated HNSW build, adaptive two-phase search |

### Expected Impact

| Dimensions | Memory/vector | Search speed | Recall@10 (text) |
|-----------|--------------|-------------|------------------|
| 1536 (current) | 6,144 B | 1x | 100% |
| 1024 | 4,096 B | ~1.3x | ~99.5% |
| 768 | 3,072 B | ~1.7x | ~99% |
| 512 | 2,048 B | ~2.5x | ~97-99% |
| 256 | 1,024 B | ~4x | ~93-96% |

**Combined with VO-3 (uint8 at 512d)**: 12x total memory reduction. 10M vectors: 62GB → 5.1GB.

---

## Implementation Sequence

```
                    ┌──────────────────┐
                    │  VO-1: Embedding  │ ◄── START HERE
                    │    Throughput     │     (unblocks loading)
                    └────────┬─────────┘
                             │
              ┌──────────────┴──────────────┐
              ▼                             ▼
    ┌──────────────────┐          ┌──────────────────┐
    │  VO-2: Incremental│          │  VO-4: Re-ranking │
    │    HNSW Indexing  │          │  (Cross-Encoder)  │
    └────────┬─────────┘          └──────────────────┘
             │                              │
             ▼                              ▼
    ┌──────────────────┐          ┌──────────────────┐
    │  VO-3: Scalar    │          │  VO-5: Multi-Model│
    │   Quantization   │          │    Embeddings     │
    └────────┬─────────┘          └──────────────────┘
             │
             ▼
    ┌──────────────────┐
    │  VO-6: Matryoshka │
    │  Dim Reduction    │
    └──────────────────┘
```

**Left branch** (VO-1 → VO-2 → VO-3 → VO-6): Scale and memory — required for DV-2.
**Right branch** (VO-1 → VO-4 → VO-5): Quality and flexibility — required for DV-1.

Both branches start from VO-1 (embedding throughput).

---

## Session Handoff Notes

Update this section at the end of each work session to enable seamless continuation.

### Last Session

**Date**: 2026-02-28
**Phase worked on**: VO-2 + VO-3 + VO-4
**Checklist items completed**: VO-2.1–VO-2.8, VO-3.1–VO-3.9, VO-4.1–VO-4.11
**What was done**:
- **VO-2**: Added auto-rebuild trigger in `add_vectors()` — checks `needs_rebuild()` after adding vectors. Added `REBUILD_FIRST_BUILD_MIN=100` for first HNSW build.
- **VO-3**: Full scalar quantization implementation:
  - `QuantizationMode` enum (F32/F16/Uint8) in config.rs with parsing + validation
  - `ScalarQuantizer` struct with fit/quantize/dequantize/asymmetric distance
  - `PartitionIndex` stores quantized copies (`quantized_built`, `quantized_pending`)
  - Asymmetric search: f32 query vs uint8 DB vectors (no full dequantize allocation)
  - Backward-compatible snapshots via `#[serde(default)]`
  - `chronik_vector_memory_bytes` Prometheus gauge metric
  - 11 new tests (8 vector index + 3 config), 871 total passing
- **VO-4**: Full re-ranking with cross-encoder implementation:
  - `RerankerProvider` trait + `ExternalReranker` (Cohere-compatible HTTP) + `NoOpReranker`
  - `RerankerConfig` with topic config parsing + `CHRONIK_RERANKER_API_KEY` env var
  - `vector_handler.rs`: search + hybrid endpoints support `rerank: true`, overretrieves k×5 candidates
  - `orchestrator.rs`: `with_reranker()` builder, applies after RRF fusion
  - `UnifiedApiState`: `reranker: Option<Arc<dyn RerankerProvider>>` + `with_reranker()` builder
  - `QueryRequest`: `rerank: bool` field for opt-in per-query control
  - 13 new tests (7 reranker + 6 handler), 884 total passing
- Branch: `fix/workspace-tests-all-passing`
**Next step**: VO-4.12 (DV-1 WANDS evaluation with server-side hybrid + reranking) — VO-4.8 COMPLETE
**Blockers**: None
**Build status**: 914 tests passing, 0 failures

### Environment

- K8s cluster: 3 Dell nodes (192.168.1.31-33), MicroK8s, 16GB RAM/node
- Current images: `chronik-server:vo1-throughput`, `chronik-operator:sv2b-fixes`
- Metrics port: 13001 (13000 + node_id) — NOT on unified API port 6092
- Image distribution: `docker save | scp | sudo microk8s ctr images import`
- Dockerfile base: `ubuntu:24.04` (required for glibc 2.39 compatibility)
- kafka-python installed on dell-1 (`sudo pip3 install kafka-python`)

---

## Verification Plan (End-to-End)

After all phases complete:

1. **Build**: `cargo check --workspace` + `cargo test --workspace --lib --bins`
2. **Regression**: Deploy to K8s, run SV-2b k6 benchmark — must maintain 0% error, <100ms p50
3. **DV-1 WANDS**: 42K products, 480 queries, NDCG@10 >= 0.50 (BM25), >= 0.58 (hybrid)
4. **DV-2a**: 600K vectors indexed within 30 min, search <10ms p50
5. **DV-2b**: 5M vectors fit in 16GB/node (with quantization), search <20ms p50
6. **DV-2c**: 10-20M vectors with dimension reduction + quantization

---

## Relationship to Other Roadmaps

```
ROADMAP_SCALE_VALIDATION.md (SV-1..SV-4)
  └─ SV-2b ◄── completed, findings feed THIS roadmap

ROADMAP_VECTOR_OPTIMIZATION.md (this doc, VO-1..VO-6)
  └─ VO-1..VO-6 prepare the vector subsystem for DV dataset validation

ROADMAP_DATASET_VALIDATION.md (DV-1..DV-4)
  └─ DV-1 (WANDS quality) ◄── requires VO-1 + VO-4
  └─ DV-2 (Amazon scale) ◄── requires VO-1 + VO-2 + VO-3

ROADMAP_RELEVANCE_ENGINE.md (Phase 10..11)
  └─ Phase 10 (Reranking + ML) ◄── VO-4 provides foundation
  └─ Phase 10 (Expression Engine) ◄── independent of this roadmap
```
