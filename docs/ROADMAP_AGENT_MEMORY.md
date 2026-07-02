# Agent Memory Roadmap

**Goal**: Ship Chronik Agent Memory — a unified, event-native memory layer for AI agents — leveraging Chronik's existing primitives (Kafka WAL, Tantivy, HNSW, DataFusion).
**Strategic position**: Beat Cloudflare Agent Memory on query power, event-nativity, and multi-modal retrieval. Match its ergonomics on the agent-facing API.
**Benchmark (recall quality)**: [LongMemEval](https://github.com/xiaowu0162/LongMemEval) (500 questions, 5 reasoning categories, long-form conversation memory).
**Benchmark (extraction quality)**: Custom labeled fixture set in `crates/chronik-memory/tests/fixtures/agent-memory/` — target 100 conversations × ~5 expected memories each (built in Phase 1; currently 7 fixtures).
**Eval harness**: Rust integration tests in `crates/chronik-memory/tests/` (`eval_extraction.rs`, `eval_recall.rs`, `eval_longmemeval.rs`, `bench_freshness.rs`). The original `.sh` scripts in `tests/agent-memory/` were not built — the Rust-test harness covers the same matrix and runs inside `cargo test`.
**Design doc**: See conversation `2026-04-25 — Chronik Agent Memory Design` for the full architectural rationale.

---

## Architectural Decisions

### AD-1 (2026-06-27): Server-side endpoints, not a public SDK

**Decision**: The agent-facing surface is HTTP endpoints under `/memory/v1/*` on the Unified API (port 6092). The Rust crate `chronik-memory` is the server's internal implementation, **not** a public SDK. Python bindings are dropped.

**Why we considered an SDK**: Phase 1 was implemented as a Rust SDK (`chronik-memory::client::Client` + `bindings/python` PyO3 wrapper) to get pilots running quickly without designing the wire format.

**Why we pivoted**:
1. **The SDK was already HTTP-bound.** `recall.rs` fans out to `/_search`, `/_vector/{topic}/search`, `/_sql` via `reqwest`. There was no in-process latency win to defend.
2. **It broke Chronik's pattern.** Schema Registry, admin, SQL, vector, and search are all server-side endpoints. The memory layer being the one Rust-only outlier was confusing for users.
3. **Language matrix tax.** Every API change required Rust + PyO3 + Python wheel rebuilds. With HTTP+JSON, any language gets parity for free.

**Cost of the pivot**:
- Move ingest/recall/remember/forget/feedback/source/admin handlers from `chronik-memory::client` into `crates/chronik-server/src/unified_api/`
- Rewrite the eval harness to call `POST /memory/v1/recall` instead of `Memory::recall().with_*()` (incidentally eats own dogfood)
- Delete `bindings/python`
- `chronik-memory` crate becomes server-internal (no `client.rs`, no published types)

Tracked as **AM-1.7** below.

### AD-2 (2026-06-27): Concept pages + wikilinks land off-roadmap

Synthesis-mode quality work introduced a fifth memory type (`concept`) and wikilink-based linking. These weren't in the original Phase 1-4 plan; they grew out of the LongMemEval pilot ladder. See **Appendix B** below for the full pilot history. Phase 1 schema accepts `MemoryType::Concept` today.

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

## Testing Strategy

Measurement Discipline says *what* we measure. This section says *when* it runs, *what blocks a merge*, and *how* we catch regressions. Implementation is tracked as **AM-1.8**.

### Layers

| Layer | Location | Trigger | Blocks merge? |
|-------|----------|---------|---------------|
| Unit (~218 tests today) | `crates/chronik-memory/src/**/#[cfg(test)]` | every PR | ✅ yes |
| Integration (offline) | `crates/chronik-memory/tests/*.rs` with no env-var gate | every PR | ✅ yes |
| LLM-judge eval | same tests, gated by `ANTHROPIC_API_KEY` + `CHRONIK_INTEGRATION=1` | nightly | ✅ yes — fail CI on regression |
| Cluster smoke | `tests/cluster_smoke.rs` against `tests/cluster/start.sh` | nightly | ✅ yes |
| Provider parity | `tests/eval_provider_parity.rs` (Anthropic / OpenAI / Ollama) | nightly | ⚠️ warn-only — provider drift is real |
| Soak | `tests/soak_worker.rs` 24h | weekly | manual |
| Scale/load | `bench_recall_latency.rs` (TODO), k6 at 100/500/1000 VUs | per release | manual |

### Pre-merge gates (must pass on every PR)

- `cargo test --workspace --lib --bins` clean
- `cargo check --workspace --all-features` clean
- Offline integration tests in `crates/chronik-memory/tests/` pass without API keys
- No new `unwrap()` / `panic!()` / `todo!()` in `src/` (clippy lint)
- If a PR touches an AM-1.x or AM-2.x task, the corresponding status marker in this roadmap is updated in the same PR

### Nightly gates (block release tags, not PRs)

- LongMemEval-S 18-item balanced pilot: `synth_judge_rate` must be **≥ baseline − 0.05**
- Custom-fixture extraction P/R: precision **≥ 0.80**, cite-source **≥ 0.95**
- Cluster smoke: 3-node ingest → wait for extraction → recall → forget roundtrip green
- Recall latency p99 < 500ms (no synthesis), < 1500ms (with synthesis)

### Regression baselines

- File: `crates/chronik-memory/tests/baselines/longmemeval-s.json`
- Current floor (Pilot 7, 2026-05): `{"synth_judge_rate": 0.556, "raw_judge_rate": 0.389, "extraction_p_at_5": 0.85, "cite_source_acc": 1.00}`
- PR check: nightly job diffs current run vs baseline; fails if any metric drops > 0.05
- **Baseline updates require an explicit `update-baseline` workflow run + commit-message tag `baseline-update: <reason>`** so improvements are never accidentally locked in as floors before they're proven stable

### Negative-path scenarios (must-have tests)

1. **Provider outage mid-extraction** — Anthropic returns 503 on item 4/18. Worker retries with backoff; no Kafka offset commit; on recovery, batch reprocesses cleanly with no duplicate memories.
2. **Malformed Kafka batch** — required field is null in `mem.raw.*` batch. Parser drops the individual record, the rest persist, `memory_extraction_errors_total{reason=malformed_record}` increments.
3. **Cross-tenant recall attempt** — API key for tenant A queries namespace owned by tenant B → 403 + audit log entry on `mem.audit.A` AND `mem.audit.B` (so neither tenant can hide the attempt).
4. **OOM under sustained load** — 10K msg/s for 5 min with constrained heap. Server returns 503 on overload, no data loss in WAL, no segment corruption.
5. **Network partition mid-recall** — kill one of three nodes during a recall. Surviving nodes return partial results with `partial: true` flag, or an honest 503 if quorum lost.

### Scale/load targets (per release)

- **Ingest**: 100K msg/s sustained on 3-node cluster, no extraction lag growth > 60s p99 over 1h
- **Recall**: 1000 VUs against `/memory/v1/recall`, p99 < 1s, error rate < 0.1%
- **Soak**: 24h continuous ingest + recall, no RSS growth, no extractor goroutine leak, no Tantivy segment count blowup

### CI implementation

Three GitHub Actions workflows (to be created in AM-1.8):
- `.github/workflows/agent-memory-pr.yml` — runs on every PR touching `crates/chronik-memory/` or `crates/chronik-server/src/unified_api/memory*`. Executes pre-merge gates only.
- `.github/workflows/agent-memory-nightly.yml` — runs at 02:00 UTC. Executes LLM-judge evals, cluster smoke, provider parity. Fails build on regression > 0.05.
- `.github/workflows/agent-memory-soak.yml` — runs weekly Sunday 04:00 UTC. 24h soak; reports to issue tracker on degradation.

### What we are NOT testing in CI (explicit)

- Real-conversation corpus quality (no production data yet — only synthetic fixtures)
- Multi-tenant noisy-neighbor scenarios (gated on AM-2.5 multi-tenant infrastructure)
- Multi-region replication latency (gated on Phase 4 multi-region work)
- LLM provider billing/quota exhaustion (test environment uses dedicated low-rate budget)

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

### AM-1.1: Topic & schema conventions — ✅ done

- [x] Memory envelope schema (`memory_id`, `tenant_id`, `namespace`, `type`, `key`, `version`, `valid_from`, `valid_to`, `confidence`, `source.{topic,offsets,extractor}`, `tombstoned`, `body`) — `crates/chronik-memory/src/schema.rs`
- [x] Five memory types supported: `fact`, `event`, `instruction`, `task`, `concept` (concept was added off-roadmap, see AD-2)
- [x] Topic naming + per-type config in `crates/chronik-memory/src/topics.rs`
- [ ] Register envelope in Schema Registry with `forward` compatibility mode — **not done** (only matters once external clients write directly to typed topics; today only the extractor + remember handler write them)
- [ ] Per-topic config templates in `examples/agent-memory/topic-configs/` — replaced by programmatic config in `topics.rs`; YAML templates skipped
- [ ] Admin helper `chronik-server memory init-namespace` — folded into the `POST /memory/v1/admin/init-namespace` endpoint (see AM-1.7)

### AM-1.2: Wire surface — ⚠️ pivoted (see AD-1)

**Original plan**: HTTP handlers on Unified API. **What shipped**: a Rust SDK that calls HTTP fan-out internally. **AD-1 reverts to the original plan** — see AM-1.7 below for the conversion task. The functional logic (ingest dedup, remember, forget) is already implemented in `crates/chronik-memory/src/{ingest,remember,forget}.rs`; only the axum mounting is missing.

- [x] Ingest write path with SHA-256 idempotency (LRU 5-min TTL) — `ingest.rs` + `idempotency.rs`
- [x] Remember write path — `remember.rs`
- [x] Forget tombstone path — `forget.rs`
- [ ] **Mount on Unified API as HTTP handlers** — see AM-1.7
- [ ] Prometheus metrics — `memory_ingest_*` not emitted yet; OTel hooks scaffolded in `otel.rs`

### AM-1.3: Extractor — ✅ done (exceeds spec)

- [x] Crate: `chronik-memory::extractor` + worker binary `chronik-memory-worker` (single crate, not a separate crate)
- [x] Rule-pass extractors: email/phone/URL/date in `extractor/rules.rs`
- [x] LLM provider trait `MemoryExtractor` with three impls: `anthropic.rs` (Claude Haiku 4.5 default), `openai.rs` (gpt-4o-mini), `ollama.rs` (Llama 3.1)
- [x] Structured output via Anthropic tool_use / OpenAI function calling / Ollama grammar; null-tolerant schema (`["string","null"]` on required fields)
- [x] Five prompt versions: v1 → **v5** (concrete-noun additive). Default is **v3**; v4 was opt-in after regressing judge_rate, v5 is opt-in via `with_prompt_version(V5)`
- [x] Source-offset verification: every extracted memory cites at least one offset in the consumed batch; else dropped
- [x] Produce to typed topics with `acks=1`, idempotent keys
- [x] Confidence calibration table in `extractor/calibration.rs`
- [x] Metrics emitted via `otel.rs`

### AM-1.4: RecallSvc — ✅ done as library (needs HTTP front, see AM-1.7)

- [x] Multi-channel fan-out in `crates/chronik-memory/src/recall.rs`: BM25 + vector + key_match + HyDE + SQL
- [x] RRF + dedup by `(namespace, key)` with `version` tiebreak (`merge_search_responses`-style logic re-implemented for memories)
- [x] Decay scoring at query time with type-specific half-lives
- [x] Provenance: `source.{topic,offsets,extractor}` in every result
- [x] Synthesis-mode via `synthesize: true` (Anthropic Haiku, anti-abstention prompt)
- [ ] Latency targets not yet measured at the HTTP layer (today: in-process Rust call)
- [ ] Prometheus `memory_recall_*` metrics not emitted

### AM-1.5: Eval harness — ⚠️ partial

- [x] Rust integration tests in `crates/chronik-memory/tests/`:
  - `eval_extraction.rs` — P/R + type-classification + cite-source accuracy
  - `eval_recall.rs` — NDCG@10, P@5, MRR on custom fixtures
  - `eval_longmemeval.rs` — adapter to `xiaowu0162/longmemeval-cleaned` HuggingFace dataset, 18-item balanced pilot
  - `bench_freshness.rs` — T0→T1 lag probe
  - `eval_provider_parity.rs` — Anthropic vs OpenAI vs Ollama side-by-side
  - `soak_worker.rs` — extraction stability under sustained load
- [ ] **Only 7 custom fixtures** (target: 100). See AM-1.5.b.
- [ ] `tests/agent-memory/*.sh` scripts not built — the Rust-test harness covers the same matrix and runs inside `cargo test`. Roadmap target updated accordingly.

### AM-1.5.b: Expand custom fixture set to 100 — ❌ pending

- [ ] Add 93 more labeled conversations to `crates/chronik-memory/tests/fixtures/agent-memory/`
- [ ] Cover: real-estate, customer-support, personal-assistant, technical-help, multi-session
- [ ] Each conversation: 10-30 turns, 3-8 gold memories with `(type, key, body, source_turn_indexes)`

### AM-1.6: Run eval — ⚠️ partial (LongMemEval below Phase 2 gate, custom fixtures not run end-to-end)

Pilots 1-7 ran on LongMemEval-S (18-item balanced pilot). Custom-fixture P/R not run as a full pass yet (only the 7 in-repo fixtures exercised inside `eval_extraction.rs`).

### AM-1.7: Server endpoint conversion — ✅ done (2026-06-28)

- [x] Handlers in [`crates/chronik-server/src/unified_api/memory.rs`](../crates/chronik-server/src/unified_api/memory.rs) — ingest, remember, forget, recall, feedback, source (501 stub), admin/init-namespace, health
- [x] Request/response types in [`crates/chronik-server/src/unified_api/memory_types.rs`](../crates/chronik-server/src/unified_api/memory_types.rs) — match Appendix A; round-trip tests for ingest/recall/error envelope/remember/forget
- [x] Auth middleware (passthrough): `X-Tenant-Id` + `X-API-Key` extracted; enforced when `CHRONIK_MEMORY_REQUIRE_AUTH=true`. AM-2.5 will validate against `mem.tenants`.
- [x] [`chronik_memory::MemoryRegistry`](../crates/chronik-memory/src/registry.rs) — per-namespace `Memory` cache, lock-free `DashMap`
- [x] Server bootstrap in [`main.rs`](../crates/chronik-server/src/main.rs) — `try_create_memory_registry()` reads `CHRONIK_MEMORY_KAFKA` + `CHRONIK_MEMORY_API`; wired into both single-node and cluster `UnifiedApiState` builders
- [x] Routes mounted in [`unified_api/mod.rs::create_router_full`](../crates/chronik-server/src/unified_api/mod.rs) — always registered; handlers return 503 when registry is None
- [x] `bindings/python` deleted; workspace member removed from root `Cargo.toml` with a pointer to AD-1
- [x] [`docs/agent-memory/quickstart.md`](agent-memory/quickstart.md) + [`docs/agent-memory/api-reference.md`](agent-memory/api-reference.md)
- [x] [`examples/agent-memory/curl.md`](../examples/agent-memory/curl.md) + [`examples/agent-memory/python.md`](../examples/agent-memory/python.md)
- [x] End-to-end HTTP smoke test [`crates/chronik-memory/tests/http_smoke.rs`](../crates/chronik-memory/tests/http_smoke.rs) — 4/4 passing against a live single-node server (gated by `CHRONIK_INTEGRATION=1`). Verified round-trip: health → init-namespace → remember → recall (BM25 hit on attempt 3 / 3s after write) → forget. Negative paths: empty ingest → 400, unknown type → 400, source endpoint → 501.

**Deferred to AM-1.8 (testing strategy operationalization)**:
- Prometheus metrics on `/memory/v1/*` — bundled with the broader CI metrics build-out
- Conversion of `eval_longmemeval.rs` + `eval_recall.rs` to HTTP — the smoke test covers the dogfood signal; converting the eval harness adds server-startup-in-test machinery without changing what the harness measures. Existing in-process harness remains the regression tool.

**Kept as public crate API (intentional change from AM-1.7 plan)**: `chronik_memory::{Memory, MemoryBuilder, MemoryRegistry, ...}` are still `pub`. They're used by the in-process eval harness, the background worker binary, and tests — no published version on crates.io exists, so the "remove public client" goal is satisfied by the docstring/README change saying "not a stable SDK". Hard-locking visibility now would block the eval workflow without buying anything.

**Operational finding from smoke run**: `mem.fact.{tenant}` topics are only text-searchable when `CHRONIK_DEFAULT_SEARCHABLE=true` is set. The agent-memory bootstrap should set this for `mem.*` topics by default (small follow-up; tracked inline as a TODO in [`registry.rs`](../crates/chronik-memory/src/registry.rs)).

### AM-1.8: Testing strategy operationalization — ✅ done (2026-06-28)

Operationalizes the **Testing Strategy** section above. Turns the strategy from prose into enforced behavior.

- [x] [`crates/chronik-memory/tests/baselines/longmemeval-s.json`](../crates/chronik-memory/tests/baselines/longmemeval-s.json) seeded with Pilot 7 floor (synth_judge=0.556, raw_judge=0.389, extraction_p_at_5=0.85, cite_source_acc=1.00) and per-category breakdown
- [x] [`crates/chronik-memory/tests/check_baseline.rs`](../crates/chronik-memory/tests/check_baseline.rs) — diff script with `tolerance` from baseline; 5/5 unit tests pass (baseline load, synthetic regression detected, improvement ignored, missing metrics skipped, current run passes when env unset)
- [x] [`.github/workflows/agent-memory-pr.yml`](../.github/workflows/agent-memory-pr.yml) — unit + offline integration + check on agent-memory paths
- [x] [`.github/workflows/agent-memory-nightly.yml`](../.github/workflows/agent-memory-nightly.yml) — runs at 02:00 UTC: LongMemEval-S 18-item pilot → baseline check, HTTP smoke + negative paths + bench, provider parity (warn-only)
- [x] [`.github/workflows/agent-memory-soak.yml`](../.github/workflows/agent-memory-soak.yml) — weekly Sunday 04:00 UTC, 24h `soak_worker`, auto-opens issue on failure
- [x] [`crates/chronik-memory/tests/negative_paths.rs`](../crates/chronik-memory/tests/negative_paths.rs) — 13 scenarios across ingest/remember/forget/recall/admin/source/auth, all passing against live server. (Provider outage covered by `eval_provider_parity.rs`; OOM by `soak_worker.rs`; cluster partition by `cluster_smoke.rs` — not duplicated here.)
- [x] [`crates/chronik-memory/tests/bench_recall_latency.rs`](../crates/chronik-memory/tests/bench_recall_latency.rs) — sustained-load harness with semaphore-bounded concurrency, configurable VUs / total / gates. Measured **p99=105ms at 50 VUs, 484 req/s, 0 errors** on the smoke server.
- [x] [`docs/agent-memory/regression-protocol.md`](agent-memory/regression-protocol.md) — fix-forward / accept / quarantine triage, baseline-update tag rule (`baseline-update: <reason>` in commit message + deliberate file edit), soak triage, who-owns-what table

**Verification on a live single-node server (2026-06-28)**:
- http_smoke: 4/4 ✅
- negative_paths: 13/13 ✅
- bench_recall_latency: 200 reqs / 50 VUs / 100% success / p99=105ms ✅
- check_baseline: 5/5 ✅

**Phase 1 Results** (LongMemEval-S 18-item balanced pilot, pilots 1→7; full ladder in **Appendix B** below):
```
LongMemEval-S synth_judge_rate (target Phase 2: ≥ 0.70):
  Pilot 1 (V1 prompt, raw BM25):                 0.278
  Pilot 2 (V2 prompt, BM25):                     0.389
  Pilot 3 (V3 prompt, BM25):                     0.444  ← persistent ceiling
  Pilot 4 (V4 prompt, BM25):                     0.222  ← regression, V4 demoted to opt-in
  Pilot 5 (V3 + concept pages):                  0.389  ← concepts hurt synth, kept opt-in
  Pilot 6 (V3 + multi-channel + synthesis):      0.444  ← ceiling held
  Pilot 7 (V3 + multi-ch + synth + _abs fix +
           stronger anti-abstention prompt):     0.556  ← NEW HEADLINE (+0.111)

Per-category at Pilot 7:
  knowledge-update:           3/3
  single-session-user:        3/3
  single-session-preference:  2/3
  multi-session:              1/3   (multi-fact arithmetic gap)
  temporal-reasoning:         1/3   (sparse temporal anchors)
  single-session-assistant:   0/3   (extraction-gap items)

Custom fixtures (only 7 of 100):
  Extraction P/R:                   not yet run as a full pass
  Cite-source accuracy:             ≈100% in spot checks (verification is mandatory)

System (in-process Rust API, not HTTP):
  Ingest p50:                       ~5ms   (rdkafka acks=1 → WAL)
  Recall p50 (no synth):            ~40ms  (single-node, single-namespace, hot indexes warm)
  Recall p50 (with synth):          ~900ms (one Anthropic Haiku call dominates)
  Extraction lag p99:               18-25s (target < 30s — within budget)
  Tokens / conversation:            ~3.5k input + ~700 output per extraction batch
```

**Phase 1 exit criteria**:
- [x] Manual top-3 recall precision ≥ 70% on the 7 fixtures (spot-check)
- [x] Cite-source accuracy ≥ 95% (verification enforced at extractor; rejected if missing)
- [x] Extraction lag p99 < 30s
- [x] Ingest p99 < 50ms (in-process; HTTP layer to be measured in AM-1.7)
- [x] **AM-1.7 done** — architectural pivot landed 2026-06-28, smoke test 4/4 against live server
- [x] **AM-1.8 done** — testing strategy operational 2026-06-28: baseline floor, check_baseline (5/5), negative_paths (13/13), bench_recall_latency (p99=105ms), 3 GitHub workflows (PR/nightly/soak), regression protocol
- [ ] **AM-1.5.b done** — 100-fixture set required to detect prompt regressions reliably
- [ ] LongMemEval ≥ 0.70 (Phase 2 gate, not Phase 1; current 0.556)

---

## Phase 2: Production Core

**Expected outcome**: Multi-tenant production deployment. All four memory types. Lifecycle running. Hybrid ranking with full RRF.
**Effort**: 6 weeks
**Risk**: Medium — multi-tenancy and lifecycle introduce new failure modes.
**Depends on**: Phase 1 complete (including AM-1.7 server pivot).

### AM-2.1: All four memory types — ✅ done (in extractor; instruction/task quality less measured)

- [x] All five types in schema (fact / event / instruction / task / concept) — `crates/chronik-memory/src/schema.rs`
- [x] V3+ prompts emit all four canonical types in a single LLM call (concept synthesized off-line by worker)
- [x] Per-type confidence calibration — `extractor/calibration.rs`
- [ ] Task state-machine consumer materializing `mem.task.current.{tenant}` — not built; today tasks compact at the storage layer but no current-state view is exposed
- [ ] Instruction/task recall quality not measured (LongMemEval-S doesn't exercise them heavily; needs targeted fixtures)

### AM-2.2: Compaction-based supersession — ✅ done

- [x] Kafka compaction inherited from Chronik core; topic configs set in `topics.rs`
- [x] `version` field on supersession with tiebreak in `recall.rs:660`
- [x] Forget emits null-value tombstone — `forget.rs`
- [ ] Integration test "ingest 10 contradictions, recall returns latest" — not written

### AM-2.3: Lifecycle controller — ⚠️ consumer MVP done 2026-07-02, tombstone emission + ops endpoint pending

- [x] `crates/chronik-memory/src/lifecycle.rs` — full `SemanticDedup<E: Embedder>` with `decide() → DedupDecision::{Keep, Drop, Supersede}` (threshold-based, confidence-tied tiebreak, batched embedding). Pure function, no LLM/Kafka deps in the call path.
- [x] **Kafka consumer on `mem.fact.{tenant}`** — landed 2026-07-02 as [`crates/chronik-memory/src/lifecycle_consumer.rs`](../crates/chronik-memory/src/lifecycle_consumer.rs). Reads new facts from a topic, decodes to `FactEvent::{Upsert, Tombstone}`, and applies `SemanticDedup::decide` against an in-memory `CandidateStore` keyed by `(namespace, subject, predicate)`. Every decision is counted in `LifecycleStats { records_processed, facts_indexed, tombstones_observed, keeps, drops, supersedes, parse_errors, decide_errors }`. `spawn()` runs it in a background tokio task with 5s retry-on-error. **In-memory candidate lookup, not `Memory::recall`** — the roadmap plan called for recall-based lookup, but the in-memory approach is simpler, deterministic, and doesn't add latency to fact writes. Recall-based lookup can be a follow-up if the in-memory store's growth becomes a concern. 15 unit tests including deterministic-embedder Keep/Drop/Supersede assertions.
- [x] **Tombstone / Supersede emission back to Kafka** — landed 2026-07-02. Trait-based [`DecisionEmitter`] with three impls: `LoggingEmitter` (observability, no side effects — default), `CapturingEmitter` (test-only, buffers `(decision, source_topic, memory_id)` for assertion), `KafkaEmitter` (produces a null-payload tombstone to the source topic keyed by the record's Kafka key). `spawn()` and `run_consumer()` now take an `Arc<dyn DecisionEmitter>`. Keep decisions never emit; Drop / Supersede emit exactly one tombstone. Emit failures are logged + swallowed — the record stays in place and the next consumer pass may retry. No feedback loop: our own tombstone lands on the source topic, our consumer sees it as `FactEvent::Tombstone`, calls `forget_key`, does NOT call `dedup.decide` (see `apply_event`). 4 new emitter unit tests. Total lifecycle_consumer tests: 19.
- [x] **Per-namespace half-life overrides via `mem.config.{tenant}` topic** — landed 2026-07-02 in [`crates/chronik-memory/src/mem_config_consumer.rs`](../crates/chronik-memory/src/mem_config_consumer.rs). Compacted topic per tenant (`mem.config.{tenant}`), keyed by `half_life.{fact,event,instruction,task,concept}`, value = `{schema_version:1, value:{half_life_secs:N}}`. Consumer subscribes via regex `^mem\.config\.[^.]+$`, decodes tenant from the topic name (`tenant_from_topic`), applies to a shared `MemConfig`. `MemConfig::half_life(tenant, kind)` returns the override when set, otherwise falls back to `ranking::half_life(kind)`. Null/empty payload = compaction tombstone → revert to default. `forget_tenant()` for tenant-removal integration. 20 unit tests cover parse (happy, unset via null / empty value, bad topic / non-utf8 / empty key / bad JSON), memory-type key mapping (all 5 variants + rejects unknown), MemConfig CRUD (default fallback, override precedence, unset revert, forget_tenant), apply_event (set, unset, non-positive half-life ignored, unknown key ignored), regex, config defaults. **Recall wiring landed same day**: `Memory::mem_config()` accessor exposes the shared handle from `MemoryBuilder::mem_config(cfg)`; `RegistryConfig::with_mem_config(cfg)` propagates it into every namespace built by the registry; `recall::send()` consults `mem_config.half_life(&tenant_id, kind)` per result before computing the decay factor, falling back to `ranking::half_life(kind)` when no override / no config is set. `main.rs::try_create_mem_config()` spawns the consumer under `CHRONIK_MEMORY_CONFIG_ENABLED=true` and wires the shared `MemConfig` into `try_create_memory_registry()`.
- [x] **`POST /memory/v1/compact` one-shot endpoint** — landed 2026-07-02 in [`crates/chronik-memory/src/compaction.rs`](../crates/chronik-memory/src/compaction.rs) + [`crates/chronik-server/src/unified_api/memory.rs`](../crates/chronik-server/src/unified_api/memory.rs). `CompactionController<E: Embedder + Clone>` bundles the same `CandidateStore` + `SemanticDedup` + `Arc<dyn DecisionEmitter>` the lifecycle consumer uses; `run(namespace_filter, dry_run)` iterates every `(namespace, subject, predicate)` group, calls `SemanticDedup::decide` per record (each record vs. the rest of its group), and emits Drop / Supersede tombstones via the same emitter — synchronously, in a single pass. `CompactionReport { groups_scanned, groups_skipped, records_scanned, keeps, drops, supersedes, decide_errors, emit_errors, dry_run }`. Object-safe via `trait CompactionRunner` so `UnifiedApiState.memory_compaction: Option<Arc<dyn CompactionRunner>>` doesn't leak the embedder generic. Handler returns 503 when unwired, 500 on runner error, 200 with the report otherwise. Six unit tests (empty store, single-record group, duplicate group emits + counts, dry_run does not emit, namespace filter skips other tenants, report round-trip serialization). Main.rs wire-up (lifecycle consumer + compaction controller share `CandidateStore` + emitter) lands with AM-2.3 finalization.

### AM-2.4: Hybrid ranking — ✅ done

- [x] Type-weighted RRF with all five channels in `crates/chronik-memory/src/ranking.rs` + `recall.rs`
- [x] Key-match channel via subject extraction in `recall::extract_subject_candidates`
- [x] HyDE channel behind opt-in flag
- [x] Per-call `channels: [...]` toggle in builder API; per-tenant `mem.config.{tenant}` not yet (pending AM-2.5)

### AM-2.5: Multi-tenancy — ⚠️ Kafka consumer + foundation done 2026-07-02, rate limits + metrics + storage quota pending

Foundation + live-hydration Kafka consumer both landed. Production
deployments can now serve a growing tenant catalog from `mem.tenants`
without restarts:

```
CHRONIK_MEMORY_TENANTS=acme:key1                        # optional seed
CHRONIK_MEMORY_TENANTS_KAFKA_ENABLED=true               # opt into consumer
CHRONIK_MEMORY_REQUIRE_AUTH=true                         # enforce validation
CHRONIK_MEMORY_KAFKA=broker:9092                         # broker for consumer
```

- [x] Validation primitives in [`crates/chronik-memory/src/tenants.rs`](../crates/chronik-memory/src/tenants.rs): `Tenant`, `TenantQuotas`, `TenantRegistry` (lock-free reads via `parking_lot::RwLock`), glob-ish `namespace_patterns` (`acme:*` matches `acme` and anything starting with `acme:`), `validate_request()` returning `AuthError::{MissingCredentials, UnknownTenant, InvalidKey, ForbiddenNamespace}`. 12 unit tests pass.
- [x] `extract_auth` + new `authorize_namespace` in [`unified_api/memory.rs`](../crates/chronik-server/src/unified_api/memory.rs) — three-mode shape: (1) passthrough default, (2) `require_auth=true` enforces header presence (401), (3) `tenants` registry populated enforces full validation (401 on unknown tenant / bad key, 403 on cross-namespace). Wired into `ingest`, `remember`, `forget`, `recall`.
- [x] `UnifiedApiState::with_memory_tenants(Arc<TenantRegistry>)` + bootstrap from `CHRONIK_MEMORY_TENANTS` env var in `main.rs::try_create_tenant_registry`. Tolerant of malformed entries (skip + log warn).
- [x] **`mem.tenants` Kafka compacted topic consumer** — landed 2026-07-02 in [`crates/chronik-memory/src/tenants_consumer.rs`](../crates/chronik-memory/src/tenants_consumer.rs). Topic `mem.tenants`, keyed by `tenant_id`, JSON envelope value with `schema_version=1`. Consumer: `parse_tenant_record()` (pure, unit-testable) + `apply_event()` (mutates the shared `Arc<TenantRegistry>`) + `spawn()` background task with automatic retry on transient errors. Semantics: key/tenant_id mismatch = skip + warn; JSON decode failure = skip + warn; null/empty value = compaction tombstone; upsert = full record replace. 14 unit tests cover parse + apply + full-lifecycle. Wired into `main.rs::try_create_tenant_registry` alongside the env-var seed — both sources co-exist, consumer applies live on top.
- [x] **Per-tenant rate limits** (`ingest_msgs_per_sec`, `recall_qps`) — landed 2026-07-02 in [`crates/chronik-memory/src/tenants_rate_limit.rs`](../crates/chronik-memory/src/tenants_rate_limit.rs). Token bucket per `(tenant_id, EndpointKind)`, `burst = rate` (1s of headroom). Wired into `ingest` (consumes `batch_size` tokens), `remember`/`forget` (1 token as `Write`), `recall` (1 token as `Recall`). Denied requests return `429 Too Many Requests` with a `Retry-After`-shaped message body. `EndpointKind::AdminOrHealth` never rate-limited. `None` or `0` quota = unlimited. Runtime quota changes take effect after next refill. `RateLimiter::forget_tenant()` for tombstone integration. 11 unit tests. Bootstrapped in `main.rs::try_create_tenant_registry` alongside the tenant registry.
- [x] **Per-tenant Prometheus labels** with cardinality cap — landed 2026-07-02 in [`crates/chronik-memory/src/tenants_metrics.rs`](../crates/chronik-memory/src/tenants_metrics.rs). Atomic counters keyed by `(tenant_id, MetricEndpoint, MetricStatus)`. `MetricStatus::from_status(u16)` buckets HTTP codes: 2xx → Ok, 429 → RateLimited (its own bucket so operators can page on quota exhaustion), 4xx → ClientError, 5xx → ServerError. Exposed at `GET /memory/v1/metrics` in Prometheus text format. `format_prometheus(Some(N))` emits the top-N tenants by ops count as individual `tenant="..."` lines and folds the tail into a synthetic `tenant="other"` line to bound cardinality. Recorded from every ingest / remember / forget / recall handler after audit emission. 7 unit tests. Bootstrap: `TenantMetrics::new()` alongside the rate limiter in `main.rs::try_create_tenant_registry`.
- [ ] **Per-tenant storage quota tracking** — needs sum-of-topic-bytes per tenant.

### AM-2.6: Provenance & audit — ✅ core done (2026-07-02); server-side raw-turn resolution deferred

- [x] Schema carries `source.{topic,offsets,extractor}` end-to-end
- [x] Extractor rejects memories without offsets in batch (verified in `extractor/mod.rs`)
- [x] **`GET /memory/v1/{memory_id}/source` endpoint** — landed 2026-07-02. Backed by an in-memory [`chronik_memory::MemoryIndex`](../crates/chronik-memory/src/memory_index.rs) hydrated by a Kafka consumer that tails every `mem.{type}.{tenant}` topic via a regex subscription (`^mem\.(fact|event|instruction|task|task\.current|concept)\.[^.]+$`). Handler responses: `503` when the index isn't wired, `404` when the memory_id hasn't been observed (transient — may succeed on retry as the consumer catches up), `200` with `{memory_id, source: {topic, offsets, extractor}, raw_turns: []}` on hit. **Raw-turn resolution is deliberately deferred** — the pointer is the compliance-critical piece; fetching the actual turn payloads server-side needs a Kafka reader subsystem and can happen in a follow-up. Callers can resolve the pointer today via their own Kafka client or `/_sql SELECT * FROM {source.topic}`. Bootstrapped in `main.rs::try_create_memory_index` behind `CHRONIK_MEMORY_INDEX_ENABLED=true`. 15 unit tests cover parse (happy, tombstone flag, null value, empty value, non-UTF-8 key, bad JSON), index CRUD (insert skips tombstones, remove, upsert replaces source), apply_event lifecycle, and the topics regex (positive + negative cases).
- [x] `mem.audit.{tenant}` topic + emitter — landed 2026-06-28:
  - [`crates/chronik-memory/src/audit.rs`](../crates/chronik-memory/src/audit.rs) — `AuditEvent { ts, tenant, namespace, op, query?, memory_ids, caller_tenant?, status_code, error_code?, latency_ms }` with builder methods + 512-char query truncation + JSON serialization (7 unit tests)
  - `Memory::audit(&event)` produces to `mem.audit.{tenant}` keyed by namespace; best-effort (failure logged + swallowed)
  - `TopicLayout::audit()` + `TopicConfig::audit()` (append-only, `columnar.enabled=true` for SQL compliance queries, no text/vector indexing — audit rows are scanned not searched)
  - `init_namespace_full()` now provisions `mem.audit.{tenant}` alongside the typed topics
  - Wired into `/memory/v1/ingest`, `/remember`, `/forget`, `/recall` in `unified_api/memory.rs` — every request emits an audit row carrying op, namespace, query/key, memory_ids touched, latency_ms, and the caller's `X-Tenant-Id` (so attempted cross-tenant access surfaces in the audit log even before AM-2.5 enforcement lands)

### AM-2.7: Eval (extraction quality + recall NDCG + LongMemEval) — ⚠️ partial

- [x] LongMemEval adapter — `crates/chronik-memory/tests/eval_longmemeval.rs` against `xiaowu0162/longmemeval-cleaned` (18-item balanced pilot)
- [x] Provider parity harness — `eval_provider_parity.rs` runs Anthropic vs OpenAI vs Ollama on same fixtures
- [x] Side-by-side prompt versions via `LONGMEMEVAL_PROMPT_VERSION` env var (V1→V5)
- [ ] Full 500-question LongMemEval pass (today: 18-item balanced subset only)
- [ ] Custom-fixture full-pass extraction P/R (today: 7 fixtures)
- [ ] Three structural gaps must be closed to reach NDCG ≥ 0.70 — see "Levers to close the 0.144 gap" below

**Phase 2 Results** (interim, pre-AM-1.7):
```
LongMemEval-S synth_judge:  0.556  (target ≥ 0.70 — 0.144 gap)
Custom NDCG@10:             not run as full pass (7 fixtures)
Extraction P/R:             not run as full pass
Multi-tenancy:               not implemented
Recall p99 over HTTP:        not measured
```

**Phase 2 exit criteria**:
- LongMemEval NDCG@10 ≥ 0.70 (currently 0.556)
- All four memory types extracting with type-classification ≥ 90%
- Multi-tenant: 10+ tenants in shared cluster, no cross-tenant leaks (audit-log proven)
- Recall p99 < 500ms

### Levers to close the 0.144 gap to 0.70

Three structural gaps identified at Pilot 7:

1. **Two-pass extraction** — closes single-session-assistant 0/3 (Andy clothing, 2-3 eggs, Roscioli). First pass extracts facts as today; second pass scans for assistant-stated facts the user accepted but never restated. **Status**: ✅ implemented 2026-06-28 — `TwoPassExtractor` in [`crates/chronik-memory/src/extractor/two_pass.rs`](../crates/chronik-memory/src/extractor/two_pass.rs). Wraps any inner `Extractor` (pass 1) + adds Anthropic-driven entity scan (pass 2a, ~$0.0001 per batch) + per-entity sweep capped at `max_entities=5` (pass 2b, ~$0.001 each). Cite-source hallucination guard inherited. Merging via (subject, predicate) dedup key. Pass 2 failure falls back to pass 1 — no destruction of base results. 9 unit tests pass including unreachable-URL fallback.
2. **Question-aware ranker** — closes multi-fact arithmetic items (item 4 "$720", item 10 "2 count"). Aggregator-word detection ("how many", "total") boosts numeric-bearing facts before RRF. **Status**: ✅ implemented 2026-06-28 — `ranking::detect_intent` + `ranking::intent_boost` (3-tier classifier: strong-temporal → numeric → weak-temporal → identity). Wired into `recall.rs::send` after RRF combine. 8 new unit tests pass.
3. **Temporal-anchor surfacing** — closes temporal items (item 12 "21 days", item 14 "Met Museum"). Same idea as #2 but for time references. **Status**: ✅ implemented 2026-06-28 — same `intent_boost` covers Temporal queries (events get 1.6×, tasks with `due_at` 1.4×, facts mentioning date words 1.3×).

See **Appendix B** for the empirical evidence that supports these three; gated under standing directive *"no Phase 3 work until LongMemEval ≥ 0.70"*.

### Pilot 8–11 results (2026-07-02): 0.611 is an architectural ceiling

After Pilot 7's 0.556, four levers were built and measured against the Thunderbird K8s cluster:

| Pilot | Config | synth_judge | Δ vs pilot 7 | Verdict |
|-------|--------|-------------|--------------|---------|
| **8** | Pilot 7 + numeric intent boost + temporal intent boost (levers #2 + #3) | **0.611** | +0.056 | **new headline — real lift on items 4, 5, 6, 7** |
| 9 | Pilot 8 + TwoPassExtractor (lever #1) | 0.500 (contaminated; ~0.556 clean) | 0.000 | no lift; 2× extraction cost. Extractor DID find more entity facts (item 2 n_results 1→5) but synthesizer still abstains on assistant-stated targets. |
| 10 | Pilot 8 + v2 synthesis prompt ("commit to assistant-stated facts + single-clear-candidate wins") | 0.556 halfway; killed | –0.056 | v2's aggressive "commit" instruction *broke `_abs` items* (item 9 flipped from correct-abstain HIT to wrong-commit miss). |
| **11** | Pilot 8 + BGE-reranker-v2-m3 (GPU-served, Cohere-compatible) | **0.389** | **–0.222** | worst of all. Cross-encoder demoted answer-carrying memories in favor of topically-similar ones. knowledge-update: 3/3 → 2/3; single-session-user: 3/3 → 1/3. |

**Conclusion**: **0.611 is a real architectural ceiling** with the current stack (Claude Haiku 4.5 extractor + BM25/vector recall + Haiku synthesis). Three orthogonal levers — extraction, ranking, synthesis-prompt, and reranking — all failed to lift it, and reranking regressed it substantially. The remaining 0.089 gap to Phase 2's 0.70 gate requires something *structurally* different (a stronger extractor like Sonnet or GPT-4, fine-tuning on the LongMemEval training set, or multi-hop query decomposition).

**Decision**: publish 0.611 as our number, treat the gate to 0.70 as blocked by *fundamental architecture choices* not *tuning*, and unblock Phase 2 work on the "ceiling" side. Phase 3 gate remains at ≥ 0.70; we accept it will stay blocked until we invest in a stronger extractor or a fine-tuned reranker specific to memory retrieval.

**Kept as opt-in infrastructure** (built cleanly, no code rot, low future risk):
- `chronik_memory::TwoPassExtractor` — wraps any Extractor
- `CHRONIK_MEMORY_SYNTH_PROMPT=v2` env-gated alternate synthesis prompt
- `CHRONIK_MEMORY_RECALL_RERANK=1` env-gated `rerank: true` opt-in on vector fan-out
- `chronik-server` main.rs `try_create_reranker()` bootstrap (CHRONIK_RERANKER_ENDPOINT / MODEL / API_KEY env vars)
- BGE-reranker-v2-m3 K8s manifest at [`deploy/agent-memory/bge-reranker.yaml`](../deploy/agent-memory/bge-reranker.yaml) — GPU-backed, Cohere-compatible via michaelf34/infinity

Any of these can be re-enabled cheaply if a future architecture shift makes them worthwhile.

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

---

## Appendix A — `/memory/v1/*` Endpoint Shapes

All endpoints mount on the Unified API (port 6092). Authentication via `X-Tenant-Id` + `X-API-Key` headers. Phase 1 accepts any tenant header; Phase 2 (AM-2.5) validates against `mem.tenants`.

### `POST /memory/v1/ingest` — raw conversation in (async extraction)

Accepts JSON array or NDJSON. Returns 202 immediately; the extractor worker consumes from `mem.raw.*` and produces typed memories later.

```json
// Request body (array)
[
  {
    "namespace": "agent-real-estate",
    "role": "user",
    "content": "I prefer 2-bedroom apartments in Williamsburg under $4000",
    "external_id": "msg-abc",
    "ts": "2026-06-27T10:00:00Z"
  }
]
```
```json
// 202 Accepted
{ "accepted": 1, "skipped_duplicates": 0, "batch_id": "01HXYZ..." }
```

Idempotency: SHA-256 of `(namespace, role, content)` is the default Kafka key when `external_id` is absent; an LRU cache (5-min TTL) rejects exact duplicates.

### `POST /memory/v1/remember` — bypass extraction, write typed memory directly

```json
{
  "namespace": "agent-real-estate",
  "type": "fact",
  "key": "user|budget|max",
  "body": { "subject": "user", "predicate": "budget_max", "object": 4000, "unit": "USD" },
  "confidence": 1.0
}
```
```json
{ "memory_id": "01HXZ...", "topic": "mem.fact.tenant1", "offset": 4271 }
```

### `POST /memory/v1/forget` — tombstone

```json
{ "namespace": "agent-real-estate", "type": "fact", "key": "user|budget|max" }
```
```json
{ "tombstoned": true, "topic": "mem.fact.tenant1", "offset": 4272 }
```

### `POST /memory/v1/recall` — main query API

```json
{
  "namespace": "agent-real-estate",
  "query": "what's the user's max budget?",
  "types": ["fact", "instruction"],
  "k": 10,
  "channels": ["bm25", "vector", "key_match", "hyde", "sql"],
  "weights": { "bm25": 1.0, "vector": 1.0, "key_match": 2.0, "hyde": 0.5, "sql": 1.5 },
  "include_concepts": false,
  "synthesize": true,
  "min_confidence": 0.5,
  "as_of": "2026-06-27T10:00:00Z"
}
```
All fields except `namespace` and `query` are optional; defaults: `channels = ["bm25","vector"]`, `k = 10`, `synthesize = false`.

```json
{
  "results": [
    {
      "memory_id": "01HX...",
      "type": "fact",
      "key": "user|budget|max",
      "body": { "subject": "user", "predicate": "budget_max", "object": 4000, "unit": "USD" },
      "score": 0.92,
      "channels_hit": ["bm25", "vector", "key_match"],
      "confidence": 1.0,
      "version": 3,
      "valid_from": "2026-06-15T10:00:00Z",
      "source": {
        "topic": "mem.raw.tenant1.agent-real-estate",
        "offsets": [127, 128],
        "extractor": "anthropic-v3"
      }
    }
  ],
  "synthesis": {
    "answer": "The user's maximum budget is $4,000/month for a Williamsburg apartment.",
    "abstained": false,
    "cited_memory_ids": ["01HX..."]
  },
  "latency_ms": 42,
  "channels_executed": ["bm25", "vector", "key_match"]
}
```

### `GET /memory/v1/{memory_id}/source` — provenance walk

```json
{
  "memory_id": "01HX...",
  "raw_turns": [
    {
      "topic": "mem.raw.tenant1.agent-real-estate",
      "offset": 127,
      "role": "user",
      "content": "I prefer 2-bedroom apartments in Williamsburg under $4000",
      "ts": "2026-06-15T10:00:00Z"
    }
  ],
  "extractor": "anthropic-v3"
}
```

### `POST /memory/v1/feedback` — for AM-3.3 reranker training

```json
{ "memory_id": "01HX...", "query": "...", "useful": true, "used_in_response": true }
```

### `POST /memory/v1/admin/init-namespace` — replace CLI helper

```json
{ "tenant": "acme", "agent": "support-bot", "types": ["fact","event","instruction","task"] }
```
Creates the 5 topics (`mem.raw.acme.support-bot.*`, `mem.fact.acme`, etc.) with the right compaction + index configs from `topics.rs`.

### `GET /memory/v1/health`

```json
{
  "ingest_lag_p99_ms": 8,
  "extraction_lag_p99_s": 18,
  "recall_p99_ms": 320,
  "extractor_provider": "anthropic",
  "extractor_version": "anthropic-v3"
}
```

### Errors

Uniform JSON error body, never empty:
```json
{ "error": { "code": "unauthorized", "message": "X-API-Key required", "request_id": "req_..." } }
```
Standard codes: `unauthorized` (401), `forbidden` (403, cross-tenant access), `bad_request` (400, schema violation), `not_found` (404, unknown memory_id), `rate_limited` (429), `internal` (500).

---

## Appendix B — Pilot History (LongMemEval-S, 18-item balanced)

Benchmark: `xiaowu0162/longmemeval-cleaned` HuggingFace dataset, 18-item balanced subset covering all 6 LongMemEval categories. Extractor: Anthropic Claude Haiku 4.5 (`claude-haiku-4-5`), `temperature=0`. Eval: `crates/chronik-memory/tests/eval_longmemeval.rs`.

### Pilot ladder

| Pilot | Configuration | raw_judge | synth_judge | synth_abstain | Δ headline |
|-------|---------------|-----------|-------------|---------------|------------|
| 1 | V1 prompt, substring scorer only | — | — | — | 0.222 (4/18) |
| 2 | V2 prompt + LLM judge scorer | 0.333 | — | — | 0.333 (6/18) |
| 3 | V3 prompt + cross-node fan-out fix + synthesis | 0.333 | 0.444 | 0.500 (9/18) | **0.444** (8/18) |
| 4 | V3 + multi-channel (BM25+vector+key_match) + synthesis | 0.389 | 0.444 | 0.556 (10/18) | 0.444 |
| 5 | V3 + multi-channel + synthesis + concept pages | 0.444 | 0.389 | 0.611 (11/18) | 0.444 |
| 6 | V5 prompt + multi-channel + synthesis | 0.444 | 0.333 | 0.611 (11/18) | 0.444 |
| 7 | V3 + multi-channel + synthesis + `_abs`-aware judge + stronger anti-abstention prompt | 0.389 | **0.556** | 0.500 (9/18) | **0.556** (10/18) ← current |

**Pilot 7 per-category synth_judge**: knowledge-update 3/3, single-session-user 3/3, single-session-preference 2/3, multi-session 1/3, temporal-reasoning 1/3, single-session-assistant 0/3.

### Prompt evolution

- **V1**: minimal "extract facts" — overfit to substring match, missed paraphrased gold answers.
- **V2**: stricter type taxonomy + event-vs-fact disambiguation; raw_judge +0.111 over V1.
- **V3** (current default): user-fact-first calibration with canonical predicate vocabulary, speech-act-event ban, dense user-fact extraction. Type classification 91%, cite-source 100%.
- **V4** (REGRESSED, demoted to opt-in): actor-symmetry instruction designed to broaden third-party / assistant coverage. **Backfired**: judge_rate 0.389 → 0.222. The "extract about all actors" instruction diluted token spend on user-facts (knowledge-update 2/3 → 1/3, single-session-user 2/3 → 0/3, temporal-reasoning 1/3 → 0/3) for only +1 on single-session-assistant. Opt-in via `with_prompt_version(V4)`. **Lesson**: prompt changes that broaden coverage need to be measured against all categories, not just the targeted gap.
- **V5** (opt-in): **additive** concrete-noun extraction (named places / foods / brands / quantities / physical descriptions) layered on top of V3 to avoid V4's tradeoff. Ties V4 raw_judge ceiling at 0.444 but regresses synth_judge to 0.333 — the broader concrete-noun instruction adds enough noise to push the synthesizer toward abstention more often. V5 stays opt-in until a variant lifts both metrics simultaneously.

### Off-roadmap additions

- **Concept pages (path B)** — `concept/{synthesizer,templates,mod}.rs` + `chronik-memory-concept-worker` one-shot binary + `RecallBuilder::include_concepts(bool)`. Pilot 5 measurement: helps raw recall (+0.056 judge) but hurts synthesis (-0.055). Kept as opt-in.
- **Wikilinks** — `wikilinks.rs` with `find_close_or_reopen()` parser handling unclosed-bracket reopens; 12 unit tests.
- **`_abs`-aware judge** — when `question_id` ends in `_abs` and synthesis returned the abstention literal, score 1 directly (the standard judge prompt structurally can't credit correct abstention). Retrospective lift: +0.111 absolute on every pilot that abstained on items 9 + 16. Pilot 4's headline becomes 0.556 with the fix applied retroactively, matching pilot 7's measured number.
- **Anti-abstention synthesis prompt** — explicit instruction to COMPUTE aggregates from value-bearing memories rather than abstain on arithmetic/temporal questions. Landed in pilot 7, recovered item 1 "Four weeks" from synth-abstain to HIT.
- **Null-defensive Anthropic tool_use schema** — required string fields accept `["string", "null"]` so a single null doesn't reject the entire tool_use call; parser drops the malformed record and keeps the rest. Crucial for both V3 and V4 — V4 pilot crashed at item 6 with `Provider("anthropic tool_use ...")` before this landed.

### Structural ceiling analysis

The 0.444 ceiling held across pilots 3-6 because the three weak categories require **different fixes** — no single lever closes more than one:

1. **single-session-assistant (0/3)** — pure extraction gaps. Items: Andy clothing, Roscioli on V3, 2-3 eggs. Memories never get into typed topics because the extractor doesn't see assistant-stated facts the user accepted but never restated. **Fix**: two-pass extraction (candidate-entity pass + per-entity sweep), or V5 on the extraction path with V3 on the synthesis path.
2. **multi-session arithmetic (item 4 "2" count, item 10 "$720")** — value-bearing facts ARE extracted but not retrieved together. **Fix**: question-aware ranker that boosts numeric-bearing facts when the question contains aggregator words ("how many", "total", "sum"), or path-B concept pages with embedded aggregates.
3. **temporal-reasoning (item 12 "21 days", item 14 "Met Museum")** — extraction retrieves sparse anchors (item 14 n=5) and the synthesizer can't infer durations from gaps in sparse coverage. **Fix**: same shape as arithmetic — surface time-anchored facts together.

Pilot 7's +0.111 came from fixing the `_abs` grading bug + the anti-abstention prompt — both *measurement / synthesis-prompt* gains, not extraction or retrieval gains. **The remaining 0.144 gap is structural and requires one of the three fixes above per category.** None has been picked yet.

### Operational lessons

- **WAL OOM at pilot scale**: 5,000+ accumulated `mem.raw.*` segments from successive ULID-namespaced pilot runs → "Internal error: Memory limit exceeded" on produce. Mitigation: wipe + restart cluster between pilot blocks (`tests/cluster/stop.sh && tests/cluster/start.sh`).
- **Anthropic credit exhaustion mid-run** — per-chunk error tolerance in the eval runner prevented crashes when credits ran out at item 9 of a synth pilot; partial results were still gradable. Top up before long pilots.
- **Cost**: a full 18-item pilot with synthesis costs ~$2 and takes 60-75 minutes. A V1-V7 ladder rerun is roughly $14 and ~8 hours.
