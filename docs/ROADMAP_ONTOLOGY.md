# Ontology Roadmap — the event-native Ontology for agents

**Status**: DRAFT (2026-07-04) — successor to [ROADMAP_MEMORY_QUALITY.md](ROADMAP_MEMORY_QUALITY.md). Work begins **after** the memory-quality sprints reach the Phase 2 gate (LongMemEval `synth_judge_rate ≥ 0.70`).
**Version at authoring**: chronik-server 2.7.1.
**Goal**: Give AI agents (and apps, and humans) a single governed interface — typed **Object Types**, **Link Types** (relationships), and **Action Types** (validated writeback) — that resolves to Chronik's existing projections and immutable event log. Agents reason in domain nouns and verbs, never in topics, offsets, SQL, or vector endpoints.
**Research basis**: codebase primitive map + Palantir Foundry Ontology architecture + Zep/Graphiti bi-temporal KG + agent-writeback-safety survey (2026-07-04). Sources inline and in **References**.

> **Terminology (deliberate).** We do **not** call this a "semantic layer." Palantir — the canonical reference implementation — explicitly rejects that framing: *"The Ontology is not a 'semantic layer'; the fourfold integration of data, logic, action, and security cannot be accomplished with a thin semantic layer."* ([ontology-system](https://www.palantir.com/docs/foundry/architecture-center/ontology-system)) We adopt Palantir's own vocabulary — **Object Types, Link Types, Action Types, Functions** — because it is precise and battle-tested, and frame the whole as **Language + Engine + Toolchain over data + logic + action + security**.

---

## 1. Product thesis — why this, why now

Chronik is already, at the substrate level, an event-sourced object store with search, SQL, and vectors. The Agent Memory layer ([ROADMAP_AGENT_MEMORY.md](ROADMAP_AGENT_MEMORY.md)) proved you can derive **typed, identified, provenance-carrying objects** from the immutable log and serve them to agents over HTTP — and it built the exact engine pattern the Ontology needs (see §4). The Ontology is the **generalization of that proof from one fixed domain (an agent's memory) to arbitrary user-defined domains**, plus the two pillars Memory never needed: **relationships** between objects and **validated actions** that write back.

The bet is **not** "add a graph database." It is: **an Ontology that is event-native gets, structurally, the three things every graph-DB-backed competitor (Zep/Graphiti, Neo4j+LLM, Cognee, Palantir-on-warehouses) has to engineer around.**

1. **Time-travel / "as-of" for free.** Objects and links are folds over the log; `as_of(t)` is a fold to an offset. Bi-temporal by construction — matching Graphiti's headline feature ([graphiti](https://github.com/getzep/graphiti)) without a bolt-on validity engine.
2. **Provenance is the substrate, not a feature.** Every object attribute and every edge carries the source offset(s) that justified it. "Why do you believe this?" resolves to raw events. Memory already ships this (`source.{topic,offsets}` in [schema.rs:95-105](../crates/chronik-memory/src/schema.rs#L95), plus the `LineageIndex` DAG in [lineage.rs](../crates/chronik-memory/src/lineage.rs)).
3. **Relationships are derived, not asserted.** An edge exists because an event happened, so it can never drift out of sync with the truth and can always be re-derived. No dual-write consistency problem between "the data" and "the graph" — the failure mode every graph-DB-over-a-warehouse deployment fights.

**What makes the *product* better.** Today an agent must know Chronik's plumbing (`POST /_search` with a Tantivy match, `/_vector/{topic}/search`, `/_sql`, hot-vs-cold tables, the 5s fan-out timeout). That leaks infrastructure into the agent's reasoning. Both reference systems reject this: Palantir's AIP agents operate *"through governed, auditable pathways defined within the Ontology rather than bypassing it for raw data access"* ([aip-architecture](https://www.palantir.com/docs/foundry/architecture-center/aip-architecture)), and Graphiti exposes *six fixed MCP tools, no raw Cypher* ([mcp-server](https://help.getzep.com/graphiti/getting-started/mcp-server)). The Ontology replaces "know the storage engine" with "ask for objects, traverse links, invoke actions" over one MCP/HTTP surface.

**The defensible wedge — temporal + auditable memory (from the differentiation research).** Points 1–3 above are not just conveniences; on an immutable log they are things a mutable-graph competitor (Zep/Graphiti-on-Neo4j, Mem0, Cognee) *cannot retrofit* without effectively rebuilding on an event log. Three lead with the strongest moat:
- **Knowledge-time replay.** "What did the agent believe about X *as of last Tuesday, given only what it had ingested by then*?" Competitors track valid-time (world-time) but invalidate-in-place over a mutable store; replaying the whole *belief state* at a past transaction-time needs an immutable substrate. XTDB/Datomic prove the model ("all joins are as-of joins"); we get it because objects/edges are folds to an offset. ([XTDB bitemporality](https://v1-docs.xtdb.com/concepts/bitemporality/), [Zep](https://arxiv.org/html/2501.13956v1))
- **Recompute-the-ontology-on-schema-change.** When extraction/relation logic improves, *reproject the entire graph from source events* — no lossy migration of an asserted graph, no orphaned edges. Asserted-graph competitors can't cheaply re-derive; their graph *is* the source of truth. ([Fowler, Event Sourcing](https://martinfowler.com/eaaDev/EventSourcing.html))
- **Audit == memory.** "Replay exactly what the agent saw at decision T" for GDPR/SOC2/HIPAA is the *same* `as_of` mechanism that powers time-travel — the audit trail and the memory store are one immutable log, not a separate subsystem that can drift. ([AI audit-trail guidance](https://galileo.ai/blog/ai-agent-compliance-governance-audit-trails-risk-management))

Positioning line: *"your agent's memory is an auditable, time-travelable ledger — ask it what it knew, when it knew it, and why."*

---

## 2. The Palantir → Chronik architecture mapping (the core insight)

Palantir's Ontology backend is a set of named services over *backing datasets/streams* — objects are **not stored natively**, they are indexed from datasources ([object-backend/overview](https://www.palantir.com/docs/foundry/object-backend/overview)). That is exactly Chronik's model: projections materialized from the log. The mapping is unusually tight, which is strong evidence the approach is sound:

| Palantir service / concept | What it does | Chronik equivalent | Status |
|---|---|---|---|
| **Ontology Metadata Service (OMS)** | defines which object/link/action types exist | **Type registry** = new keyed-index consumer over `ont.types.{tenant}` (§4 template) | to build (O-0) |
| **Object Set Service (OSS)** | reads: search / filter / aggregate / load | `recall.rs` multi-channel fan-out + `query_router.rs` (`/_search`, `/_vector`, `/_sql`) | **exists** |
| **Object Data Funnel** | indexes datasources **and Action edits** into object DBs, with **offset-tracked edit queue**, read-after-write | `WalIndexer` (indexing) + new object/edge consumers + **CAS-append** (the offset-tracked edit queue) | partial; CAS missing |
| **Object types backed by datasets/streams** | objects are indexed, not stored | Memory's typed records materialized via Tantivy/Parquet/HNSW | **exists** (generalize types) |
| **Link Types** | schema of a relationship between two object types | **edge index** (new keyed-index consumer over `ont.edges.{tenant}`); `LineageIndex` is the prototype | to build (O-1) |
| **Action Types** | validated set of edits committed atomically; `auto` vs `confirm`; side-effects | **command handler**: propose→confirm→apply events + **CAS-append** + audit | to build (O-3) |
| **Functions (TS/Python, traverse links, make edits)** | server-side sandboxed compute over the ontology | out of scope early; later via `/_sql` + WASM | future (O-4+) |
| **AIP typed tools: Action / Object Query / Function** | the only way agents touch the ontology | **MCP tools**: `invoke_action` / `query_objects` / `traverse` / `as_of` | to build (O-2) |

Palantir's Object Storage V2 even describes its edit path as *"a Funnel-managed queue that has offset tracking to support simultaneous user edits"* with *read-after-write* guarantees ([os-v2](https://www.palantir.com/docs/foundry/object-backend/object-storage-v2-breaking-changes)). **That offset-tracked edit queue is precisely an optimistic-concurrency CAS-append on an event log** — which is the one primitive Chronik lacks and O-3 must build. The reference architecture independently arrives at the same design an event log makes natural.

---

## 3. The five pillars, and which are already built

| Pillar | Status | Evidence |
|---|---|---|
| **Objects** (typed entities derived from events) | ~Built | Memory's typed records (fact/event/instruction/task/concept) are objects; generalize to user-defined types. [schema.rs:41-92](../crates/chronik-memory/src/schema.rs#L41) |
| **Identity** (stable object id across events) | ~Built | Deterministic `key` + compaction = supersedeable identity ([schema.rs](../crates/chronik-memory/src/schema.rs)). Fuzzy entity resolution (§5.6, §9 risk) will appear. |
| **Provenance** (object/edge → source events) | **Built** | `source.{topic,offsets}` end-to-end (AM-2.6) + `LineageIndex` ancestor/descendant DAG (AM-3.4). |
| **Relationships** (typed edges between objects) | **Missing** | Needs an edge index. `LineageIndex` ([lineage.rs](../crates/chronik-memory/src/lineage.rs)) is the working prototype (§4). |
| **Actions** (validated commands that write back) | **Missing** | Needs single-aggregate **compare-and-swap append**. Transactions are stubbed; **no CAS today** — confirmed in [produce_handler.rs](../crates/chronik-server/src/produce_handler.rs) (idempotent dedup only, no expected-offset validation). |

**Sequencing consequence**: the read side (Objects + Identity + Provenance + Links-as-derived-views + the agent surface) ships and delivers agent value *before* the hard write side (Actions). We do **not** gate the whole Ontology on the CAS primitive.

---

## 4. The reusable engine pattern this rides on (confirmed)

My earlier feasibility note worried "no cross-record keyed state exists in the codebase." **That was true of the WalIndexer projection pipeline but false of the Memory layer**, which built and unit-tested the exact pattern the Ontology needs, six times: a **consumer-maintained, keyed, in-memory index rebuilt from a compacted Kafka topic**, wired into `main.rs` behind an env flag, exposed on the Unified API.

**The 5-piece template** (from the codebase map): (1) an `Arc<DashMap>` index keyed by a composite key; (2) an event enum `{Upsert, Tombstone}`; (3) a *pure* decode fn `(key_bytes, value_bytes) → Event` (unit-testable, no Kafka); (4) a *pure* apply fn (lock-scoped mutation + stats); (5) an async consumer loop (rdkafka, regex or explicit subscription, hydrates from beginning-of-log since the topic is compacted, 5s retry).

**The six existing instances:**

| Index | Key | Topic | File | Tests |
|---|---|---|---|---|
| `TaskCurrentIndex` | `(namespace, task_id)` | `mem.task.{tenant}` | [task_current_consumer.rs](../crates/chronik-memory/src/task_current_consumer.rs) | 15 |
| `CandidateStore` | `(namespace, subject, predicate)` | `mem.fact.{tenant}` | [lifecycle_consumer.rs](../crates/chronik-memory/src/lifecycle_consumer.rs) | 10 |
| `MemConfig` | `(tenant, MemoryType)` | `mem.config.{tenant}` | [mem_config_consumer.rs](../crates/chronik-memory/src/mem_config_consumer.rs) | 20 |
| `TenantRegistry` | `tenant_id` | `mem.tenants` | [tenants_consumer.rs](../crates/chronik-memory/src/tenants_consumer.rs) | 14 |
| `MemoryIndex` | `memory_id` | `mem.{type}.*` (regex) | [memory_index.rs](../crates/chronik-memory/src/memory_index.rs) | 15 |
| `LineageIndex` | `memory_id` → ancestors/descendants | fed via `observe()` | [lineage.rs](../crates/chronik-memory/src/lineage.rs) | 10 |

**This collapses most of the risk I previously assigned to Relationships.** The edge index is not a from-scratch capability — it is the **seventh instance of an already-proven, already-tested template**, and the codebase map gives a concrete recipe: a `RelationshipIndex { outgoing, incoming: Arc<DashMap<entity, Vec<Edge>>> }` auto-populated by a consumer over `mem.fact.*` that reads `(subject, predicate, object)` triples. `LineageIndex` already does the bidirectional ancestor/descendant half; the gap is (a) auto-population from facts rather than manual `add_citation()`, and (b) general typed edges rather than only citation edges.

**What remains genuinely new** (not covered by the template): the **CAS-append** primitive for Actions (§6) — the one true engine gap.

---

## 5. External grounding & validated design stances

**Palantir Foundry Ontology** ([core-concepts](https://www.palantir.com/docs/foundry/ontology/core-concepts), [action-types](https://www.palantir.com/docs/foundry/action-types/overview), [agent-studio/tools](https://www.palantir.com/docs/foundry/agent-studio/tools)):
- Types are precise "schema definitions"; objects are *backed by* datasources, not stored → **matches Chronik's projection model exactly**.
- All agent writes flow through **Action Types** (a closed catalog with validation, submission criteria, permissions, side-effects). The agent cannot mutate except via a predefined Action → **enforce writeback authority at the append boundary, never in the prompt**.
- AIP agent tools are **Action / Object Query / Function** — typed, governed, auditable → our MCP surface mirrors these three.

**Zep / Graphiti** — the closest published analog ([graphiti](https://github.com/getzep/graphiti), [mcp-server](https://help.getzep.com/graphiti/getting-started/mcp-server)):
- **Bi-temporal**: every edge carries valid-time (`t_valid`/`t_invalid`) and transaction-time; outdated facts are **invalidated, not deleted**. Chronik gets transaction-time free (offset/ingestion) and Memory already has valid-time (`valid_from`/`valid_to`) → **bi-temporal is nearly free for us**.
- Agent surface is **fixed retrieval tools + an MCP server** (`search_facts`, `search_nodes`, `get_episodes`, …), **never raw Cypher** → validates "fixed traversal tools over NL-to-Cypher."

**Agent-writeback safety** — strong convergence across Palantir, LangGraph HITL, OpenAI, NeMo Guardrails, Anthropic ([action-types](https://www.palantir.com/docs/foundry/action-types/overview), [langgraph HITL](https://docs.langchain.com/oss/python/langchain/human-in-the-loop), [openai function-calling](https://developers.openai.com/api/docs/guides/function-calling), [nemo rails](https://docs.nvidia.com/nemo/guardrails/about-nemo-guardrails-library/rail-types), [anthropic effective agents](https://www.anthropic.com/research/building-effective-agents)):
- **Propose → confirm → apply**: split irreversible writes into a draft (no effect) and a commit (fires only after confirmation). Per-action risk tiers: auto-execute (reads), confirm-execute (writes), approval-execute (destructive).
- **Idempotency key + optimistic concurrency** (`expected_prev` version/offset) are the two load-bearing primitives for retry-safe agent writes.
- **An append-only log makes all of this inherent**: proposals, approvals, commits are just events; idempotency keys are event fields; the optimistic-concurrency token *is* the last-observed offset = the CAS-append conflict check; **dry-run/preview is free** because a Proposal event carries no domain effect.

**Competitive landscape (the field agrees with these stances).** Every surveyed agent-memory/graph system converges on **fixed tools + an MCP server, not raw query languages** — the two that expose query languages do so as a *gated fallback*, not the default:

| System | Graph? | Bi-temporal? | Agent interface | Raw query to agent? | MCP |
|---|---|---|---|---|---|
| **Zep/Graphiti** | LPG (Neo4j) | **Yes** (4 timestamps/edge) | 6 fixed tools (`search_facts`, `search_nodes`, …) | No | Ships server |
| **Cognee** | LPG+vector | Yes (`temporal_cognify`) | fixed `SearchType` enum (`add`/`cognify`/`search`) | Cypher = gated opt-in | Ships server |
| **Mem0** | Platform only | No | fixed `add`/`search`/`get_all` | No | 2 servers |
| **Neo4j GraphRAG** | LPG (Neo4j) | No (only temporal types) | SDK retrievers | **Yes — raw read/write Cypher tools** | Official |
| **LlamaIndex PGI** | LPG | No | composable retrievers | Text2Cypher opt-in | Client only |
| **Letta/MemGPT** | No (vector tiers) | No | fixed memory tools | No | Client only |
| **Chronik Ontology** | LPG (derived) | **Yes, free** (log offsets) | fixed MCP tools + resources | No (guarded fallback) | Ships server |

Our differentiation is not the interface (the field has standardized it) — it is the **event-native substrate** (bi-temporal for free, provenance-durable, recompute-on-change) delivered *through* the interface the field already agrees on.

**Why fixed tools, quantified (text-to-query reliability research).** Free-form NL→query is too unreliable to be the primary path: Text2Cypher hits only **~30%** execution match with GPT-4o ([Neo4j benchmark](https://neo4j.com/blog/developer/benchmarking-neo4j-text2cypher-dataset/)), CypherBench **60%** ([arXiv](https://arxiv.org/html/2412.18702v1)), NL-to-SQL tops out ~**72%** on BIRD (whose own labels agree with experts only 62% of the time) ([CIDR audit](https://www.vldb.org/cidrdb/papers/2026/p5-jin.pdf)). Schema-linking hallucination (invented columns/relations) causes **~20%+** of failures ([arXiv](https://arxiv.org/pdf/2501.09310)). A semantic layer that lets the agent *select named entities/metrics* instead of inventing identifiers lifts accuracy from **~45% → ~90%** ([arXiv](https://arxiv.org/pdf/2604.25149)). → **Fixed, allowlisted, LIMIT-capped, read-only traversal tools are the default; generated queries only as a guarded, cost-gated fallback.**

**Contrast with analytics semantic layers (dbt/Cube/Malloy).** They share the "define-once, consume-many, select-by-name" governance model (Cube even ships an MCP surface for agents — [semantic layer for AI agents](https://cube.dev/articles/semantic-layer-for-ai-agents-2026)), but their primitives are entities/dimensions/**measures** and they **compile queries, not commands** — no action/mutation primitive. Our **Action Types** (write-side, validated) are precisely what an analytics semantic layer cannot express, and what makes this an *ontology* rather than a metrics layer.

**Design stances adopted (grounded above):**
1. **Labeled property graph, not RDF/OWL.** Lightweight, agent-friendly; skip formal reasoners. LPG is what Zep/Graphiti and MS GraphRAG actually ship, and it now has an ISO standard (GQL, [ISO/IEC 39075:2024](https://www.iso.org/standard/76120.html)). Reserve RDF/OWL for cross-org interop / formal compliance only.
2. **Fixed traversal + query tools over free-form NL-to-Cypher/SQL** for the agent surface (numbers above).
3. **MCP is the primary agent surface**; REST `/ontology/v1/*` is the programmatic fallback.
4. **Actions = event-sourcing command handlers** with propose/confirm/apply + CAS, not a Kafka-transaction monster.
5. **Bi-temporal edges**: invalidate, never delete.
6. **Identity: deterministic-first, LLM-fallback.** Graphiti evolved *from* pure-LLM dedup *to* deterministic front-ends (exact-match, MinHash+LSH, entropy gate) with LLM only in the ambiguous "grey zone" — cheaper, lower-variance. GraphRAG's pure exact-string match is the cautionary under-merge baseline. Adopt: deterministic key → blocking → LLM adjudication only for the uncertain band. ([Zep dedup](https://arxiv.org/html/2501.13956v1), [ER survey](https://arxiv.org/pdf/2008.04443))

---

## 6. The one hard primitive — CAS-append for Actions (design sketch)

Everything except Actions rides the §4 template. Actions need the single missing engine capability, and the writeback-safety research hands us the design.

**Primitive**: single-aggregate compare-and-swap append — "append this event to stream keyed `K` only if the stream's latest offset for `K` is still `N`." Confirmed absent today ([produce_handler.rs](../crates/chronik-server/src/produce_handler.rs) has idempotent sequence dedup but no expected-offset validation; transaction state machine is stubbed). Atomic offset allocation already exists in the produce path, and the idempotent producer's per-key sequence tracking is a working template — so this is a **localized change**, not a rewrite.

**Action lifecycle (three event types on `ont.action.{tenant}`):**
1. `ActionProposed{action_type, params, idempotency_key, read_offset}` — no domain effect (free dry-run/preview; agents can show the user what would happen).
2. `ActionConfirmed{proposal_id, approver}` — only for `confirm`/`approval`-tier actions; `auto`-tier skips.
3. `ActionApplied{proposal_id, emitted_events}` — the validator runs (schema + cross-object invariants + permissions), then **CAS-appends** the domain events using `read_offset` as the concurrency token; a stale token rejects with a precondition failure, exactly like Palantir's Funnel edit queue.

**Per-action risk tier** (`auto | confirm | approval`) is a property of the Action Type in the registry, driving both the MCP tool schema the LLM sees and whether a human gate is required — mirroring Palantir's per-tool `auto`-vs-`confirm` and LangGraph's `interrupt_on` policy.

**Spike CAS standalone first** — before building any Action semantics, prove the append-only-if-offset-N primitive under concurrency in isolation. It gates every Action; it is the long pole.

---

## 7. Phased plan (product-value-ordered)

Each phase ships agent-visible value on its own. Ordered so the cheapest, highest-leverage, lowest-risk capability lands first and the one hard primitive (CAS) is isolated and late.

### Phase O-0: Object Types — generalize Memory's typed records into user-defined objects
**Product value**: define an Object Type; Chronik materializes instances from projections with identity + provenance + `as_of` — domain-agnostic Memory.
**Engine work**: **type registry** (7th keyed-index consumer over `ont.types.{tenant}`); object **instance resolution** reusing recall/`query_router` fan-out to assemble current attributes; `as_of` fold to offset.
**Reuses**: Memory schema/provenance, §4 template, recall fan-out, Unified-API mount + `MemoryRegistry` pattern.
**Grounding**: Palantir Object Type = "schema definition… backed by datasources" — mirror the model, not the storage.
**Exit**: define `Repository`/`Issue`/`Deployment`-style types in a demo namespace; `get_object`, `list`, `as_of` return correct, provenance-carrying instances on a labeled fixture.

### Phase O-1: Link Types — derived, provenance-carrying, bi-temporal edges + traversal
**Product value**: `traverse(objectA, "fixes", Issue)` and 1–3 hop queries agents can't cheaply assemble from SQL+search today.
**Engine work**: **edge index** (`RelationshipIndex`, per §4 recipe) over `ont.edges.{tenant}`; edge records carry `(src, type, dst, source_offset, valid_from, valid_to)`; derivation rules emit edges from events (rule-pass + optional LLM-pass, mirroring the extractor); bidirectional traversal API; **edge invalidation over time (Graphiti model)** rather than deletion.
**Reuses**: §4 template, extractor pattern for derivation, `LineageIndex` bidirectional model.
**Grounding**: Graphiti bi-temporal edges + invalidation; Palantir Link Type.
**Exit**: edges derived from a demo event stream; 1–3 hop traversal with provenance on every edge; `as_of` traversal; edge-invalidation correctness on a supersession fixture.

### Phase O-2: Agent-facing surface (MCP + REST) — the payoff
**Product value**: agents stop touching storage primitives. One MCP server exposes objects, traversal, search, and `as_of`.
**Engine work**: `ont.mcp` server (+ `/ontology/v1/*` REST) exposing a **small set of consolidated, fixed tools** (Anthropic tool-design guidance: few high-impact tools, `search_*` not `list_*`, semantic names not UUIDs, `concise|detailed` verbosity flag — [writing tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents)): `get_object`, `query_objects` (filter/aggregate — Palantir's "Object Query"), `traverse`, `search` (routes to the right projection under the hood), `as_of`, `explain` (provenance/lineage). Results use MCP `structuredContent` + `outputSchema` mirroring object/edge types; traversal returns edges as **`resource_link`s to neighbor URIs** so the agent walks a navigable graph rather than swallowing one serialized subgraph. No raw Cypher/SQL exposed to the LLM (guarded fallback only).
**Two MCP planes (Palantir precedent — do not conflate)**: a **data plane** (read objects / traverse / invoke actions — the OMCP analog) and a **control plane** (define object/link/action types — the "Palantir MCP" analog). Ship the data plane here; control plane is O-4.
**Differentiation — MCP *resources*, not just tools.** No surveyed graph/ontology MCP server exposed data via MCP **resources** — all used tools even for reads ([MCP resources](https://modelcontextprotocol.io/docs/concepts/resources)). An event-native backend is uniquely suited to close that gap: expose objects as **URI-addressable resource templates** (`chronik://object/{type}/{id}`) that are **subscribable** (`resources/subscribe` → `notifications/resources/updated`), so the host *pushes live object updates into the agent's context* off the event stream instead of re-polling a query tool. This is a capability the mutable-graph competitors structurally lack.
**Reuses**: recall fan-out, `query_router`, the O-0/O-1 indexes; the hot-path stream for resource-update notifications.
**Grounding**: Graphiti's 6 fixed MCP tools; Palantir OMCP vs Palantir MCP two-plane split; Neo4j/Memgraph fixed toolsets; "fixed tools > NL-to-query."
**Exit**: an agent completes a multi-hop task ("conversations discussing decisions that affected a service with an open incident") via MCP tools only — and beats a baseline agent using raw `/_search`+`/_sql` on the agent-task-success fixture (§8 metric).

### Phase O-3: Action Types — validated writeback (the hard, isolated primitive)
**Product value**: agents can *do* things (close an issue, reassign a deployment) with validation, audit, dry-run preview, and no lost updates.
**Engine work**: **CAS-append** primitive (§6, spike first); Action Type definitions with pre/post validation; propose→confirm→apply event flow; per-action risk tiers; full audit (reuse Memory's `mem.audit` pattern); MCP `invoke_action` tool with the `auto`/`confirm`/`approval` gate.
**Reuses**: Memory audit topic, idempotent-producer machinery as the CAS template, §4 template for the action-state index.
**Grounding**: Palantir Action Types + Funnel offset-tracked edit queue; propose/confirm/apply + idempotency + optimistic concurrency (writeback-safety survey).
**Exit**: an Action rejects an invalid command (cross-object invariant) under concurrency **without a lost update**; every applied Action is auditable to its emitted events; dry-run returns a correct preview with zero domain effect.

### Phase O-4: Platformization — product track
Type-authoring UX, ontology governance/permissions on objects/edges/actions, Functions (server-side compute), docs, reference integrations (MCP catalog, LangGraph/CrewAI). Gated on customer commitment — mirrors Memory's Phase 4 stance. Governance lessons from Palantir permissions + analytics semantic layers (dbt/Cube) fold in here.

---

## 8. Measurement discipline

Unlike Memory, the Ontology has no single LongMemEval-style oracle, so the **product metric leads**: does the semantic surface make the agent *better*, not just prettier.

- **Agent task success (headline)**: end-to-end completion rate on a fixture of multi-hop tasks via the MCP surface, **vs. a baseline agent using raw `/_search`+`/_sql`**. This is the product proof. (No standard KG-agent benchmark exists to borrow — hand-roll a fixture using LongMemEval-style methodology; a full-fleet run per phase, per the Memory measurement discipline.)
- **Object correctness**: materialized attributes match labeled fixtures (P/R); `as_of` correctness at N historical offsets.
- **Relationship quality**: edge P/R vs labeled edges; **provenance-cite accuracy ≥ 95%** (every edge's `source_offset` actually justifies it — the same hallucination guard Memory enforces); edge-invalidation correctness on supersession fixtures.
- **Action safety**: zero lost updates under concurrent CAS; 100% of applied actions auditable to emitted events; dry-run has zero domain effect.
- **System**: object-resolution & traversal p50/p99; edge-index memory footprint per 1M edges; `as_of` latency.

**Discipline inherited from Memory**: every phase ends with a full fixture-fleet run; no lever ships without a before/after number; provenance-cite is a hard gate.

---

## 9. Risks & open questions
- **Premature generalization** — do NOT build the abstract type engine before O-0 proves the object model on a real domain. (Same lesson as "ship Memory before Ontology.")
- **CAS correctness** — Actions create a new correctness-critical write path; **isolate and spike CAS (§6) before believing in writeback.**
- **Cross-record state is a category, not a feature** — the edge index is Chronik's entry into stateful stream processing; the §4 template de-risks the mechanics, but scope O-1 aware that this is the door to Kafka-Streams/Flink-class capability.
- **Entity resolution creep** — deterministic keys cover most cases; fuzzy resolution (à la Memory's 0.97-cosine dedup) will appear. Decide the identity model deliberately, once, in O-0, using the deterministic-first / LLM-fallback approach in stance §5.6 (blocking → grey-zone adjudication → canonicalization). Watch both failure directions: under-merge (GraphRAG's exact-string cautionary tale) and over-merge (embedding false-positives).
- **Boundary discipline** — the Ontology is a layer *on* Chronik (new `chronik-ontology` crate + Unified-API mount), never domain semantics inside `chronik-server`. (AD-1 precedent from Memory.)
- **Don't over-model** — resist RDF/OWL/SPARQL and NL-to-Cypher; both reference systems chose lightweight property graphs + fixed tools. Revisit only with evidence.

## 10. References
- Palantir Ontology: [core-concepts](https://www.palantir.com/docs/foundry/ontology/core-concepts) · [object-backend](https://www.palantir.com/docs/foundry/object-backend/overview) · [OSv2](https://www.palantir.com/docs/foundry/object-backend/object-storage-v2-breaking-changes) · [action-types](https://www.palantir.com/docs/foundry/action-types/overview) · [functions](https://www.palantir.com/docs/foundry/functions/overview) · [agent-studio tools](https://www.palantir.com/docs/foundry/agent-studio/tools) · [AIP architecture](https://www.palantir.com/docs/foundry/architecture-center/aip-architecture) · [ontology-system ("not a semantic layer")](https://www.palantir.com/docs/foundry/architecture-center/ontology-system) · [Ontology MCP (OMCP)](https://www.palantir.com/docs/foundry/ontology-mcp/overview) · [Palantir MCP (control plane)](https://www.palantir.com/docs/foundry/palantir-mcp/overview)
- Zep/Graphiti (bi-temporal KG, closest analog): [github](https://github.com/getzep/graphiti) · [Zep paper (arXiv 2501.13956)](https://arxiv.org/html/2501.13956v1) · [beyond static KGs](https://blog.getzep.com/beyond-static-knowledge-graphs/) · [memory API](https://help.getzep.com/v2/memory) · [MCP server](https://help.getzep.com/graphiti/getting-started/mcp-server)
- Competitive landscape: [Cognee](https://github.com/topoteretes/cognee/blob/main/cognee/skill.md) · [Mem0 graph memory](https://docs.mem0.ai/open-source/graph_memory/overview) · [Neo4j GraphRAG python](https://github.com/neo4j/neo4j-graphrag-python) · [Neo4j MCP cypher](https://github.com/neo4j-contrib/mcp-neo4j/tree/main/servers/mcp-neo4j-cypher) · [LlamaIndex Property Graph Index](https://developers.llamaindex.ai/python/framework/module_guides/indexing/lpg_index_guide/) · [Letta archival memory](https://docs.letta.com/guides/core-concepts/memory/archival-memory) · [MS GraphRAG (arXiv 2404.16130)](https://arxiv.org/html/2404.16130v2)
- LPG vs RDF/OWL: [Neo4j RDF-vs-PG](https://neo4j.com/blog/knowledge-graph/rdf-vs-property-graphs-knowledge-graphs/) · [GQL ISO/IEC 39075:2024](https://www.iso.org/standard/76120.html) · [W3C RDF-star WG](https://www.w3.org/2025/04/rdf-star-wg-charter.html)
- MCP surface design: [tools](https://modelcontextprotocol.io/docs/concepts/tools) · [resources](https://modelcontextprotocol.io/docs/concepts/resources) · [Anthropic — writing tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents) · [Neo4j MCP](https://neo4j.com/developer/genai-ecosystem/model-context-protocol-mcp/) · [Memgraph MCP](https://memgraph.com/blog/introducing-memgraph-mcp-server)
- Text-to-query reliability: [Neo4j Text2Cypher benchmark](https://neo4j.com/blog/developer/benchmarking-neo4j-text2cypher-dataset/) · [CypherBench (arXiv 2412.18702)](https://arxiv.org/html/2412.18702v1) · [schema-linking failures (arXiv 2501.09310)](https://arxiv.org/pdf/2501.09310) · [semantic-layer-vs-text2sql (arXiv 2604.25149)](https://arxiv.org/pdf/2604.25149) · [BIRD audit (CIDR 2026)](https://www.vldb.org/cidrdb/papers/2026/p5-jin.pdf)
- Entity resolution: [(Almost) All of ER (arXiv 2008.04443)](https://arxiv.org/pdf/2008.04443) · [LLMs for entity matching (arXiv 2405.16884)](https://arxiv.org/pdf/2405.16884) · [Graphiti dedup evolution](https://blog.getzep.com/graphiti-hits-20k-stars-mcp-server-1-0/)
- Analytics semantic layers: [dbt MetricFlow](https://docs.getdbt.com/docs/build/about-metricflow) · [Cube — semantic layer for AI agents](https://cube.dev/articles/semantic-layer-for-ai-agents-2026) · [Malloy](https://docs.malloydata.dev/documentation/)
- Event-native differentiation: [XTDB bitemporality](https://v1-docs.xtdb.com/concepts/bitemporality/) · [Datomic as-of](https://blog.datomic.com/2013/05/a-whirlwind-tour-of-datomic-query_16.html) · [Fowler — Event Sourcing](https://martinfowler.com/eaaDev/EventSourcing.html) / [CQRS](https://martinfowler.com/bliki/CQRS.html) · [AI audit trails](https://galileo.ai/blog/ai-agent-compliance-governance-audit-trails-risk-management)
- Agent writeback safety: [LangGraph HITL](https://docs.langchain.com/oss/python/langchain/human-in-the-loop) · [OpenAI function-calling](https://developers.openai.com/api/docs/guides/function-calling) · [NeMo Guardrails rails](https://docs.nvidia.com/nemo/guardrails/about-nemo-guardrails-library/rail-types) · [Anthropic — effective agents](https://www.anthropic.com/research/building-effective-agents)
- Internal: [ROADMAP_AGENT_MEMORY.md](ROADMAP_AGENT_MEMORY.md) · [ROADMAP_MEMORY_QUALITY.md](ROADMAP_MEMORY_QUALITY.md) · `ontology-feasibility` memory (code-grounded seed analysis).

---

## Appendix A — Codebase primitive inventory (what exists vs. missing)

**Exists & reusable**: 6 keyed-index consumers (§4 table); memory envelope with provenance ([schema.rs:41-105](../crates/chronik-memory/src/schema.rs#L41)); 5-channel recall + RRF ([recall.rs](../crates/chronik-memory/src/recall.rs)); Unified-API mount + `MemoryRegistry` per-namespace cache ([unified_api/memory.rs](../crates/chronik-server/src/unified_api/memory.rs)); event-sourced metadata (JSON events, `apply_replicated_event`, rebuild-from-log — [metadata/events.rs](../crates/chronik-common/src/metadata/events.rs)); temporal validity (`valid_from`/`valid_to`); idempotent producer dedup.

**Missing (build in this roadmap)**:
- **Entity/edge index** — auto-populated `RelationshipIndex` from `mem.fact.*` triples; `LineageIndex` is the manual prototype (O-1).
- **Object type registry** — user-defined types vs. 5 hardcoded memory types (O-0).
- **Action/command handler** — precondition-gated state transitions; today `TaskCurrentIndex` is latest-wins with no preconditions (O-3).
- **CAS / conditional append** — no expected-offset validation in the produce path; transactions stubbed (O-3, §6).

## Appendix B — Open research threads to resolve during design
Resolved in the 2026-07-04 research pass (see §5): competitive landscape (near-universal fixed-tools+MCP convergence), LPG-vs-RDF (LPG wins), text-to-query reliability (numbers), MCP surface design (tools vs resources), entity-resolution approach (deterministic-first), analytics-semantic-layer contrast (no action primitive). Remaining genuinely open:
- **Fuzzy entity-resolution tuning** — the *approach* is decided (§5.6); the thresholds/blocking keys per object type are an O-0 empirical task.
- **Functions** (server-side compute over the ontology, Palantir's TS/Python) — map to `/_sql` + WASM, or defer entirely? Decide at O-4.
- **Edge-invalidation semantics under out-of-order events** — Graphiti sorts by `valid_at` and invalidates the earlier edge; validate this holds on Chronik's offset-ordered log during O-1.
- **Verification flags from research** (re-check before load-bearing citation): a few arXiv IDs surfaced with anomalous future dates; OWL-2-EL reasoner-compliance claims rest on unfetched summaries; treat as directional.
