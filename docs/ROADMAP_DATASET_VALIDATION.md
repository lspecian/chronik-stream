# Dataset Validation Roadmap — Ecommerce & Scale

**Status**: Planned
**Target**: v2.5.x
**Prerequisites**: Phase 9 (Query Orchestrator) complete, SV-1 (Embedding cache) from [ROADMAP_SCALE_VALIDATION.md](ROADMAP_SCALE_VALIDATION.md)
**Related**: [MARKET_COMPARISON.md](MARKET_COMPARISON.md), [SEARCH_RELEVANCE_PERFORMANCE.md](SEARCH_RELEVANCE_PERFORMANCE.md), [ROADMAP_SCALE_VALIDATION.md](ROADMAP_SCALE_VALIDATION.md)

---

## Motivation

Current validation uses 30K synthetic messages across 3 topics. This proved Chronik works and is fast, but leaves two gaps:

1. **No search quality measurement** — We benchmarked latency and throughput but never measured whether search results are actually *relevant*. Without ground-truth relevance labels, we can't compute NDCG, precision, or recall — the metrics that matter for search.

2. **Unvalidated beyond 150K docs** — Elasticsearch and Pinecone publish benchmarks at 1M-50M+. We need real-world datasets at scale, not synthetic data.

This roadmap closes both gaps with two complementary datasets:

| Dataset | Purpose | Records | What it proves |
|---------|---------|---------|----------------|
| **WANDS (Wayfair)** | Search quality | 42K products + 233K relevance labels | Chronik returns *the right results*, measured by NDCG@10 |
| **Amazon Reviews 2023** | Scale validation | 1M-20M+ reviews | Chronik handles real-world ecommerce data at scale |

Together these shift the narrative from "fast on synthetic data" to "fast, relevant, and proven at scale on real product data."

---

## DV-1 — WANDS: Search Relevance Quality

**Goal**: Measure Chronik's search quality (NDCG@10, Precision@10) on a standard ecommerce benchmark and compare against published Elasticsearch and Weaviate results.

### Dataset: Wayfair Annotation Dataset (WANDS)

| Property | Value |
|----------|-------|
| **Source** | Wayfair (furniture/home goods retailer) |
| **Products** | 42,994 with name, description, category, features, ratings |
| **Queries** | 480 real search queries |
| **Relevance labels** | 233,448 query-product pairs labeled Exact / Partial / Irrelevant |
| **Format** | 3 CSV files: `product.csv`, `query.csv`, `label.csv` |
| **Download** | `git clone https://github.com/wayfair/WANDS.git` |
| **License** | Public, used in academic and industry benchmarks |

### Why WANDS

- **Ground-truth relevance labels** — The only way to measure NDCG, precision, and recall. Without labels you can only measure speed, not quality.
- **Published baselines** — Elasticsearch BM25, Weaviate hybrid, and Solr results exist on this dataset. Direct apples-to-apples comparison.
- **Product search use case** — Validates Chronik for ecommerce, not just log analysis.
- **Small but complete** — 42K products is small enough to test quickly but large enough for meaningful quality metrics.
- **Rich text fields** — Product descriptions average 50-200 words, good for both BM25 and embedding quality.

### Published Baselines (What We Compare Against)

| System | NDCG@10 | Method | Source |
|--------|---------|--------|--------|
| BM25 (Elasticsearch) | ~0.50-0.56 | Lexical search | softwaredoug.com, jxnl.co |
| Hybrid (ES BM25 + kNN) | ~0.58-0.65 | RRF fusion | softwaredoug.com |
| Agentic search (LLM-driven) | ~0.65-0.72 | LLM reranking | softwaredoug.com |

These are the numbers we need to match or beat.

### Test Plan

#### Step 1: Ingest WANDS into Chronik

```bash
# Clone dataset
git clone https://github.com/wayfair/WANDS.git

# Parse product.csv → Kafka messages (JSON)
# Key: product_id
# Value: {"product_name": "...", "product_description": "...", "product_class": "...", "category": "...", "features": "..."}
# Topic: wands-products (columnar + vector + text enabled)
```

Ingestion script creates one Kafka message per product. Configure topic with:
- `columnar.enabled=true` (SQL queries on product fields)
- `vector.enabled=true` + `vector.field=$.product_description` (semantic search on descriptions)
- Full-text search auto-enabled (Tantivy indexes all text)

#### Step 2: Build Search Index

Wait for WalIndexer to complete:
- Tantivy: ~42K docs indexed (~30-60s)
- Embeddings: ~42K descriptions embedded via OpenAI (~$0.03, ~15 minutes)
- Parquet: ~42K rows written

#### Step 3: Run All 480 Queries, All Modes

For each of the 480 queries in `query.csv`:

| Mode | Endpoint | What it tests |
|------|----------|---------------|
| **Text (BM25)** | `POST /_search` | Tantivy relevance ranking |
| **Vector (semantic)** | `POST /_vector/wands-products/search` | Embedding similarity ranking |
| **Hybrid (RRF)** | `POST /_vector/wands-products/hybrid` | RRF fusion quality |
| **SQL** | `POST /_sql` | Structured filtering (`WHERE product_class = ...`) |

Collect top-10 results for each query.

#### Step 4: Compute Quality Metrics

Match returned results against `label.csv` ground truth:

```python
# NDCG@10 computation
# Relevance mapping: Exact=3, Partial=1, Irrelevant=0
for query in queries:
    ideal_ranking = sorted(labels[query], reverse=True)[:10]
    actual_ranking = [labels[query].get(doc_id, 0) for doc_id in results[query][:10]]
    ndcg = compute_ndcg(actual_ranking, ideal_ranking)
```

**Metrics to compute per mode**:
- **NDCG@10** — Primary quality metric (normalized discounted cumulative gain)
- **Precision@10** — Fraction of top-10 that are Exact or Partial
- **Recall@100** — Fraction of all Exact matches found in top-100
- **MRR** — Mean reciprocal rank of first Exact match

#### Step 5: Compare Against Baselines

| System | NDCG@10 | Notes |
|--------|---------|-------|
| ES BM25 baseline | ~0.50-0.56 | Published on WANDS |
| ES Hybrid (BM25 + kNN) | ~0.58-0.65 | Published on WANDS |
| **Chronik BM25** | ? | Tantivy |
| **Chronik Vector** | ? | HNSW + OpenAI embeddings |
| **Chronik Hybrid** | ? | RRF (text + vector) |

### Success Criteria

- Chronik BM25 NDCG@10 >= 0.50 (match Elasticsearch baseline)
- Chronik Hybrid NDCG@10 >= 0.58 (match or beat ES hybrid)
- All 480 queries complete without errors
- Latency remains < 10ms for text, < 500ms for vector (42K is small)

### OpenAI Cost

| Item | Estimate |
|------|----------|
| 42K product descriptions (~100 tokens avg) | ~4.2M tokens |
| 480 queries (~5 tokens avg) | ~2.4K tokens |
| Cost at $0.02/1M tokens | **~$0.09** |

### K8s Manifests

| File | Purpose |
|------|---------|
| `tests/k8s-perf/70-wands-loader-configmap.yaml` | Python script to parse WANDS CSVs and produce to Kafka |
| `tests/k8s-perf/71-wands-loader-job.yaml` | K8s Job for ingestion |
| `tests/k8s-perf/72-wands-quality-configmap.yaml` | NDCG/precision/recall evaluation script |
| `tests/k8s-perf/73-wands-quality-job.yaml` | K8s Job for quality evaluation |

**Effort**: ~4-6 hours (download, ingest, run 480 queries x 4 modes, compute metrics, analyze)
**Risk**: Low — small dataset, well-understood benchmark, cheap embeddings

---

## DV-2 — Amazon Reviews 2023: Scale Validation

**Goal**: Validate Chronik at 1M-20M real-world ecommerce records. Close the "unvalidated beyond 150K" gap in [MARKET_COMPARISON.md](MARKET_COMPARISON.md).

### Dataset: Amazon Reviews 2023 (McAuley Lab, UCSD)

| Property | Value |
|----------|-------|
| **Source** | Amazon.com product reviews (June 1996 - Sept 2023) |
| **Total size** | 571M reviews, 48M items, 33 categories |
| **Format** | JSON Lines (`.jsonl.gz`) |
| **Download** | [UCSD Data Repo](https://datarepo.eng.ucsd.edu/mcauley_group/data/amazon_2023/raw/review_categories/) or [Hugging Face](https://huggingface.co/datasets/McAuley-Lab/Amazon-Reviews-2023) |
| **License** | Research use, publicly accessible |

### Category Subsets (pick the right scale)

| Category | Reviews | Items | Tokens (reviews) | Good for |
|----------|---------|-------|-------------------|----------|
| **Appliances** | 602K | 95K | 67M | Quick 1M validation |
| **Grocery_and_Gourmet_Food** | 5.1M | 289K | 432M | 5M validation |
| **Electronics** | 43.9M | 1.6M | 2.7B | 10M+ scale test |
| **Home_and_Kitchen** | 67.4M | 3.7M | 3.1B | Maximum scale test |

### Review Fields

Each review is a JSON object:

```json
{
  "rating": 4.0,
  "title": "Great blender for the price",
  "text": "I've been using this blender for 3 months now and it handles everything from smoothies to soups. The motor is powerful enough for ice crushing. Only downside is the lid doesn't seal perfectly.",
  "asin": "B09XYZ1234",
  "parent_asin": "B09XYZ0000",
  "user_id": "AF2M0KC...",
  "timestamp": 1672531200000,
  "verified_purchase": true,
  "helpful_vote": 12
}
```

Product metadata is available separately (`meta_<Category>.jsonl.gz`) with title, description, features, price, images.

### Test Plan

#### DV-2a — Scale Validation at 1M (Appliances)

Load 602K Appliances reviews. Quick validation that Chronik handles real ecommerce data at moderate scale.

**Ingestion**:
```bash
# Download
wget https://datarepo.eng.ucsd.edu/mcauley_group/data/amazon_2023/raw/review_categories/Appliances.jsonl.gz
gunzip Appliances.jsonl.gz

# Produce to Chronik
# Key: asin (product ID)
# Value: full review JSON
# Topic: amazon-appliances (columnar + text enabled)
```

**Queries**:

| Category | Queries |
|----------|---------|
| **Text search** | `blender not working`, `dishwasher leaking`, `air fryer smoke`, `refrigerator noise` |
| **SQL** | `SELECT rating, COUNT(*) FROM amazon_appliances_hot GROUP BY rating` |
| **SQL filter** | `SELECT * FROM amazon_appliances WHERE rating = 1.0 LIMIT 50` |
| **SQL time range** | `SELECT COUNT(*) FROM amazon_appliances WHERE timestamp > 1672531200000` |
| **Aggregation** | `SELECT asin, AVG(rating) FROM amazon_appliances GROUP BY asin ORDER BY AVG(rating) LIMIT 20` |

**Success criteria**:
- Text search p50 < 10ms at 602K docs
- SQL COUNT p50 < 20ms
- SQL GROUP BY p50 < 50ms
- Zero errors

#### DV-2b — Scale Validation at 5M (Grocery)

Load 5.1M Grocery_and_Gourmet_Food reviews.

**Queries** (same categories as DV-2a, adapted):

| Category | Queries |
|----------|---------|
| **Text search** | `coffee tastes burnt`, `protein powder clumps`, `expired product`, `organic certification` |
| **SQL** | `SELECT rating, COUNT(*) FROM amazon_grocery_hot GROUP BY rating` |
| **SQL product stats** | `SELECT parent_asin, COUNT(*), AVG(rating) FROM amazon_grocery GROUP BY parent_asin HAVING COUNT(*) > 100 ORDER BY AVG(rating) DESC LIMIT 20` |

**Success criteria**:
- Text search p50 < 15ms at 5M docs
- SQL COUNT p50 < 50ms
- Tantivy index build completes without OOM
- HotDataBuffer handles 5M rows or gracefully falls back to Parquet-only

#### DV-2c — Scale Validation at 10-20M (Electronics)

Load first 10M-20M Electronics reviews (truncate at target count during ingestion).

**Success criteria**:
- Text search p50 < 20ms at 10M docs
- SQL COUNT p50 < 100ms at 10M rows
- Tantivy merge strategy handles large index (may need tuning)
- System stable under sustained query load at this scale

### Scaling Expectations

Based on Tantivy and DataFusion architecture:

| Scale | Text Search (expected) | SQL COUNT (expected) | Rationale |
|-------|----------------------|---------------------|-----------|
| 30K (current) | 3.4ms | 5.9ms | Validated |
| 600K | < 10ms | < 20ms | Tantivy sub-linear, DataFusion columnar scan |
| 5M | < 15ms | < 50ms | Tantivy inverted index, Parquet pruning |
| 10M | < 20ms | < 100ms | May need Tantivy segment merge tuning |
| 20M | < 30ms | < 200ms | Approaches Elasticsearch published numbers |

---

## DV-3 — Sentiment Labeling Pipeline

**Goal**: Generate sentiment labels from Amazon reviews using an LLM on the k8s cluster, then use those labels for search quality evaluation and SQL analytics.

### Why This Matters

Amazon Reviews don't come with search relevance labels (unlike WANDS). But they have **ratings (1-5 stars)** and **review text** — which means we can:

1. **Derive sentiment labels** from ratings (trivial: 1-2 = negative, 3 = neutral, 4-5 = positive)
2. **Generate richer labels via LLM** — Run a small model (e.g., Llama 3, Mistral 7B, or Phi-3) on the k8s cluster to classify reviews into categories: complaint, praise, feature request, comparison, quality issue, etc.
3. **Use labels for search evaluation** — "Find negative reviews about battery life" should return reviews labeled as complaints about battery

### Architecture

```
Amazon Reviews (Chronik topic)
        │
        ▼
Label Generator (k8s Job)
  ├─ Consume from amazon-electronics topic
  ├─ For each review:
  │   ├─ Simple: rating → sentiment (1-2=neg, 3=neutral, 4-5=pos)
  │   └─ LLM: classify into [complaint, praise, feature_request, comparison, quality_issue, shipping]
  ├─ Produce labeled version to amazon-electronics-labeled topic
  └─ Metrics: labels/sec, model latency
        │
        ▼
Chronik indexes labeled data
  ├─ Tantivy: searchable by label + text
  ├─ Parquet: SQL analytics on labels
  └─ HNSW: semantic search with label-aware ranking
```

### Label Generation Options

#### Option A: Rating-Based Labels (Zero Cost, Instant)

```python
def label_from_rating(review):
    if review['rating'] <= 2.0:
        return 'negative'
    elif review['rating'] == 3.0:
        return 'neutral'
    else:
        return 'positive'
```

Produces basic sentiment. Good enough for SQL analytics and basic search evaluation.

#### Option B: LLM Classification on K8s (Richer Labels)

Deploy a small model on the k8s cluster:

| Model | VRAM | Speed | Quality |
|-------|------|-------|---------|
| **Phi-3 Mini (3.8B)** | ~4GB | ~50 reviews/s | Good for classification |
| **Mistral 7B** | ~8GB | ~20 reviews/s | Better nuance |
| **Llama 3 8B** | ~8GB | ~15 reviews/s | Best quality |

**K8s Job**:
```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: sentiment-labeler
spec:
  template:
    spec:
      containers:
        - name: labeler
          image: sentiment-labeler:latest
          resources:
            limits:
              nvidia.com/gpu: 1  # If GPU available
              memory: 16Gi
          env:
            - name: CHRONIK_BOOTSTRAP
              value: "chronik-query:9092"
            - name: SOURCE_TOPIC
              value: "amazon-electronics"
            - name: DEST_TOPIC
              value: "amazon-electronics-labeled"
            - name: MODEL
              value: "phi-3-mini"
```

**Prompt template**:
```
Classify this product review into one or more categories:
[complaint, praise, feature_request, comparison, quality_issue, shipping_issue, value_assessment]

Review: "{review_text}"

Categories:
```

#### Option C: OpenAI Classification (Highest Quality, Costs Money)

Use GPT-4o-mini for classification at ~$0.15/1M input tokens:

| Scale | Tokens (~100/review) | Cost |
|-------|---------------------|------|
| 10K reviews | 1M | $0.15 |
| 100K reviews | 10M | $1.50 |
| 1M reviews | 100M | $15.00 |

Best quality but gets expensive at scale. Good for creating a gold-standard subset.

### Recommended Approach

1. **Start with Option A** (rating-based) for all reviews — zero cost, immediate
2. **Run Option B** (Phi-3 on k8s) on 100K-500K reviews — validates the pipeline
3. **Use Option C** (OpenAI) on 10K reviews — gold-standard evaluation set

### Search Evaluation with Labels

Once labels exist, construct evaluation queries:

| Query | Expected results | Metric |
|-------|-----------------|--------|
| `battery life complaint` (text) | Reviews labeled `complaint` mentioning battery | Precision@10 |
| `better than competitor` (text) | Reviews labeled `comparison` | Precision@10 |
| `SELECT category, COUNT(*) GROUP BY category` | Label distribution | Correctness |
| `SELECT * WHERE label = 'quality_issue' AND rating = 1` | Negative quality reviews | Precision |
| `product stopped working after warranty` (vector) | Reviews labeled `complaint` + `quality_issue` | Semantic relevance |

### K8s Manifests

| File | Purpose |
|------|---------|
| `tests/k8s-perf/74-sentiment-labeler-configmap.yaml` | Label generator Python script |
| `tests/k8s-perf/75-sentiment-labeler-job.yaml` | K8s Job (CPU or GPU) |
| `tests/k8s-perf/76-label-quality-configmap.yaml` | Evaluation script |
| `tests/k8s-perf/77-label-quality-job.yaml` | K8s Job for evaluation |

**Effort**: ~6-8 hours (Option A: 1 hour, Option B: 4-6 hours for model setup + run, Option C: 1 hour)
**Risk**: Medium — GPU availability on k8s cluster, model memory requirements, inference speed at scale

---

## DV-4 — Combined Quality + Scale Report

**Goal**: Produce a single performance report combining WANDS quality metrics with Amazon scale benchmarks. Update [MARKET_COMPARISON.md](MARKET_COMPARISON.md) with validated claims.

### Report Structure

```
1. Search Quality (WANDS)
   - NDCG@10 per mode (BM25, vector, hybrid)
   - Comparison table vs Elasticsearch/Weaviate published baselines
   - Analysis: where Chronik wins, where it loses, why

2. Scale Performance (Amazon Reviews)
   - Latency at 600K, 5M, 10M, 20M
   - Scaling curve (latency vs document count)
   - Memory and CPU usage at each scale point
   - Tantivy index size and merge behavior

3. Ecommerce Use Case Validation
   - Product search quality (WANDS NDCG)
   - Review analytics (Amazon SQL queries)
   - Sentiment-aware search (labeled reviews)
   - Comparison with dedicated ecommerce search (Algolia, Searchspring)

4. Updated Market Comparison
   - Refresh MARKET_COMPARISON.md with validated numbers
   - Remove "unvalidated beyond 150K" caveat
   - Add search quality section (NDCG comparison)
```

### Files to Update

| File | Update |
|------|--------|
| `docs/SEARCH_RELEVANCE_PERFORMANCE.md` | Add WANDS quality results + Amazon scale results |
| `docs/MARKET_COMPARISON.md` | Replace "unvalidated" with real numbers, add NDCG comparison |

**Effort**: ~2-3 hours (analysis + writing)
**Risk**: Low — just documentation

---

## Summary

| Phase | What | Records | Cost | Effort | Risk |
|-------|------|---------|------|--------|------|
| **DV-1** | WANDS search quality (NDCG) | 42K products, 480 queries | ~$0.09 (OpenAI) | 4-6 hours | Low |
| **DV-2a** | Amazon Appliances (scale) | 602K reviews | $0 (text+SQL only) | 2-3 hours | Low |
| **DV-2b** | Amazon Grocery (scale) | 5.1M reviews | $0 (text+SQL only) | 3-4 hours | Medium |
| **DV-2c** | Amazon Electronics (scale) | 10-20M reviews | $0 (text+SQL only) | 4-6 hours | Medium |
| **DV-3** | Sentiment labeling pipeline | 100K-1M labeled | $0-15 depending on method | 6-8 hours | Medium |
| **DV-4** | Combined report + market update | — | $0 | 2-3 hours | Low |

### Execution Order

```
DV-1 (WANDS quality) ─────────────► First — proves search quality, small + fast
DV-2a (600K Appliances) ──────────► Parallel with DV-1 — quick scale check
DV-2b (5M Grocery) ───────────────► After DV-2a — medium scale
DV-3 Option A (rating labels) ────► After DV-2a — zero cost, enriches data
DV-2c (10-20M Electronics) ───────► After DV-2b — maximum scale
DV-3 Option B (LLM labeling) ─────► After DV-2c — requires model setup
DV-4 (combined report) ───────────► Last — synthesize everything
```

### What This Proves

| Claim | Dataset | Metric |
|-------|---------|--------|
| **Search quality matches Elasticsearch** | WANDS | NDCG@10 >= 0.50 (BM25), >= 0.58 (hybrid) |
| **Scales to millions of documents** | Amazon Reviews | Text search < 20ms at 10M docs |
| **SQL on streams at scale** | Amazon Reviews | COUNT < 100ms at 10M rows |
| **Works for ecommerce, not just logs** | WANDS + Amazon | End-to-end product search + review analytics |
| **Supports ML/AI pipelines** | Amazon + DV-3 | LLM-labeled data indexed and searchable |

### Relationship to Other Roadmaps

```
ROADMAP_SCALE_VALIDATION.md (SV-1..SV-4)
  └─ SV-1 (embedding cache) ◄── prerequisite for DV-1 vector queries
  └─ SV-2 (Thunderbird 16.6M) ◄── log-focused scale test (complementary)
  └─ SV-3 (cluster deploy) ◄── can run DV-2c on cluster for max throughput

ROADMAP_DATASET_VALIDATION.md (this doc, DV-1..DV-4)
  └─ DV-1 (WANDS quality) — search quality proof
  └─ DV-2 (Amazon scale) — ecommerce scale proof
  └─ DV-3 (sentiment pipeline) — ML/AI pipeline proof
  └─ DV-4 (combined report) — updated market claims

ROADMAP_RELEVANCE_ENGINE.md (Phase 10..11)
  └─ Phase 10 (Reranking + ML) ◄── DV-1 WANDS provides evaluation data for reranking experiments
  └─ Phase 11 (Policy + Governance) ◄── DV-3 labels enable policy-based filtering
```
