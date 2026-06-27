# Agent Memory Roadmap

**Goal**: Ship Chronik Agent Memory — a unified, event-native memory layer for AI agents — leveraging Chronik's existing primitives (Kafka WAL, Tantivy, HNSW, DataFusion).
**Strategic position**: Beat Cloudflare Agent Memory on query power, event-nativity, and multi-modal retrieval. Match its ergonomics on the agent-facing API.
**Benchmark (recall quality)**: [LongMemEval](https://github.com/xiaowu0162/LongMemEval) (500 questions, 5 reasoning categories, long-form conversation memory).
**Benchmark (extraction quality)**: Custom labeled fixture set in `tests/fixtures/agent-memory/` — 100 conversations × ~5 expected memories each (built in Phase 1).
**Eval scripts**: `tests/agent-memory/eval-recall.sh` (recall NDCG/MRR), `tests/agent-memory/eval-extraction.sh` (extraction P/R), `tests/agent-memory/bench-freshness.sh` (T0→T1 lag). All to be created in AM-1.5.
**Design doc**: See conversation `2026-04-25 — Chronik Agent Memory Design` for the full architectural rationale.

---

## Measurement Discipline

Every phase records **three classes of metrics**. Improving recall NDCG while degrading extraction precision or freshness is a regression.

**Recall quality** (per phase, on LongMemEval + custom fixtures):
- NDCG@10, P@5, MRR
- Per-category deltas (single-session vs multi-session, fact vs event vs instruction)
- Synthesis-mode quality (LLM-judge score on returned natural-language answers)

**Extraction quality** (per phase, on custom fixtures):
- Precision: % of extracted memories that match a labeled gold memory
- Recall: % of labeled gold memories that were extracted
- Type-classification accuracy (fact/event/instruction/task)
- Cite-source accuracy: % of memories whose `source_offsets` actually contain the claim
- Confidence calibration (Brier score)

**System metrics** (per phase):
- Ingest latency: `/memory/v1/ingest` p50 / p95 / p99
- Recall latency: `/memory/v1/recall` p50 / p95 / p99 — **with and without** synthesis
- Extraction lag: T0 (raw msg in `mem.raw.*` WAL) → T1 (typed memory recallable in `/_search`)
- Tokens per conversation (LLM extraction cost)
- Storage growth per 1k turns (WAL + Tantivy + HNSW + Parquet)

**Freshness must be measured after each phase** to ensure the LLM extractor doesn't blow the lag budget (target: p99 < 30s end-to-end T0→T1).

---

## Current State Analysis

Chronik already provides ~70% of an agent memory system at the substrate level:

**Already shipping**:
- Kafka-compatible ingestion with WAL durability
- Per-topic Tantivy index with hot text path (T0→T1 p50 ≈201ms, p99 ≈507ms — measured 2026-04-17)
- Per-topic HNSW vector index with hot vector path (T0→T2 p50 ≈18ms with mock embedder)
- DataFusion SQL over Parquet
- Unified API on port 6092: `/_search`, `/_vector`, `/_sql`, `/_query`
- Schema Registry (Confluent-compatible) for envelope validation
- Multi-node Raft cluster + zero-downtime node add/remove
- Log compaction (key-based, time-based, hybrid) — already in `chronik-wal/src/compaction.rs`
- `merge_search_responses()` dedup pattern in `query_router.rs:710-758`

**Missing for agent memory**:
- Structured extraction pipeline (raw conversation → typed memory)
- Memory envelope schemas (fact / event / instruction / task)
- Supersession semantics on top of compaction
- Agent-native REST surface (`/memory/v1/*`)
- Lifecycle controller (dedup, decay, summarization)
- Per-tenant namespace isolation + auth
- Provenance graph (memory → source offsets)

The roadmap below is **only the missing 30%** — every phase reuses existing Chronik primitives where possible.

### Key Lessons (from research and Cloudflare's design)

1. **Extraction is the whole game.** A retrieval system over bad memories is worse than no memory at all. Eval extraction quality from day one.

2. **Async extraction is non-negotiable.** Synchronous extraction in `/ingest` makes ingest latency hostage to LLM provider tail latency. Cloudflare also runs extraction async (at compaction). Lag budget < 30s p99.

3. **Compaction = supersession.** Kafka log compaction with stable keys gives idempotent, supersedeable memories for free. Don't reinvent it.

4. **Provenance must be first-class.** Every memory carries `source_offsets`. Without this, hallucinated memories cannot be detected and the system loses trust.

5. **Decay is a query-time score, not a write-time process.** Memories don't physically age; their relevance score does. No background sweeper needed.

6. **BYO LLM, BYO embedder.** Cloudflare bundles Workers AI; we cannot. The provider trait must support Anthropic, OpenAI, and local (Ollama / vLLM) from Phase 1.

7. **Per-namespace isolation maps cleanly to topic prefixes.** No need for row-level multi-tenancy gymnastics — Kafka topics already isolate.

---

## Phase 1: MVP — Ingest → Extract → Recall

**Expected outcome**: One demo namespace, end-to-end. 100-conversation fixture set with ≥70% top-3 recall precision, manual eval.
**Effort**: 3 weeks (1 Rust eng + 1 ML eng)
**Risk**: Low — all primitives exist; this is glue code + LLM prompts.

### AM-1.1: Topic & schema conventions

- [ ] Define memory envelope JSON schema (`memory_id`, `tenant_id`, `namespace`, `type`, `key`, `version`, `valid_from`, `valid_to`, `confidence`, `source.{topic,offsets,extractor}`, `tombstoned`, `body`)
- [ ] Register envelope in Schema Registry with `forward` compatibility mode
- [ ] Topic naming spec: `mem.raw.{tenant}.{agent}.{conversation}`, `mem.fact.{tenant}`, `mem.event.{tenant}`, `mem.instruction.{tenant}`, `mem.task.{tenant}`, `mem.task.current.{tenant}` (compacted view)
- [ ] Per-topic config templates in `examples/agent-memory/topic-configs/` — fact/instruction compacted with vector + text + columnar; event append-only with vector + text + columnar; task compacted current-state with text + columnar only
- [ ] Admin helper `chronik-server memory init-namespace --tenant X --agent Y` — creates the 5 topics with correct configs

**Files**: `crates/chronik-memory/src/schema.rs` (new crate), `examples/agent-memory/topic-configs/*.toml`, `crates/chronik-cli/src/memory.rs`

### AM-1.2: IngestSvc

- [ ] New crate `chronik-memory` with axum HTTP server module
- [ ] `POST /memory/v1/ingest` handler — accepts NDJSON or JSON array, writes to `mem.raw.{ns}` via internal Kafka produce path
- [ ] Idempotency: SHA-256 of `(namespace, role, content)` as default Kafka key when no `external_id` provided; in-memory LRU cache (5-min TTL) rejects exact duplicates
- [ ] `POST /memory/v1/remember` handler — direct write to typed topic, bypasses extraction
- [ ] `POST /memory/v1/forget` handler — emits null-value record to compacted topic
- [ ] Mount on Unified API port 6092 under `/memory/v1/*`
- [ ] Prometheus metrics: `memory_ingest_total{tenant,outcome}`, `memory_ingest_latency_seconds`

**Files**: `crates/chronik-memory/src/{ingest,remember,forget}.rs`, mount in `crates/chronik-server/src/unified_api/mod.rs`

### AM-1.3: Extractor v1 (rule pass + LLM pass, fact + event only)

- [ ] New crate `chronik-memory-extractor`, Kafka consumer using existing client patterns (group `memory-extractor-v1`)
- [ ] Rule-pass extractors:
  - Email regex → `fact{predicate=has_email}`
  - Phone regex → `fact{predicate=has_phone}`
  - URL regex → `fact{predicate=mentioned_url}`
  - Dates (chrono parser) → `event{ts=...}`
- [ ] LLM provider trait `EmbeddingExtractor` with implementations:
  - `AnthropicExtractor` (Claude Haiku 4.5 default)
  - `OpenAIExtractor` (gpt-4o-mini)
  - `OllamaExtractor` (Llama 3.1 8B local)
- [ ] Structured output enforced via JSON schema (Anthropic tool use / OpenAI function calling / llama.cpp grammar)
- [ ] Extraction prompt v1 (fact + event only, instruction + task in Phase 2)
- [ ] Verify: every extracted memory cites at least one offset within the consumed batch; reject if not
- [ ] Produce to `mem.fact.{tenant}` / `mem.event.{tenant}` with `acks=1`, idempotent keys
- [ ] Commit Kafka offset only after successful produce (at-least-once → effectively-once via compaction)
- [ ] Micro-batching: process 200 messages OR 5s window, whichever first
- [ ] Metrics: `memory_extraction_latency_seconds`, `memory_extraction_lag_seconds`, `memory_extraction_tokens_total{provider,kind}`, `memory_extraction_errors_total`

**Files**: `crates/chronik-memory-extractor/src/{main,rules,llm,prompts}.rs`

### AM-1.4: RecallSvc v1 (BM25 + vector, no synthesis)

- [ ] `POST /memory/v1/recall` handler in `chronik-memory` crate
- [ ] Parallel fan-out via existing `query_router.rs` patterns:
  - `/_search` against `mem.{type}.{tenant}` for each requested type
  - `/_vector/{topic}/search` against same topics
- [ ] RRF merger reusing the dedup logic from `merge_search_responses()` in `query_router.rs:710-758`; key dedup on `(namespace, key)` with `version` tiebreak
- [ ] Decay scoring at query time: `score *= exp(-(now - valid_from) / half_life(type))` with defaults fact=365d, event=30d, instruction=730d, task=7d
- [ ] Provenance: include `source.{topic,offsets,extractor}` in every result
- [ ] Latency targets: p50 < 200ms, p99 < 500ms (no synthesis)
- [ ] Metrics: `memory_recall_latency_seconds`, `memory_recall_results_total{type}`, `memory_recall_zero_hits_total`

**Files**: `crates/chronik-memory/src/recall.rs`

### AM-1.5: Eval harness

- [ ] Create `tests/fixtures/agent-memory/` with 100 labeled conversations
  - Each conversation: 10-30 turns of synthetic dialog
  - Each conversation: 3-8 gold memories with `(type, key, body, source_turn_indexes)`
  - Cover: real-estate, customer-support, personal-assistant, technical-help, multi-session
- [ ] Eval script `tests/agent-memory/eval-extraction.sh`:
  - Ingest each fixture, wait for extraction lag, fetch produced memories
  - Compute precision/recall against gold; type-classification accuracy; cite-source accuracy
- [ ] Eval script `tests/agent-memory/eval-recall.sh`:
  - For each fixture, run a set of recall queries with expected memory IDs
  - Compute NDCG@10, P@5, MRR
- [ ] Bench script `tests/agent-memory/bench-freshness.sh`:
  - Produce raw turn → poll `/memory/v1/recall` until memory appears → record T0→T1
  - Run N=100 probes, report min/p50/p95/p99
- [ ] Bench script `tests/agent-memory/bench-recall-latency.sh` — k6 against `/memory/v1/recall` at 100/500/1000 VUs
- [ ] Sample LongMemEval-style adapter (download dataset, convert to Chronik fixtures)

**Files**: `tests/fixtures/agent-memory/*.json`, `tests/agent-memory/*.sh`

### AM-1.6: Run eval

- [ ] Run extraction eval, record P/R/type-accuracy/cite-accuracy
- [ ] Run recall eval (manual top-3 inspection on 30 queries)
- [ ] Run freshness bench
- [ ] Run latency bench
- [ ] Document results in this file

**Phase 1 Results**: _Not yet run_
```
Recall quality (custom fixtures):
  NDCG@10:               _____
  P@5:                   _____
  MRR:                   _____
  Manual top-3 precision: _____ %  (target: >= 70%)

Extraction quality (custom fixtures):
  Precision:             _____ %
  Recall:                _____ %
  Type-classification:   _____ %
  Cite-source accuracy:  _____ %  (target: >= 95% — hallucination guard)

System:
  Ingest p50/p99:        ___/___ ms     (target: p50 < 10ms)
  Recall p50/p99:        ___/___ ms     (target: p50 < 200ms, p99 < 500ms)
  Extraction lag p99:    _____ s        (target: < 30s)
  Tokens / conversation: _____          (cost benchmark)
```

**Phase 1 exit criteria**:
- All AM-1.x tasks checked
- Manual top-3 recall precision ≥ 70%
- Cite-source accuracy ≥ 95%
- Extraction lag p99 < 30s
- Ingest p99 < 50ms

---

## Phase 2: Production Core

**Expected outcome**: Multi-tenant production deployment. All four memory types. Lifecycle running. Hybrid ranking with full RRF.
**Effort**: 6 weeks
**Risk**: Medium — multi-tenancy and lifecycle introduce new failure modes.
**Depends on**: Phase 1 complete.

### AM-2.1: All four memory types

- [ ] Instruction schema + extractor prompt — keyed by `{scope}|{trigger}|sha256(rule)`
- [ ] Task schema + extractor prompt — emits `task` with `state=open`
- [ ] Task state-machine: produce transitions to `mem.task.{tenant}` keyed by `task_id`; consumer materializes `mem.task.current.{tenant}` (compacted, latest state per `task_id`)
- [ ] Update extraction prompt to emit all four types in a single LLM call (single prompt, structured output with arrays per type)
- [ ] Per-type extraction confidence calibration table (offline-tuned multiplier per `(extractor_version, type)`)

**Files**: `crates/chronik-memory-extractor/src/prompts/v2.rs`, `crates/chronik-memory/src/types/{task_state,instruction}.rs`

### AM-2.2: Compaction-based supersession

- [ ] Verify per-topic compaction config is honored end-to-end (produce N updates with same key, confirm only latest survives after compaction tick)
- [ ] Add `version` field auto-increment on supersession (extractor reads existing memory by key, increments version)
- [ ] Tombstone helper in `forget` endpoint emits null-value record correctly
- [ ] Integration test: ingest contradicting fact 10 times, recall returns latest only

**Files**: `crates/chronik-wal/src/compaction.rs` (verify), `crates/chronik-memory/src/forget.rs`

### AM-2.3: Lifecycle controller

- [ ] New service `chronik-memory-lifecycle` — consumer + scheduler
- [ ] Semantic dedup consumer on `mem.fact.{tenant}`:
  - For each new fact, compute embedding (reuse hot vector embedder)
  - If existing fact with same `(subject, predicate)` and >0.97 cosine similarity: drop new write OR supersede if new confidence is higher
  - Emit `memory_dedup_suppressed_total{tenant}` metric
- [ ] Decay config: per-namespace half-life overrides via `mem.config.{tenant}` topic
- [ ] No background sweeper for decay — scoring is computed at recall time (already in AM-1.4)
- [ ] Operator endpoint `POST /memory/v1/compact` triggers a one-shot dedup pass, returns job ID

**Files**: `crates/chronik-memory-lifecycle/src/{main,dedup,config}.rs`

### AM-2.4: Hybrid ranking — full RRF

- [ ] Type-weighted RRF: `score = Σ_c w_c * w_type * 1/(k+rank) * decay * confidence`
  - Defaults: `w_bm25=1.0, w_vector=1.0, w_key_match=2.0, w_hyde=0.5, w_sql=1.5`
  - Type weights: `fact=1.0, instruction=1.2, event=0.8, task=1.0`
  - `k=60` (RRF constant)
- [ ] Key-match channel: extract subject from query (small LLM call OR rule-based NER), if matches a known fact subject, exact lookup via `/_sql`
- [ ] HyDE channel (optional, behind `recall.hyde=true` flag): cheap LLM generates hypothetical answer, embed, vector search
- [ ] Per-tenant ranking config in `mem.config.{tenant}` (weights, half-lives, k, channel toggles)
- [ ] Recall request supports `channels: ["bm25","vector","key_match","hyde","sql"]` to selectively disable

**Files**: `crates/chronik-memory/src/recall.rs` (extend), `crates/chronik-memory/src/ranking.rs` (new)

### AM-2.5: Multi-tenancy

- [ ] API-key auth middleware: `X-Tenant-Id` + `X-API-Key` validated against `mem.tenants` topic (compacted, key=tenant_id)
- [ ] Namespace authorization: API key bound to one or more `(tenant, namespace_pattern)` tuples
- [ ] Per-tenant rate limits on `/memory/v1/{ingest,recall}` (token bucket, configurable)
- [ ] Per-tenant Prometheus labels (cardinality concern — cap at top-N tenants, fold rest into `tenant=other`)
- [ ] Quota enforcement: ingest msgs/sec, recall qps, storage GB (computed from topic byte count)
- [ ] Reject writes/reads to namespaces the API key doesn't own → 403 with structured JSON body

**Files**: `crates/chronik-memory/src/{auth,quota}.rs`

### AM-2.6: Provenance & audit

- [ ] Verify `source_offsets` round-trip: ingest → extract → recall → fetch raw turn from offset → equal
- [ ] Add `GET /memory/v1/{memory_id}/source` endpoint — returns the actual raw turns referenced
- [ ] Audit log topic `mem.audit.{tenant}` — every recall and write emits an audit record (tenant, namespace, op, query, result_ids, ts)
- [ ] Audit log retention: configurable, default 90 days

**Files**: `crates/chronik-memory/src/{provenance,audit}.rs`

### AM-2.7: Eval (extraction quality + recall NDCG + LongMemEval)

- [ ] Add LongMemEval adapter (download dataset, convert questions to Chronik fixtures)
- [ ] Run full eval: extraction P/R, recall NDCG@10 on custom + LongMemEval, freshness, latency
- [ ] Two extractor versions running side-by-side (v1 fact+event vs v2 all-types) → A/B in recall
- [ ] Document results

**Phase 2 Results**: _Not yet run_
```
Recall quality:
  Custom NDCG@10:        _____   (was Phase 1 result)
  LongMemEval NDCG@10:   _____   (target: >= 0.70)
  P@5:                   _____
  MRR:                   _____

Extraction quality (all 4 types):
  Precision:             _____ %  (target: >= 80%)
  Recall:                _____ %
  Type-classification:   _____ %  (target: >= 90%)
  Confidence Brier:      _____    (lower = better calibrated)

System:
  Ingest p99:            ___ ms   (target: < 50ms)
  Recall p99 (no synth): ___ ms   (target: < 500ms)
  Extraction lag p99:    ___ s    (target: < 30s)
  Dedup suppression:     ___ %    (sanity: 5-15% expected on duplicate-heavy fixtures)
  Storage / 1k turns:    ___ MB
```

**Phase 2 exit criteria**:
- LongMemEval NDCG@10 ≥ 0.70
- All four memory types extracting with type-classification ≥ 90%
- Multi-tenant: 10+ tenants in shared cluster, no cross-tenant leaks (audit-log proven)
- Recall p99 < 500ms

---

## Phase 3: Advanced Intelligence

**Expected outcome**: Differentiation features that Cloudflare cannot easily match — summarization, conflict detection, agent feedback loop, provenance graph, local-only deployments.
**Effort**: 6 weeks
**Risk**: Medium — feature breadth, but each feature is independent.
**Depends on**: Phase 2 complete and stable.

### AM-3.1: Summarization rollups

- [ ] Rollup consumer on `mem.event.{tenant}` — windowed by `(actor, object, day)` or `(actor, object, count=10)`
- [ ] Trigger v1: count-based (10 events for same `(actor, object)`)
- [ ] Trigger v2 (optional): LLM classifier decides if rollup is warranted
- [ ] Synthesizes a `summary` memory: new memory type or `fact` with `predicate=summary_of`
- [ ] Topic: `mem.summary.{tenant}` (compacted, key=`summary:{actor}:{object}:{period}`)
- [ ] Recall: optionally bias ranking toward summaries when query is broad (`include_summaries=true`)
- [ ] Original events retained (audit) — summaries never delete originals

**Files**: `crates/chronik-memory-lifecycle/src/summarize.rs`

### AM-3.2: Conflict detection

- [ ] On extraction, the extractor checks for existing fact with same `(subject, predicate)` but contradicting `object`
- [ ] If contradiction found → emit new fact with `polarity=conflicting` and metadata pointing to the contradicted memory
- [ ] Recall surfaces conflicts: `score *= conflict_penalty` (default 0.5) until resolved
- [ ] Resolution: agent (or human) calls `POST /memory/v1/{id}/resolve` with chosen version, others are tombstoned
- [ ] Per-tenant policy: `conflict.strategy = surface_both | latest_wins | highest_confidence_wins`

**Files**: `crates/chronik-memory-extractor/src/conflict.rs`, `crates/chronik-memory/src/resolve.rs`

### AM-3.3: Agent feedback loop

- [ ] `POST /memory/v1/feedback` endpoint — agent reports `(memory_id, recall_query, useful: bool, used_in_response: bool)`
- [ ] Emit to `mem.feedback.{tenant}` topic
- [ ] Offline reranker training pipeline (weekly cron):
  - Read `mem.feedback.{tenant}` → build (query, memory, useful) triples
  - Train per-tenant linear reranker on RRF channel scores + decay + confidence + type
  - Deploy as a per-tenant config blob applied at recall time
- [ ] Guard: only deploy a new reranker if offline NDCG improves AND no per-category regressions

**Files**: `crates/chronik-memory/src/feedback.rs`, `crates/chronik-memory-lifecycle/src/reranker_train.rs`

### AM-3.4: Provenance graph

- [ ] `GET /memory/v1/{id}/lineage` — walks back through `source_offsets` to raw turns, and forward through summaries/supersessions
- [ ] Returns a DAG: ancestors (raw turns + earlier versions) and descendants (summaries that cited this memory)
- [ ] Use case: agent can show user "I remember this because you said X on date Y"

**Files**: `crates/chronik-memory/src/lineage.rs`

### AM-3.5: Cross-namespace memory (team memory)

- [ ] Namespace ACL model: per-namespace `read_grants: [tenant_or_namespace]`
- [ ] Recall fan-out: if request has `include_grants=true`, expand search across granted namespaces
- [ ] Audit log marks cross-namespace recalls explicitly
- [ ] Use case: shared team memory pool that multiple agents can read

**Files**: `crates/chronik-memory/src/acl.rs`

### AM-3.6: Streaming recall (SSE)

- [ ] `GET /memory/v1/recall/stream` — SSE endpoint that emits results as channels complete
- [ ] First event: cheapest channel results (key_match if hit, otherwise BM25)
- [ ] Subsequent events: vector, hyde, synthesis
- [ ] Use case: agent UX where the model can start generating with partial context

**Files**: `crates/chronik-memory/src/recall_stream.rs`

### AM-3.7: Local embeddings + local LLM

- [ ] Local embedding provider: BGE-small or mxbai-embed-large via candle (in-process) or external HTTP server
- [ ] Local LLM provider: Ollama and vLLM HTTP clients in `chronik-memory-extractor`
- [ ] Eval parity: same custom fixtures + LongMemEval, compare local vs cloud extraction quality
- [ ] Document deployment topology: GPU node for embeddings, CPU node for Chronik core, optional local LLM node
- [ ] Performance budget: local embed < 50ms p99 batch, local LLM extract < 5s p99 per batch

**Files**: `crates/chronik-embeddings/src/providers/{bge,mxbai}.rs` (extend existing crate), `crates/chronik-memory-extractor/src/llm/{ollama,vllm}.rs`

### AM-3.8: Run eval

**Phase 3 Results**: _Not yet run_
```
Quality:
  Custom NDCG@10:           _____   (was Phase 2)
  LongMemEval NDCG@10:      _____   (target: maintain >= 0.70 with summarization on)
  Conflict detection F1:    _____   (custom contradiction fixtures)
  Local-only NDCG@10:       _____   (target: within 5% of cloud)

Cost:
  Tokens / conversation:    _____   (target: 30% reduction via summarization rollup)
  Embedding cost / 1k mem:  $_____  (cloud) vs $0 (local)

System:
  Recall p99 (with stream): ___ ms first-event, ___ ms full
  Reranker training:        weekly, per-tenant, no regressions deployed
```

**Phase 3 exit criteria**:
- Summarization reduces effective recall context size by ≥ 30% on multi-session fixtures with no NDCG loss
- Local-only deployment produces ≥ 95% of cloud extraction quality
- One production customer with ≥ 1M memories live

---

## Phase 4: Platformization

**Expected outcome**: Self-serve SaaS or licensed appliance. Auth, billing, SDKs, console, multi-region, compliance.
**Effort**: 8 weeks
**Risk**: Low-Medium — well-understood platform engineering, but breadth is high.
**Depends on**: Phase 3 stable in production.

### AM-4.1: Auth + RBAC

- [ ] OAuth2 / OIDC provider integration (auth.js / Ory / Auth0)
- [ ] API-key issuance/rotation via console
- [ ] RBAC roles: `admin`, `writer`, `reader`, `auditor`
- [ ] Namespace-scoped role bindings stored in `mem.acl.{tenant}` topic

### AM-4.2: Billing telemetry

- [ ] Per-tenant counters in Prometheus: ingest_msgs, recall_qps, storage_bytes, extraction_tokens
- [ ] Hourly rollup → billing topic `billing.tenant.{tenant_id}`
- [ ] Stripe metering integration (or licensed appliance: emit to customer's billing system via webhook)
- [ ] Tenant dashboard: usage + cost projection

### AM-4.3: SDKs

- [ ] Python SDK (`chronik-memory-py`) — thin async client, retries, OpenTelemetry tracing
- [ ] TypeScript SDK (`@chronik/memory`) — same surface, browser + node
- [ ] Rust SDK (`chronik-memory-client`) — for embedded / agent harness use cases
- [ ] All SDKs cover: ingest, recall, remember, forget, list, feedback, lineage
- [ ] Reference integrations: LangGraph node, CrewAI tool, Mastra plugin, MCP server, OpenAI tool spec

### AM-4.4: Web console

- [ ] Next.js app, deployed alongside cluster
- [ ] Memory browser: filter by namespace/type/key/time, view body + provenance
- [ ] Eval dashboard: per-tenant recall quality trends from `mem.feedback.{tenant}`
- [ ] Namespace admin: ACLs, quotas, half-life config, ranking weights
- [ ] Audit log viewer
- [ ] Live extraction tail (SSE on `mem.raw.{ns}` + `mem.{type}.{ns}` outputs)

### AM-4.5: Backup + restore

- [ ] Per-namespace snapshot using existing Chronik DR (S3 metadata uploader + segment archive)
- [ ] Restore flow: spin up empty namespace, replay from S3 snapshot
- [ ] Automated weekly snapshots, configurable retention

### AM-4.6: Multi-region

- [ ] Regional pinning: namespaces declared with primary region (`region: eu-west-1`)
- [ ] Cross-region read replicas via existing Raft cluster (followers in other regions)
- [ ] Recall routes to nearest replica with bounded staleness (configurable)
- [ ] Compliance: namespace cannot be replicated across regions if `compliance.geo_lock=true`

### AM-4.7: Compliance posture

- [ ] PII tagging in extractor: every fact gets `pii_class: none|low|medium|high`
- [ ] Redaction API: `POST /memory/v1/redact` with regex or `subject` filter
- [ ] Right-to-erasure: `DELETE /memory/v1/subject/{subject_id}` tombstones all memories with that subject across all types
- [ ] Audit log immutability: `mem.audit.{tenant}` is append-only with no compaction
- [ ] SOC 2 / GDPR / HIPAA documentation pack (assumes underlying Chronik deployment is compliant)

### AM-4.8: Reference docs + integrations

- [ ] Public docs site at `docs.chronik.io/memory` — quickstart, API reference, deployment guide
- [ ] Integration guides for LangGraph, CrewAI, Mastra, MCP, OpenAI tools, Anthropic computer use
- [ ] Tutorial: "Real-estate WhatsApp assistant in 50 lines" (the use case from the design doc)
- [ ] Tutorial: "Migrating from Cloudflare Agent Memory" — schema mapping + ingest replay

**Phase 4 Results**: _Not yet run_
```
Platform readiness:
  Tenants supported:        ___    (target: 1000+ multi-tenant)
  SDK languages:            Python, TypeScript, Rust
  Reference integrations:   LangGraph, CrewAI, Mastra, MCP, OpenAI
  Compliance pack:          SOC 2 / GDPR / HIPAA documented

Production:
  Customers in production:  ___
  Total memories under management: ___
  Cluster footprint:        ___ nodes, ___ regions
```

**Phase 4 exit criteria**:
- Public-facing docs live
- Three reference integrations shipped
- One self-serve customer onboarded without engineering involvement

---

## Summary: Expected Trajectory

| After | Recall NDCG@10 (LongMemEval) | Extraction P/R | Notes |
|-------|------------------------------|----------------|-------|
| Phase 1 (MVP) | _not measured_ — manual top-3 ≥ 70% | P ≥ 70%, R ≥ 60% | Fact + event only, single tenant |
| Phase 2 (Production Core) | **≥ 0.70** | P ≥ 80%, R ≥ 70% | All four types, multi-tenant, hybrid RRF |
| Phase 3 (Advanced) | maintain ≥ 0.70 | maintain or improve | Summarization, conflict detection, local-only |
| Phase 4 (Platform) | maintain ≥ 0.70 | maintain | Self-serve SaaS-ready |

**Honest assessment**: The unknowns are extraction quality (LLM-dependent — prompts and model choice will dominate Phase 1-2 results) and how well Tantivy hot-text + HNSW hot-vector hold up under per-tenant fan-out at 1000+ tenants (will be tested in Phase 4). NDCG ≥ 0.70 on LongMemEval is the published cross-encoder territory; achieving it with our hybrid RRF over good extraction is plausible but not guaranteed.

**What we are NOT building** (explicitly excluded):
- A new vector database (HNSW already exists)
- A foundation model (BYO LLM)
- A managed agent framework (we are the memory layer for LangGraph / CrewAI / Mastra, not a competitor)
- A custom query DSL (`MemoryQL` is a tarpit — SQL + REST is enough)
- Per-message embedding of every chat turn (embed extracted memories, not every line)
- Synchronous extraction in `/ingest` (latency disaster)
- Cross-tenant ML training (privacy / compliance landmine)

---

## Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Extraction quality silently regresses with prompt/model changes | High | High | Strict eval CI (AM-1.5, AM-2.7), shadow-mode A/B for new extractor versions, per-tenant "rebuild memories" admin op |
| Embedding cost blowup at scale | Medium | High | Hot-vector reuse already in Chronik; AM-3.7 local embeddings; per-tenant cost dashboard in AM-4.2 |
| Compaction lag → stale recall returns superseded memories | Medium | Medium | Query-time dedup by `(namespace, key)` with version tiebreak — AM-1.4, must be enforced |
| Multi-tenant noisy neighbor on extraction | Medium | Medium | AM-2.5 per-tenant rate limits + separate consumer groups for premium tiers |
| Schema evolution after tenants accumulate millions of memories | Low | High | Schema Registry forward-compatible from day one (AM-1.1); envelope changes additive only |
| Hot-text/hot-vector index RAM blowup with N tenants × M namespaces | Medium | Medium | Per-namespace `hot.enabled` flag (already in Chronik); AM-2.5 quota enforcement; observability on hot-index size |
| LLM provider outage stalls extraction | High | Low | Async extraction = ingest unaffected; AM-1.3 multi-provider trait allows failover; lag spike clears on recovery |
| Hallucinated memories without source citations | Medium | High | AM-1.3 cite-source verification (reject extractions without offsets in batch); cite-source eval metric tracked every phase |

---

## References

1. **Cloudflare Agent Memory** (2026-04, blog post): Session-centric, retrieval-based, four memory types (facts/events/instructions/tasks), Llama 4 Scout for classification, Nemotron 3 for synthesis, RRF over five channels (BM25 + key + raw + direct vector + HyDE vector). https://blog.cloudflare.com/introducing-agent-memory

2. **LongMemEval** (2024): Long-form conversational memory benchmark, 500 questions across 5 reasoning categories. Primary recall-quality target. https://github.com/xiaowu0162/LongMemEval

3. **MemGPT / Letta**: OS-style memory hierarchy with eviction. Influenced our hot/cold tiering thinking but Chronik already provides the tiering for free. https://github.com/letta-ai/letta

4. **Mem0**: Production memory layer, similar four-type taxonomy. Validates the schema design but uses Postgres + pgvector (not event-native). https://github.com/mem0ai/mem0

5. **Chronik Search Quality Roadmap** (`docs/ROADMAP_SEARCH_QUALITY.md`): Phase 1 stemming + dedup fix; same RRF infrastructure used by recall.

6. **Chronik Hot Path Roadmap** (`docs/ROADMAP_HOT_PATH.md`): Hot text + hot vector freshness numbers reused as recall freshness baselines.

7. **Chronik Vector Optimization** (`docs/ROADMAP_VECTOR_OPTIMIZATION.md`): HNSW tuning history; `chronik-embeddings/src/reranker.rs` reused for AM-3.3.

8. **RRF (Reciprocal Rank Fusion)**: Cormack, Clarke, Buettcher (2009). Standard channel-fusion algorithm; `k=60` is the canonical constant.

9. **Anthropic structured output / OpenAI function calling**: Used for LLM-pass extraction with schema enforcement. Lower hallucination rate than free-form generation.

10. **Confluent Schema Registry compatibility modes**: `forward` mode chosen for envelope evolution — old consumers tolerate new optional fields.
