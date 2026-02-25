# Research: Query Orchestration, Ranking, and ML Relevance

**Date**: 2026-02-22
**Purpose**: Inform Chronik's query orchestrator and ranking architecture
**Scope**: Production ranking systems, ML relevance lifecycle, market positioning

---

## Executive Summary

This document synthesizes research across production ranking platforms (Vespa, Elasticsearch, OpenSearch, Weaviate, Pinecone), the ML relevance lifecycle (LTR, LLM-as-a-judge, feature stores), and market positioning for a unified streaming + relevance platform.

**Key finding**: No platform today combines data streaming (Kafka-like) with search, vector search, SQL, and ranking in a single system. Companies need 8-15 distinct systems for an ML relevance pipeline. Chronik already has streaming, search, vector, SQL, and columnar storage — adding orchestration and ranking completes a category-defining platform.

---

## 1. How Production Ranking Systems Work

### Vespa: The Gold Standard

Vespa implements the most sophisticated ranking architecture in open-source:

**Three-Phase Ranking Pipeline**:

| Phase | Where It Runs | Documents Scored | Cost | Typical Use |
|-------|---------------|-----------------|------|-------------|
| First-Phase | Content nodes | Every match | Cheap | BM25 + simple features |
| Second-Phase | Content nodes | Top-N per node (default 100) | Moderate | GBDT models (XGBoost/LightGBM) |
| Global-Phase | Container (stateless) | Top-N globally (default 100) | Expensive | Cross-encoder, ONNX inference |

**Ranking Expression Language**: Vespa has a full mathematical expression language, not just configuration:

```
rank-profile my-profile {
    first-phase {
        expression: bm25(title) + 3 * freshness(timestamp)
    }
    second-phase {
        expression: xgboost("my-model.json")
        rerank-count: 50
    }
    global-phase {
        expression: sum(onnx(cross_encoder).out)
        rerank-count: 200
    }
}
```

Supports: arithmetic operators, conditionals (`if/switch`), tensor operations, built-in rank features (~100+), ML model calls (`xgboost()`, `lightgbm()`, `onnx()`), normalization functions (`normalize_linear()`, `reciprocal_rank()`, `reciprocal_rank_fusion()`).

**Feature Model**: Three categories computed at query time (no external feature store):
- **Document features**: `attribute(fieldName)` for any indexed field
- **Query features**: `query(name)` for values sent with the query
- **Computed features**: BM25, fieldMatch, closeness, freshness (generated from matching)

**Progression from Rules to ML**:
1. Start with BM25 (zero-shot, no training data needed)
2. Add hand-tuned rules (`bm25(title) + 3 * freshness + popularity`)
3. Introduce GBDT models (train offline, deploy as second-phase)
4. Add neural models (ONNX cross-encoders in global-phase)

**Why Vespa is considered the gold standard**:
- Computation at data (co-located, minimal network traffic)
- True multi-phase (three distinct levels, not just a rescore)
- Full expression language (not just config)
- Real-time updates (continuous indexing, no shard rebalancing)
- 8.5x higher throughput per CPU core vs Elasticsearch for hybrid queries

### Elasticsearch Learning-to-Rank (LTR)

Two implementations exist:

**Community Plugin (o19s)**: Five-stage workflow:
1. Judgment list development (external, human-labeled)
2. Feature engineering (templated queries stored in feature store)
3. Feature logging (log feature values during production search)
4. Model training (external, XGBoost or RankLib)
5. Model deployment (upload model, use as Elasticsearch rescore)

**Native LTR (8.13+)**: Integrated via `eland` Python library for feature extraction, training data collection, model upload, and search-time rescoring.

Both use Elasticsearch's rescore functionality — a second-phase re-ranking of the top-N results from the initial query.

### OpenSearch

Adds **Search Pipelines** (v2.12+) — composable request/response processors:
- Rerank processor supports ML Commons models, Cohere, Amazon Bedrock
- LTR plugin (forked from o19s) with modernized system indices
- Neural search via ML Commons for hybrid BM25 + vector

### Weaviate

**Hybrid Search Architecture**: Runs BM25 and vector search in parallel, combines via:
- **Relative Score Fusion** (default since v1.24): Normalizes scores to [0,1], weighted sum
- **Ranked Fusion** (legacy): Uses rank positions only

`alpha` parameter controls balance: 1.0 = pure vector, 0.0 = pure BM25, 0.5 = equal.

**Reranking**: Second-stage operation via pluggable modules (Cohere, VoyageAI, local transformers).

### Pinecone

**Two-Stage Retrieval**: Fast vector retrieval (bi-encoder) -> expensive reranking (cross-encoder).

**Rerank API**: Standalone endpoint or integrated into search. Models: `bge-reranker-v2-m3` (free), `cohere-rerank-3.5` (paid), `pinecone-rerank-v0` (proprietary).

**Key design**: Collapses embed + search + rerank into a single API call.

### Cross-Platform Comparison

| Capability | Vespa | ES LTR | OpenSearch | Weaviate | Pinecone |
|-----------|-------|--------|-----------|----------|----------|
| Ranking Phases | 3 | 2 (rescore) | 2 (pipeline) | 2 (search + rerank) | 2 (retrieve + rerank) |
| Expression Language | Full math + tensors | N/A | N/A | Alpha only | N/A |
| Built-in Features | ~100+ rank features | Templated queries | Templated queries | BM25 + vectors | Vectors only |
| Feature Logging | match-features, summary-features | Dedicated API | sltr logging | explainScore | None |
| Model Types | XGBoost, LightGBM, ONNX, TF | XGBoost, RankLib | XGBoost, RankLib | Cohere, VoyageAI | Hosted cross-encoders |
| Cold-Start | BM25 (zero-shot) | BM25 + function_score | BM25 + neural | Hybrid (pre-trained) | Pre-trained embeddings |

---

## 2. The ML Relevance Lifecycle

### Learning-to-Rank (LTR) Fundamentals

**Three approaches**:

| Approach | What It Optimizes | Models | Pros | Cons |
|----------|------------------|--------|------|------|
| Pointwise | Absolute relevance per doc | Regression, classification | Simple | Ignores relative ordering |
| Pairwise | Which doc in a pair is better | RankNet, LambdaRank, LambdaMART | Models relative order | Pair explosion at scale |
| Listwise | Ranking metric over full list | ListNet, LambdaMART (NDCG) | Direct metric optimization | Most complex to train |

**Dominant model**: LambdaMART (XGBoost `rank:ndcg`) — gradient boosting trees with LambdaRank scaling. Remains the industry standard. A 2024 study found "most recent neural LTR models are inferior to the best GBDTs on benchmark datasets."

**Feature categories**:
- **Query features**: length, term count, intent classification, popularity
- **Document features**: freshness, PageRank, popularity, quality scores, metadata
- **Query-document features**: BM25 score, cosine similarity, field match scores, historical CTR

### Training Data: The Critical Challenge

Three sources:

1. **Human judgments** (explicit): Domain experts rate query-doc pairs 0-4. Highest quality, most expensive ($2-10+ per judgment). Establishes ground truth.

2. **Click data** (implicit): Clicks, dwell time, conversions. Cheap and scalable but noisy, subject to position bias. Click models (position-based, cascade) attempt debiasing.

3. **LLM-generated judgments**: LLMs rate query-doc pairs. Scalable ($2-10 per 1000 judgments). Research shows good correlation with human judges when using chain-of-thought prompting. Vespa's blog demonstrated significant retrieval improvements from LLM-generated judgments.

**Recommended workflow**: Start with small expert human judgment set (ground truth) -> scale with LLM-generated judgments validated against human baseline -> supplement with click data.

### LLM-as-a-Judge for Relevance

Three use patterns in 2025-2026:

1. **Judgment generation**: LLMs evaluate query-doc pairs, replacing/augmenting human annotators. A SIGIR 2025 paper formalized this as "an instantiation of Learning-to-Rank where the judge model implicitly implements an LTR paradigm."

2. **Reranking**: LLMs re-order candidate documents after initial retrieval. Can be pointwise, pairwise, or listwise. AFR-Rank framework adds a filtering step before LLM reranking to reduce cost.

3. **Query understanding**: Parse, expand, classify, and reformulate queries for better retrieval.

**Caveats**: LLM judges are more lenient than humans, exhibit bias toward LLM-generated content, and have limited ability to discern subtle performance differences.

### Cross-Encoder vs Bi-Encoder vs ColBERT

| Architecture | How It Works | Latency | Scalability | Quality |
|-------------|-------------|---------|-------------|---------|
| Bi-encoder | Query and doc encoded independently, cosine similarity | 5-20ms | Billions (pre-computed) | Good |
| Cross-encoder | Query + doc processed jointly through transformer | 100-500ms/pair | Top 10-100 only | Excellent |
| ColBERT (late interaction) | Independent token-level encoding, MaxSim matching | 20-100ms | Millions (pre-computed) | Very good |

**Production pattern**: Bi-encoder retrieval (top 1000) -> ColBERT/cross-encoder rerank (top 100) -> capture most quality improvement with 90% less compute.

**Optimization**: Walmart's SIGIR 2025 paper achieved 40-65% latency reduction for BERT cross-encoders via ONNX/TensorRT conversion, operator fusion, text truncation, and query batching.

### The Reranking Latency/Cost Spectrum

| Approach | Latency | Cost/1K Queries | Quality |
|----------|---------|-----------------|---------|
| BM25 baseline | 1-5ms | ~$0 | Moderate |
| LambdaMART/GBDT | 1-10ms | ~$0 | Good |
| ColBERT | 20-100ms | $0.10-1.00 | Very good |
| Cross-encoder (Cohere) | 100-500ms | $1-3 | Excellent |
| LLM reranker (GPT-4 class) | 1-7 seconds | $25-30 | Excellent |

**Cascade pattern**: BM25 + vectors (top 200) -> GBDT (top 100) -> cross-encoder (top 20) -> LLM (top 5, high-value only).

### Feature Stores

Production ranking systems manage features in three categories:

- **User/query features** (offline or streaming): user embeddings, click patterns, session context
- **Document features** (offline, refreshed periodically): embeddings, quality scores, metadata
- **Query-document features** (online, real-time): BM25 scores, semantic similarity, personalized history

**Vespa's approach** (recommended for Chronik): Features computed at query time by the ranking engine, not pulled from external store. This eliminates training-serving skew. Features are logged with queries for offline model training.

**External stores** (Feast, Tecton): Used when features require complex pre-computation (user embeddings, 30-day rolling statistics). Feast is open-source, Tecton is managed. Dropbox Dash uses Feast for search ranking features.

### The 7-Phase Progression

Every company follows this lifecycle:

```
Phase 1: Rules-Based Baseline
  BM25 + hand-tuned boosting/burying rules
  -> Working search with business-controlled relevance

Phase 2: Measurement
  Judgment lists (human + LLM) + offline metrics (NDCG, MRR)
  -> Ability to measure relevance quality

Phase 3: First ML Model
  LambdaMART/XGBoost on judgments + click data
  -> Airbnb: "largest step improvement in bookings"

Phase 4: Experimentation Infrastructure
  A/B testing + interleaving + feature store + automated pipeline
  -> Rapid iteration (Airbnb: interleaving = 50x faster than A/B)

Phase 5: Multi-Stage Pipeline
  BM25 recall -> cross-encoder rerank -> business rules
  -> Precision-focused ranking with business control

Phase 6: Deep Learning & Personalization
  Neural rankers, two-tower retrieval, real-time features
  -> LinkedIn LiRank (KDD 2024), personalized ranking

Phase 7: Governance & Production Excellence
  Access control, content policies, audit trails, bias detection
  -> Enterprise-grade relevance with compliance
```

Companies like Airbnb took 3-5 years from Phase 1 to Phase 6. The platform should support all phases so users progress at their own pace.

---

## 3. Policy and Governance in Ranking

### Business Rules (Boosting, Burying, Pinning)

| Rule Type | What It Does | When Applied | Example |
|-----------|-------------|-------------|---------|
| Pinning | Fix specific result at specific position | After ranking (overrides ML) | "Pin 'Getting Started' at position 1 for 'onboarding'" |
| Boosting | Increase score for matching results | Before ranking (ML can override) | "Boost featured products +20%" |
| Burying | Decrease score for matching results | Before ranking (ML can override) | "Bury out-of-stock items" |
| Hiding/Excluding | Remove from results entirely | Pre-filter | "Exclude deleted content" |
| Scheduling | Time-bound rules | Any stage | "Boost holiday products Dec 1-25" |

**Key interaction**: In Algolia, pinning/hiding apply AFTER ML ranking (override). Boosting/burying apply BEFORE (ML can adjust). This ordering matters.

### Access Control

Enterprise search requires document-level security:
- Each document tagged with ACLs
- Query-time filtering removes unauthorized results BEFORE ranking
- Real-time permission sync when roles change
- Field-level security (redact sensitive fields like salary, SSN)

### Content Policies

- **NSFW/Safety**: Content classification -> remove or heavily demote
- **Compliance** (HIPAA, SOX, ITAR): Data classification + handling requirements
- **Data sovereignty**: 75+ countries with localization laws (2024). EU Cloud Sovereignty SEAL (2025).

### Audit and Explainability

- **Audit trails**: Log every query, results shown, positions, clicks
- **Explainability**: Score breakdowns, feature importance, ranking reasoning
- **Model governance**: Versioning, A/B test records, rollback, approval workflows
- **Bias detection**: Monitoring for systematic disadvantage of content types/categories

---

## 4. RAG Infrastructure

### Why Retrieval Quality Matters

Anthropic's Contextual Retrieval research quantifies the impact:

| Retrieval Method | Failure Rate (top-20) | Improvement |
|-----------------|----------------------|-------------|
| Basic embedding | 5.7% | Baseline |
| + Contextual embeddings | 3.7% | 35% better |
| + Contextual BM25 | 2.9% | 49% better |
| + Reranking | 1.9% | 67% better |

Chunking quality constrains retrieval more than model choice. Optimized semantic chunking: 0.79-0.82 faithfulness vs naive chunking: 0.47-0.51.

### Production RAG Stack

Recommended: hybrid retrieval (embeddings + BM25) -> reranking (cross-encoder, top 20-50) -> LLM generation.

- **Cohere Embed v4**: Highest MTEB score (65.2). Rerank 3.5 adds 25% improvement.
- **Voyage AI**: 9.74% better than OpenAI, 2.2x cheaper per million tokens.
- **Optimal candidate set**: 50-75 documents for most applications.

### What This Means for Chronik

A streaming platform with built-in hybrid retrieval, chunking, and reranking eliminates 3-4 systems from a typical RAG pipeline:

**Today**: Kafka -> processing -> vector DB -> search engine -> reranker -> LLM
**Chronik**: Produce -> search + rank -> LLM (data never leaves the platform)

---

## 5. Market Positioning

### The Fragmented Landscape

Typical ML relevance pipeline requires 8-15 systems:

```
Kafka (ingest) -> Flink (process) -> Elasticsearch (search) ->
Vector DB (embeddings) -> Feature Store (features) -> ML Model (rank) ->
Serving Layer (serve) -> Monitoring (observe)
```

70% of data teams juggle 5-7 tools daily. 75% of organizations actively pursuing vendor consolidation (Gartner, up from 29% in 2020).

### What Exists Today

| System | Streaming | Search | Vector | SQL | Ranking | Serving |
|--------|-----------|--------|--------|-----|---------|---------|
| Confluent/Kafka | Yes | No | No | No | No | No |
| Vespa | No | Yes | Yes | Partial | Yes | Yes |
| Elasticsearch | No | Yes | Yes | Partial | Rescore | Yes |
| Weaviate/Pinecone | No | Limited | Yes | No | Limited | Limited |
| Redpanda | Yes | No | No | No | No | No |
| Databricks | Batch | No | No | Yes | Via MLflow | Via MLflow |

**No single platform covers ingest -> search -> rank -> serve.**

### How Competitors Position

**Confluent**: "Central nervous system" that CONNECTS to other systems. Moving toward AI via Flink AI, Tableflow, Confluent Intelligence (Oct 2025). Strategy: composability, not unification. Does NOT build search or ranking into Kafka.

**Vespa**: "Full-stack serving platform" (search + vector + ranking + serving). But requires external streaming infrastructure (Kafka/Kinesis). Partners with Nexla for data integration.

**The gap**: Confluent builds connectors OUT. Vespa expects data IN. Nobody builds from streaming UP into relevance.

### Chronik's Unique Position

Chronik already has what no other single system combines:

| Capability | Chronik | Nearest Competitor |
|-----------|---------|-------------------|
| Kafka wire protocol (19 APIs) | Complete | Confluent, Redpanda |
| WAL-based durability | Complete | Kafka, Redpanda |
| Full-text search (Tantivy) | Complete | Elasticsearch, Vespa |
| Vector search (HNSW) | Complete | Pinecone, Weaviate |
| SQL queries (DataFusion) | Complete | Databricks, Flink |
| Columnar storage (Parquet) | Complete | Databricks, Snowflake |
| Schema Registry | Complete | Confluent |
| Raft clustering | Complete | Kafka (KRaft) |
| **Query Orchestrator + Ranking** | **Phase 9** | **Vespa** |

### Target Use Cases

Ranked by market opportunity and fit:

1. **RAG infrastructure** — Hottest market, most fragmented tooling. Chronik as the retrieval layer that doesn't need 4 separate systems.

2. **E-commerce search & ranking** — Massive market ($7T+ global e-commerce). Clear pain from system fragmentation. Real-time inventory/pricing needs streaming.

3. **Real-time observability with relevance** — Alert fatigue is the #1 obstacle to incident response. Rank alerts by severity and context, not just fire thresholds.

4. **Enterprise knowledge search** — 47% YoY market growth. Permission-aware hybrid search across documents.

5. **Fraud detection** — Real-time event scoring, combining streaming features with ML ranking. Currently requires 6+ systems.

### The Value Proposition

> "Stop operating 8 systems for relevance. Ingest, search, rank, and serve from one platform."

The positioning is not "another Kafka" or "another Elasticsearch." It is the first platform built for the **full ML relevance lifecycle** — from data ingestion through search and ranking to serving — in a single binary.

---

## 6. Architectural Recommendations for Chronik

### Adopt Vespa's Multi-Phase Model

Three-phase ranking with each phase optional and composable:

```
Phase 1 (Retrieval): BM25 + vector + SQL + fetch -> candidate set
Phase 2 (First-Pass): RRF merge + rule/expression scoring -> ranked list
Phase 3 (Rerank): Cross-encoder / GBDT / LLM -> final ranking
```

### Build Features Into the Platform (Not External)

Follow Vespa's approach: features computed at query time, logged with every query, exportable for offline training. No external feature store dependency.

### Support the Full Lifecycle From Day One

Every phase of the ranking lifecycle should have a home in the platform:
- Phase 1 (Rules): JSON/YAML profiles with weight-based scoring
- Phase 2 (Measurement): Feature logging + judgment collection APIs
- Phase 3 (First ML): Load XGBoost/LightGBM model files
- Phase 5 (Multi-Stage): Pluggable rerankers (Cohere, ONNX, LLM)
- Phase 7 (Governance): Policy overlay with access control, audit logging

### Support Both Grouped and Merged Results

- `"merged"`: Single ranked list across all topics (RRF for cross-topic)
- `"grouped"`: Results partitioned by topic, independently ranked
- Let the caller choose per request

### Ranking Profiles: Start Simple, Build Toward Expressions

Phase 1: JSON/YAML weight maps (`{"rrf_score": 0.6, "freshness": 0.2}`)
Phase 2: Simple expression language (`rrf_score * 0.6 + freshness * 0.2`)
Phase 3: Full expression language with conditionals and model calls (Vespa-style)

---

## References

### Production Systems
- Vespa phased ranking: https://docs.vespa.ai/en/ranking/phased-ranking.html
- Vespa ranking expressions: https://docs.vespa.ai/en/ranking/ranking-expressions-features.html
- Vespa vs Elasticsearch benchmark: https://blog.vespa.ai/elasticsearch-vs-vespa-performance-comparison/
- Elasticsearch LTR plugin: https://elasticsearch-learning-to-rank.readthedocs.io/
- Elasticsearch native LTR: https://www.elastic.co/docs/solutions/search/ranking/learning-to-rank-ltr
- OpenSearch reranking: https://docs.opensearch.org/latest/search-plugins/search-relevance/reranking-search-results/
- Weaviate hybrid search: https://docs.weaviate.io/weaviate/search/hybrid
- Pinecone reranking: https://docs.pinecone.io/guides/search/rerank-results

### ML Relevance
- LambdaMART: https://www.shaped.ai/blog/lambdamart-explained-the-workhorse-of-learning-to-rank
- XGBoost LTR: https://xgboost.readthedocs.io/en/latest/tutorials/learning_to_rank.html
- LLM-as-a-judge (SIGIR 2025): https://krisztianbalog.com/files/sigir2025-llms.pdf
- Vespa LLM-as-judge: https://blog.vespa.ai/improving-retrieval-with-llm-as-a-judge/
- ColBERT: https://arxiv.org/abs/2004.12832
- Walmart BERT optimization (SIGIR 2025): https://dl.acm.org/doi/10.1145/3726302.3731965

### Industry Case Studies
- Airbnb deep learning search: https://ar5iv.labs.arxiv.org/html/1810.09591
- Airbnb interleaving: https://medium.com/airbnb-engineering/beyond-a-b-test-speeding-up-airbnb-search-ranking-experimentation-through-interleaving-7087afa09c8e
- LinkedIn LiRank (KDD 2024): https://arxiv.org/abs/2402.06859
- Shopify product search: https://shopify.engineering/world-class-product-search
- Dropbox Dash feature store: https://dropbox.tech/machine-learning/feature-store-powering-realtime-ai-in-dropbox-dash

### RAG & Market
- Anthropic contextual retrieval: https://www.anthropic.com/news/contextual-retrieval
- Confluent Intelligence: https://www.businesswire.com/news/home/20251029382595/en/
- Data streaming landscape 2026: https://www.kai-waehner.de/blog/2025/12/05/the-data-streaming-landscape-2026/
- Vector DB commoditization: https://venturebeat.com/ai/from-shiny-object-to-sober-reality-the-vector-database-story-two-years-later/
