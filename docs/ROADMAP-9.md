# Chronik Phase 9 — Query Orchestrator + Ranking Plane

**Project**: Multi-Topic Query Planning + Deterministic Ranking Infrastructure  
**Target Version**: v2.4.0  
**Start Date**: ___________  
**Estimated Duration**: 4–6 weeks  

---

## Purpose

Turn Chronik from a system that **retrieves data** into a system that **decides what matters**.

This phase introduces:
1. **Query Orchestrator** — One query spanning multiple topics and capabilities (text, vector, SQL, fetch)
2. **Ranking Plane** — Deterministic, extensible ranking with profiles, policies, and feature extraction

---

## Strategic Outcome

After this phase, Chronik supports:
- Multi-topic, multi-capability queries via a single API
- Unified result streams across logs, events, documents, and marketplace-style data
- Configurable, profile-based ranking logic (no redeploy required)
- A foundation for future Learning-to-Rank (LTR) and policy-driven governance

---

## Architecture Overview

```
Client
  │
  ▼
Unified API (/_query)
  │
  ▼
Query Orchestrator
  │
  ├─ Text Search (Tantivy)
  ├─ Vector Search (HNSW)
  ├─ SQL / Columnar (DataFusion)
  └─ Fetch (Kafka WAL)
  │
  ▼
Candidate Set (Normalized)
  │
  ▼
Feature Extraction
  │
  ▼
Ranking Plane
  ├─ RuleRanker (v1)
  └─ Policy Overlay
  │
  ▼
Ranked Results + Explanations
```

---

## High-Level Components

### TopicCapabilities
- `text_search`
- `vector_search`
- `sql_query`
- `fetch_offsets`
- `rankable`

### QueryPlan
- Declarative execution graph for multi-source queries
- Parallel execution with timeout + fallback handling

### Candidate
- Normalized result format across all query types

### Ranker
- Pluggable ranking interface
- Profile-driven behavior

---

## Unified API — New Endpoint

### `POST /_query`

### Request Schema

```json
{
  "sources": [
    {"topic":"logs","modes":["text","vector"]},
    {"topic":"orders","modes":["sql"]},
    {"topic":"events","modes":["fetch"]}
  ],
  "q": {
    "text": "payment failed",
    "semantic": "card declined"
  },
  "filters": {
    "timestamp_gte": "...",
    "timestamp_lte": "..."
  },
  "k": 50,
  "rank": {
    "profile": "default"
  }
}
```

### Response Schema

```json
{
  "query_id": "...",
  "results": [
    {
      "topic": "logs",
      "partition": 2,
      "offset": 91827,
      "final_score": 0.92,
      "features": {
        "text_rank": 1,
        "vector_rank": 3,
        "freshness": 0.88
      },
      "explanation": "Boosted by freshness + semantic match"
    }
  ],
  "stats": {
    "candidates": 240,
    "ranked": 50,
    "latency_ms": 42
  }
}
```

---

## Phase 9.1 — Foundation & Crate Setup (Week 1)

### Tasks
- [ ] Create crate `crates/chronik-query`
- [ ] Add to workspace `Cargo.toml`
- [ ] Define core modules:
  - `query_plan.rs`
  - `capabilities.rs`
  - `candidate.rs`
  - `ranker.rs`
  - `profiles.rs`
- [ ] Add shared types to `chronik-common`
- [ ] Compile + unit test scaffold

---

## Phase 9.2 — Topic Capabilities & Discovery (Week 1)

### Tasks
- [ ] Add `TopicCapabilities` struct
- [ ] Implement detection logic from topic config
- [ ] Cache capabilities in server state
- [ ] Admin endpoint: `GET /_query/capabilities`

---

## Phase 9.3 — Query Plan Engine (Week 2)

### Tasks
- [ ] Implement `QueryPlanBuilder`
- [ ] Execution nodes: TextNode, VectorNode, SqlNode, FetchNode
- [ ] Parallel execution + timeouts
- [ ] Normalize outputs into `Vec<Candidate>`

---

## Phase 9.4 — Candidate Normalization (Week 2)

### Tasks
- [ ] Implement adapters for all backends
- [ ] Deduplication logic
- [ ] CandidateSet container

---

## Phase 9.5 — Ranking Plane v1 (Week 3)

### Tasks
- [ ] Define `Ranker` trait
- [ ] Implement `RuleRanker`
- [ ] Feature extraction
- [ ] Explanation generator

---

## Phase 9.6 — Ranking Profiles (Week 3)

### Tasks
- [ ] Define profile schema (YAML/JSON)
- [ ] Hot reload
- [ ] Built-in profiles

---

## Phase 9.7 — Policy Overlay (Week 4)

### Tasks
- [ ] PolicyEngine interface
- [ ] Filters + enforcement
- [ ] Logging

---

## Phase 9.8 — Unified API Integration (Week 4)

### Tasks
- [ ] Add `query_handler.rs`
- [ ] Mount `POST /_query`
- [ ] Prometheus metrics

---

## Phase 9.9 — Proof-of-Scale Harness (Week 5)

### Tasks
- [ ] Synthetic data generator
- [ ] Load harness (10M+ records)
- [ ] Query benchmark
- [ ] Failure injection

---

## Phase 9.10 — Testing & Documentation (Week 6)

### Tasks
- [ ] End-to-end integration tests
- [ ] Multi-topic tests
- [ ] Profile reload tests
- [ ] Docs

---

## Strategic Positioning

> Chronik becomes a **decision-oriented data plane**, not just a retrieval system.
