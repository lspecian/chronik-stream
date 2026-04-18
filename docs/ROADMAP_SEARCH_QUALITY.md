# Search Quality Roadmap

**Goal**: Improve BM25 NDCG@10 from 0.5927 to 0.70+ on the WANDS benchmark.
**Baseline**: Chronik BM25 = 0.5927 | Chronik Hybrid (RRF) = 0.6095
**Benchmark**: [WANDS dataset](https://github.com/wayfair/WANDS) (42,994 products, 480 queries, 233K labels)
**Evaluation script**: `tests/k8s-perf/102-wands-quality-configmap.yaml`
**Relevance grades**: Exact=3, Partial=1, Irrelevant=0

### Measurement Discipline

Every phase must record **both relevance and system metrics**. Chronik's value is not just NDCG — it is search quality on a live event-native substrate. Improving relevance while degrading freshness or latency is a regression.

**Relevance metrics** (per phase):
- NDCG@10, Precision@10, MRR, Recall@100
- Per-query deltas: improved / degraded / unchanged

**System metrics** (per phase):
- Query latency: p50 / p95 / p99
- Indexing throughput: msgs/sec with analyzer enabled
- Freshness lag: T0 (event written) → T1 (BM25 searchable) → T2 (vector searchable)
- Index size on disk (Tantivy segments)
- Memory usage delta

---

## Freshness Benchmark

**Chronik's differentiator**: search over a live event stream, not a static corpus.

Define freshness as the time between event arrival and query availability:

| Metric | Definition | Cold-only baseline | With hot path (default) |
|--------|-----------|----------|--------|
| T0→T1 (BM25) | Event written → searchable by full-text | ~30-60s (WalIndexer tick) | **p50 201ms / p99 507ms** (measured 2026-04-17) |
| T0→T2 (Vector) | Event written → searchable by ANN | ~30-90s (cold embed + HNSW rebuild) | **p50 18ms / p99 31ms** with mock embedder; ~70-150ms with real local GPU; ~400-800ms with OpenAI |
| T0→T3 (Hybrid) | Event written → available in hybrid search | max(T1,T2) | same as T1/T2 bounds — hybrid handler merges hot hits into RRF fusion |

Hot paths are enabled by default. See [HOT_PATH_GUIDE.md](./HOT_PATH_GUIDE.md) for ops and [ROADMAP_HOT_PATH.md](./ROADMAP_HOT_PATH.md) for design history.

**Measurement method** (on-demand):
- [tests/bench_hot_text_freshness.sh](../tests/bench_hot_text_freshness.sh) — produces markers, polls `/_search`, reports min/p50/p95/p99 across N probes
- [tests/bench_hot_vector.sh](../tests/bench_hot_vector.sh) — same for `/_vector/:topic/search`
- Cold-only baseline: run either script with `CHRONIK_HOT_TEXT_ENABLED=false` or `CHRONIK_HOT_VECTOR_ENABLED=false`

**Freshness must be measured after each phase** to ensure analyzer/indexing changes don't increase lag.

---

## Current State Analysis

The search pipeline is minimal:
- Tantivy default tokenizer (whitespace + lowercase only)
- No stemming — "running" does not match "run"
- No stop word removal — "the", "a", "is" dilute term weights
- No field-specific boosting — all fields treated equally via `_all`
- No phrase/proximity matching — multi-word queries are OR'd individual terms
- Default BM25 parameters (k1=0.9, b=0.75), never tuned
- `_all` field computed at query time, not pre-built

### Key Lessons (from research)

1. **Reranking alone won't save bad retrieval.** The Vectorian article demonstrates that reranking NDCG gains (+0.054) vanish end-to-end because rerankers cannot recover documents that retrieval never surfaced. Always measure end-to-end.

2. **RRF is hard to beat with simple models.** A learned linear ranker on a small corpus with limited training signal cannot find structure beyond what a well-chosen heuristic already captures. Our hybrid RRF at 0.6095 may be near-optimal for our current retrieval quality.

3. **Feature design > model complexity.** Query-aware features (query length, term rarity, semantic-keyword divergence) matter more than the ranking model itself.

4. **Cross-encoders deliver +5-28% NDCG in benchmarks** but require a model serving infrastructure and add 2-3.5x latency. Real-world production gains are 5-10% end-to-end.

5. **ColBERT late interaction** achieves MRR@10 ~0.343 on MS MARCO (vs ~0.18 for BM25) with sub-200ms CPU latency via PLAID indexing. This is a realistic path to 0.70+ without GPUs.

---

## Phase 1: Text Analysis Pipeline

**Expected impact**: NDCG@10 0.5927 -> 0.62-0.64
**Effort**: 2-3 days
**Risk**: Low — well-understood techniques, easy to A/B test

### SQ-1.1: Register custom Tantivy analyzer
- [x] Build `TextAnalyzer` with: SimpleTokenizer -> LowerCaser -> StopWordFilter -> Stemmer
- [x] Parameterize language (default: `Language::English`) — read from topic config or env var
- [x] Register as `"default"` override in Tantivy index creation (all 14 sites)
- [x] Apply to `_value`, `_key`, and `_json_content` fields (automatic via default override)
- [x] Ensure same analyzer used at both index time and query time

**Implementation**: `crates/chronik-storage/src/text_analysis.rs`
**Registered in**: `tantivy_segment.rs`, `realtime_indexer.rs`, `handlers.rs`, `indexer.rs`, `api.rs`, `search.rs`, `index.rs`, `chronik_segment.rs`, `query_handler.rs`, `vector_handler.rs`

### SQ-1.2: Stop word removal
- [x] Use standard English stop word list (~127 words)
- [x] Verify stop words removed at index time AND query time (same analyzer registered both sides)
- [x] Test that single-word stop-word-only queries still return results (fallback to match-all)
- [ ] **Test three modes on WANDS** (per feedback):
  - Mode A: no stop-word removal (baseline)
  - Mode B: standard stop-word removal (current implementation)
  - Mode C: query-time selective fallback (current: stop-word-only → match-all)
- [ ] Compare per-query deltas across modes — watch for fragile short queries ("a frame", "the table")

### SQ-1.3: Snowball stemming (English)
- [x] Add `Stemmer::new(Language::English)` to analyzer pipeline
- [ ] Verify: "running shoes" matches documents containing "run", "shoe"
- [ ] Verify: "batteries" matches "battery"
- [ ] Watch for over-stemming (e.g., "universal" -> "univers" matching "university")

### SQ-1.4: Run WANDS evaluation
- [x] Re-run DV-1 WANDS benchmark with new analyzer
- [x] Record BM25 NDCG@10, Precision@10, MRR
- [ ] Record Hybrid NDCG@10
- [ ] Compare per-query deltas: which queries improved, which degraded
- [x] Document results in this file

**Phase 1 Results** (2026-03-10, local single-node eval, apples-to-apples):
```
Relevance:            Baseline (local)   Phase 1 (local)   Delta
  BM25  NDCG@10:      0.6209             0.6327            +1.9%
  Precision@10:        0.6957             0.7015            +0.8%
  MRR:                 0.5523             0.5538            +0.3%
  Empty results:       1/480              1/480
  Errors:              0                  0

System:               Baseline           Phase 1
  Query p50/p95/p99:  3.1/3.8/4.2 ms     3.0/3.6/4.1 ms
  Indexing throughput: 2,274 msgs/sec     2,281 msgs/sec
  Total eval time:     1.5s               1.5s
```

**Cluster Results** (2026-03-10, 3-node RF=3, dedup fix + stemming):
```
Relevance:            Cluster (old)   Cluster (fixed)   Delta
  BM25  NDCG@10:      0.5927          0.6176            +4.2%
  Precision@10:        —               0.6944
  MRR:                 —               0.5464

System:
  Query p50/p95/p99:  —               95.8/358.7/488.6 ms
```

**Root cause of cluster-vs-local gap**: `merge_search_responses()` had no deduplication.
With RF=3 on 3 nodes, same document appeared 3x in merged results, crowding out unique hits.
Fixed by adding HashMap dedup by `(_index, _id)` — same pattern as `merge_vector_responses()`.
Remaining ~2.4% gap is per-partition BM25 IDF fragmentation (known property of sharded search).

**Status**: Phase 1 + dedup fix delivers +4.2% NDCG on cluster (0.5927 → 0.6176).

---

## Phase 2: Field Boosting and Query Construction

**Expected impact**: +1-3% NDCG on top of Phase 1
**Effort**: 2-3 days
**Risk**: Medium — requires understanding WANDS document structure
**Depends on**: Phase 1

### SQ-2.1: Phrase proximity boosting _(moved before BM25 tuning — higher leverage)_
- [ ] Detect multi-word queries (2+ terms after stop word removal)
- [ ] Combine term query with phrase query: `BooleanQuery([Should(terms), Should(phrase * boost)])`
- [ ] Apply phrase boost factor (1.5-3.0x)
- [ ] Test: "leather sofa" should rank documents with adjacent "leather sofa" higher than documents with "leather" and "sofa" far apart

**File**: `crates/chronik-search/src/handlers.rs` (multi-word logic, ~line 484-507)

### SQ-2.2: Field-specific boosting
- [ ] Parse JSON message structure to identify field roles
- [ ] Apply boost weights in `build_tantivy_query()`:
  - Product name / title: boost 5.0-10.0x
  - Category / hierarchy: boost 2.0-3.0x
  - Description: boost 1.0x (baseline)
  - Other fields: boost 0.5x
- [ ] Make boost weights configurable via topic config or env vars
- [ ] Sweep boost ratios on WANDS validation set

**File**: `crates/chronik-search/src/handlers.rs` (build_tantivy_query, ~line 443-510)

### SQ-2.3: Distributed IDF coordination (`dfs_query_then_fetch`)
- [ ] Implement two-phase query mode for multi-shard clusters:
  - **Phase 1 (DFS)**: Fan out to all shards, collect per-term document frequencies and total doc counts
  - **Phase 2 (Query)**: Re-query with merged global IDF stats so all shards score on the same scale
- [ ] Add `search_type` parameter to `/_search` endpoint: `query_then_fetch` (default, current behavior) or `dfs_query_then_fetch`
- [ ] In `query_router.rs`: new `dfs_search()` method that does two round-trips
- [ ] In `handlers.rs` / Tantivy query: accept external IDF overrides (may require custom `Scorer` or pre-computed boost factors)
- [ ] Fallback: if cluster has only 1 shard, skip DFS phase (no benefit)
- [ ] Measure NDCG@10 on WANDS with 3-shard cluster — should close the ~2.4% gap vs single-node

**Why**: With N shards, each computes IDF from its local subset (~1/N of corpus). Rare terms on one shard get inflated scores vs the same term on another shard, producing suboptimal merged rankings. This is the same problem Elasticsearch solves with `dfs_query_then_fetch`.

**Files**: `crates/chronik-server/src/unified_api/query_router.rs`, `crates/chronik-search/src/handlers.rs`

### SQ-2.4: BM25 parameter tuning (k1, b) _(optional — do last, likely near-optimal already)_
- [ ] Investigate Tantivy 0.24 API for custom BM25 parameters
- [ ] If not exposed: evaluate if custom `Scorer` wrapper is feasible
- [ ] If exposed: grid search k1 in [0.5, 0.7, 0.9, 1.0, 1.2, 1.5] and b in [0.5, 0.6, 0.75, 0.85]
- [ ] Use WANDS queries for evaluation, record NDCG@10 for each (k1, b) pair
- [ ] Note: Tantivy defaults (k1=0.9, b=0.75) may already be near-optimal

### SQ-2.5: Run WANDS evaluation
- [ ] Re-run with phrase matching + field boosting (+ BM25 tuning if done)
- [ ] Record deltas vs Phase 1 baseline
- [ ] Document results

**Phase 2 Results**: _Not yet run_
```
Relevance:
  BM25  NDCG@10:  _____ (was Phase 1 result)
  Hybrid NDCG@10: _____ (was Phase 1 result)
  Recall@100:     _____
  Best field boost ratio: title=__ desc=__ cat=__
  Best (k1, b): (__, __) or "default — no improvement"

System:
  Query p50/p95/p99: ___/___/___ ms
  Freshness T0→T1:   _____ s (median)
```

---

## Phase 3: Query Expansion _(conditional — pursue only after Phase 2 diagnostics)_

**Expected impact**: +1-3% NDCG
**Effort**: 3-5 days
**Risk**: Medium-High — expansion can introduce noise and hurt precision on short queries
**Depends on**: Phase 2 complete with per-query error analysis
**Decision gate**: Only pursue if Phase 2 error analysis shows retrieval failures (relevant docs not in top-100) that expansion could fix. If most errors are ranking errors (relevant docs retrieved but poorly ranked), expansion won't help — focus on Phase 4 instead.

### SQ-3.1: Pseudo-Relevance Feedback (PRF / RM3-style)
- [ ] Retrieve top-10 results with BM25
- [ ] Extract top-N terms by TF-IDF from result documents (excluding stop words)
- [ ] Append top-3 expansion terms to original query with reduced weight (0.3x)
- [ ] Re-run BM25 with expanded query
- [ ] This is a classical IR technique — no LLM needed

### SQ-3.2: Synonym expansion (static)
- [ ] Build domain-specific synonym map from WANDS product data:
  - Extract co-occurrence patterns from product names + categories
  - Examples: sofa<->couch, lamp<->light, rug<->carpet, dresser<->chest
- [ ] At query time: expand query terms using synonym map
- [ ] Weight original terms 1.0x, synonyms 0.5x
- [ ] Make synonym map configurable per topic (or per-index)

### SQ-3.3: LLM-based query expansion (optional, requires API)
- [ ] Use embedding provider (already have OpenAI integration) to generate query expansions
- [ ] Prompt: "Given this product search query: '{query}', list 3-5 alternative search terms"
- [ ] Cache expansions (queries repeat in production)
- [ ] Only viable if latency budget allows (~200-500ms extra)

### SQ-3.4: Run WANDS evaluation
- [ ] Compare PRF vs synonym vs LLM expansion
- [ ] Record which approach helps most, which hurts
- [ ] **Measure P@10 alongside NDCG@10** — expansion often trades precision for recall

**Phase 3 Results**: _Not yet run_
```
Relevance:
  BM25+PRF      NDCG@10: _____   P@10: _____
  BM25+Synonyms NDCG@10: _____   P@10: _____
  BM25+LLM      NDCG@10: _____   P@10: _____
  Best approach: _____

System:
  Query p50/p95/p99: ___/___/___ ms (expansion adds latency)
  Freshness T0→T1:   _____ s (unchanged expected)
```

---

## Phase 4: Cross-Encoder Reranking

**Expected impact**: +3-8% NDCG end-to-end (if retrieval is good enough from Phases 1-2)
**Effort**: 5-7 days
**Risk**: High — requires model serving, adds latency, may not help if retrieval is weak
**Depends on**: Phases 1-2 (Phase 3 optional)
**Relates to**: ROADMAP_RELEVANCE_ENGINE.md Phase 10

### Important caveat

From the Vectorian article: reranking metrics overstate pipeline performance. A reranker that shows +5% NDCG in isolation may show +0-2% end-to-end because it can only reorder what retrieval surfaces. **Always measure end-to-end.**

### SQ-4.1: Model selection
- [ ] Evaluate lightweight cross-encoders for CPU inference:
  - `cross-encoder/ms-marco-MiniLM-L-6-v2` (~22M params, ~10ms/pair on CPU)
  - `cross-encoder/ms-marco-MiniLM-L-12-v2` (~33M params, ~20ms/pair on CPU)
  - `BAAI/bge-reranker-v2-m3` (multilingual, ~100M params)
- [ ] Measure inference latency per query-document pair
- [ ] Target: rerank top-50 in <500ms total (50 pairs * 10ms)

### SQ-4.2: Integration with search handler
- [ ] Retrieve top-50 with BM25 (over-fetch)
- [ ] Score each (query, document_text) pair with cross-encoder
- [ ] Re-sort by cross-encoder score
- [ ] Return top-10
- [ ] Make reranking optional per request: `{"rerank": true}` or `{"rerank_model": "ms-marco-MiniLM"}`

**File**: `crates/chronik-server/src/unified_api/search_handler.rs`
**Existing infra**: `crates/chronik-embeddings/src/reranker.rs` (VO-4 has reranker trait already)

### SQ-4.3: Model serving
- [ ] Option A: ONNX Runtime in-process (preferred — no external dependency)
  - Load ONNX model at startup
  - Run inference on CPU
  - Candle framework may also work (already a dependency)
- [ ] Option B: External model server (embedding-adapter pattern, already deployed on k8s)
  - HTTP call to sidecar
  - Higher latency but simpler model management

### SQ-4.4: Run WANDS evaluation (end-to-end)
- [ ] Measure NDCG@10 for BM25 + rerank (NOT reranking in isolation)
- [ ] Measure latency impact: p50 and p99 with reranking enabled
- [ ] Compare: how many queries improved vs degraded vs unchanged
- [ ] If improvement < 2% end-to-end, reconsider whether the complexity is justified

**Phase 4 Results**: _Not yet run_
```
Relevance:
  BM25+rerank    NDCG@10: _____ (end-to-end, NOT reranking-only)
  Hybrid+rerank  NDCG@10: _____

System:
  Query p50:   _____ms (was ~3ms BM25 only)
  Query p99:   _____ms
  Model:       _____
  Freshness:   unchanged (reranking is query-time only)
```

---

## Phase 5: Neural Sparse Retrieval (Stretch)

**Expected impact**: +5-15% NDCG (replaces BM25 first stage, not just reranking)
**Effort**: 2-3 weeks
**Risk**: Very High — significant architecture change, requires training or fine-tuning
**Depends on**: Phases 1-4 evaluated
**Decision gate**: Only pursue if Phases 1-4 plateau below 0.68

This is the path if Phases 1-4 are not enough. Neural sparse models like SPLADE and ColBERT fundamentally change retrieval, not just ranking.

### SQ-5.1: Evaluate SPLADE-style term expansion
- [ ] SPLADE uses a transformer to assign importance weights to vocabulary terms
- [ ] Output is a sparse vector compatible with standard inverted indexes
- [ ] Can be integrated into Tantivy's existing index structure
- [ ] Tradeoff: requires a model at indexing time (not just query time)
- [ ] Out-of-domain accuracy may fall below BM25 — must test on WANDS specifically

### SQ-5.2: Evaluate ColBERT late interaction
- [ ] Pre-compute per-token embeddings for all documents
- [ ] At query time: compute query token embeddings, score via MaxSim
- [ ] PLAID indexing enables sub-200ms CPU retrieval
- [ ] MRR@10 ~0.343 on MS MARCO vs ~0.18 for BM25 — significant uplift
- [ ] Storage: ~50-100 bytes per token (compression available)
- [ ] Libraries: PyLate, ColBERT official, or custom Rust implementation

### SQ-5.3: Decision
- [ ] If SPLADE or ColBERT shows >0.70 NDCG@10 on WANDS: adopt
- [ ] If not: accept current quality level, focus on other product areas
- [ ] Document decision and reasoning

---

## Summary: Expected Trajectory

| After | Expected NDCG@10 | Confidence |
|-------|-------------------|------------|
| Baseline (current) | 0.5927 | Measured |
| Phase 1 (stemming + stop words) | **0.6327** | **Measured (2026-03-10)** |
| Phase 2 (phrase boost + field boost) | 0.64-0.66 | Medium |
| Phase 3 (query expansion) | 0.65-0.68 | Low-Medium (conditional) |
| Phase 4 (cross-encoder rerank) | 0.67-0.72 | Low (end-to-end gains uncertain) |
| Phase 5 (neural sparse retrieval) | 0.70-0.78 | Low (high effort, architecture change) |

**Honest assessment**: Getting BM25 alone to 0.70 is unlikely without neural retrieval. The realistic ceiling for classical BM25 with good text analysis is ~0.66-0.68. To reach 0.70+, we likely need either cross-encoder reranking (Phase 4) or neural sparse retrieval (Phase 5).

---

## Future: Multilingual Support

**Not blocking NDCG goal** — separate effort when non-English topics are needed.

### What's easy (Snowball languages)

Tantivy's `Stemmer` uses `rust-stemmers` which supports ~15 languages out of the box:
Arabic, Danish, Dutch, English, Finnish, French, German, Greek, Hungarian, Italian,
Norwegian, Portuguese, Romanian, Russian, Spanish, Swedish, Tamil, Turkish.

Per-topic config: `analyzer.language=spanish` → selects the right stemmer + stop word list.
Stop word lists per language are widely available (Lucene ships them).

### What's hard (CJK + Thai)

Chinese, Japanese, Korean, and Thai require fundamentally different tokenizers:
- **Chinese**: dictionary-based segmentation (jieba, THULAC) or character n-grams
- **Japanese**: MeCab or kuromoji morphological analysis
- **Korean**: mecab-ko or Hangul jamo decomposition
- **Thai**: word boundary detection (no spaces between words)

These need dedicated tokenizer implementations, not just a language flag on `Stemmer`.
Tantivy has community crates (`tantivy-jieba`, `tantivy-tokenizer-api`) but they need evaluation.

### Recommended approach

1. Phase 1 ships with `language` as a parameter (English default) — **done in SQ-1.1**
2. Snowball languages: add stop word lists per language, expose via topic config
3. CJK: evaluate tantivy-jieba / lindera, add as optional tokenizer backends
4. Per-field language: not needed initially — one language per topic is sufficient

---

## References

1. **Vectorian article** (2026-03-05): "All I Wanted Was a Simple Code Search" — demonstrates reranking NDCG gains vanish end-to-end. Linear LTR cannot beat RRF on small corpora. https://www.vectorian.be/articles/2026-03-05/all-i-wanted-was-a-simple-code-search/

2. **Tantivy custom analyzers**: TextAnalyzer builder with StopWordFilter + Stemmer confirmed working in 0.24. Register with `index.tokenizers().register()`.

3. **Cross-encoder benchmarks**: +5-28% NDCG@10 in isolation, +5-10% end-to-end. MiniLM-L-6 achieves ~10ms/pair on CPU. Source: emergentmind.com/topics/bm25s-reranker

4. **ColBERT**: MRR@10 ~0.343 on MS MARCO, sub-200ms CPU latency via PLAID. Source: arxiv.org/html/2508.03555v1

5. **Query expansion**: PRF/RM3 adds 3-15% on TREC DL. LLM-based expansion (CSQE, ThinkQE) outperforms static expansion. Source: emergentmind.com/topics/llm-based-query-expansion

6. **SPLADE**: Outperforms BM25 on semantic tasks but falls below BM25 out-of-domain. Best as complement, not replacement. Source: qdrant.tech/articles/minicoil
