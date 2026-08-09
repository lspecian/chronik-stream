# Long-Term Layered Memory — Design Notes

**Status:** design discussion, not a spec. Captures what exists, the trade-off
space, and the one architectural decision that matters for scale. Topology
specifics (per-tenant vs per-agent, partition counts) are deliberately deferred
as implementation details.

Grounded in two code maps (2026-08-09): the `chronik-memory` layer and the
Chronik Stream broker's storage tiering.

---

## What already exists (design forward from this, don't re-propose)

**Typed layer is long-run-handled.** `mem.{fact,event,instruction,task}.{tenant}`
are **per-tenant shared topics**; recall scopes to a conversation by **filtering
on the `namespace` field** (cross-namespace records dropped; the query appends
the namespace token). Aging is a **query-time `decay_factor`** (type-specific
half-lives: fact 365d, instruction 730d, event 30d, task 7d, concept 90d) — a
scoring multiplier, never physical deletion. Plus compaction=supersession,
SemanticDedup (Keep/Drop/Supersede), per-entity concept pages, conflict
detection, provenance graph. Key lesson from the roadmap: *"memories don't
physically age; their relevance score does."*

**The broker already tiers every topic** (verified in code):
```
Producer → WAL (local, fsync)                              [recent window only]
   → segment seals (250MB / 30min / 30s-idle)
   → every 30s WalIndexer uploads raw segment (+Parquet if columnar, +Tantivy if searchable)
   → WAL segment DELETED (delete_after_index, default on)   [WAL is bounded]
   → object-store copies kept forever                       [warm → cold]
   → Kafka Fetch cold-reads on demand (buffer → WAL → S3 segments → Tantivy)
```
So data does **not** pile up in WAL; it tiers to object-store segments + Parquet
and stays retrievable. "Infinite retention" is ~the default behavior.

**Gaps the maps surfaced:**
- Object store defaults to a **local dir**; remote S3/GCS/Azure is opt-in
  (`OBJECT_STORE_BACKEND`), and it's a **single global bucket** (per-topic bucket
  is roadmap-only).
- **No retention/deletion of tiered data** — `retention.ms`/`cleanup.policy` are
  metadata-only at the broker; object-store copies are never expired.
- **`s3://` Parquet in the SQL engine is unverified** — no `register_object_store`
  for `s3://` found; cold *Fetch* is proven, cold *SQL over S3 Parquet* may not
  resolve.
- **Data-loss window — FIXED (v2.10.10, #24):** WAL segment was deleted even when
  its object-store upload failed. Now gated on a clean pass. This was a
  prerequisite: a memory-of-record can't tier onto a lossy pipeline.

---

## Three axes, mostly orthogonal (you can have all three)

- **A. Infinite retention, cheap** — closest to free today. Needs: (1)
  `OBJECT_STORE_BACKEND=s3` (not default), (2) a retention/lifecycle *policy* if
  you ever want cost-tiering/expiry (currently keep-forever), (3) per-tenant
  isolation only if required (not today → single bucket, prefixed by topic).
  Mostly **config + policy**, not a rebuild.
- **B. Scale / topic-explosion** — the one real architectural gap (below).
- **C. Long-horizon recall** — orthogonal to storage. Read-time extraction over
  cold raw works via Fetch. Gap: **no hierarchical session→higher-level
  summarization** (only per-entity concept pages + a non-wired count-based event
  rollup scaffold). Pure recall-quality work.

---

## The architectural spine: conversation-as-DATA, not conversation-as-TOPIC

**Problem:** the **raw layer is the lone outlier** — one topic *per conversation*
(`mem.raw.{tenant}.{agent}.{conv}`). At millions of conversations that's
millions of topics, each carrying metadata + a segment-index entry + a hot-index
slot **forever**. The code already proves this fails: orphan-reclamation exists
because *"~9K tenants clogged the cluster"* during the LongMemEval eval. Millions
is a non-starter no matter how well the data tiers.

**Fix:** make raw consistent with the typed layer — a **per-tenant (or per-agent)
shared topic** with `conversation_id` as an indexed field + partition key, not a
topic each. This is not a new model; it's the pattern the typed layer already
uses at scale.

```
mem.raw.{tenant}                          [one topic, like mem.fact.{tenant}]
  ├─ partitioned by hash(conversation_id)  [a conversation's turns co-located + ordered]
  ├─ each record carries conversation_id / namespace   [existing scoping field]
  └─ broker tiers it unchanged (WAL → object-store → cold-read)
```
Read-time raw retrieval: query `mem.raw.{tenant}` **filtered by conversation_id**
(default = in-conversation recall; drop/widen the filter for long-horizon).

**Why this fixes millions specifically:**
- **Topic count**: ~#conversations (millions) → ~#tenants (thousands). The
  clogged-cluster failure mode is gone.
- **Hot-index working set** is bounded by *active* conversations, not *total*:
  the hot text/vector index holds recent turns per partition, evicted on
  inactivity. The measured ~20MB/conversation-topic no longer multiplies by
  total conversations; cold conversations aren't in RAM. A follow-up months
  later cold-reads its turns from object store.
- **Tiering carries over unchanged** — a shared topic tiers exactly like a
  per-conversation one.

---

## Open decisions (deferred as implementation-specific)

1. **Sharding granularity** — per-tenant (fewest topics, matches typed) vs
   per-agent (`mem.raw.{tenant}.{agent}`, more isolation) vs fixed-N-shards for
   very large tenants. Decided by whether "agent" is a real isolation boundary.
2. **Partition count per raw topic** — sets throughput + the hot-index working-set
   ceiling; a huge tenant may need the shard option.
3. **Read-time retrieval scope** — default in-conversation with an opt-in
   agent-wide/long-horizon mode, vs always cross-conversation with recency+decay
   scoping. This is where long-horizon recall quality is decided.
4. **Retention policy (axis A)** — keep-forever (current) vs enforced
   `retention.ms` / S3 lifecycle for cost-tiering. Metadata exists; enforcement
   doesn't.
5. **Migration** — new conversations use the shared topic immediately; existing
   per-conversation topics get a one-time consolidation or are left to age out
   cold (left-to-age is simplest).

---

## Related prerequisites / follow-ups
- **s3:// Parquet SQL resolution** — verify/close before relying on long-term SQL
  over cold Parquet.
- **Hierarchical consolidation** (axis C) — session→semantic rollups for
  long-horizon recall; the rollup scaffold exists but isn't wired.
- **Read-time extraction** — the retrieve-raw→read path (`synthesize_readtime` /
  `run_raw_search`) lives on branch `feat/memory-hybrid-infra-and-quality`, not
  yet on `main`. It's what makes retained raw the source of truth, so it and this
  topic model are the same initiative.
