# Chronik Agent Memory — HTTP API Reference

All endpoints mount on the Unified API. Default port: **6092**. Override with
`CHRONIK_UNIFIED_API_PORT`.

This page is the contract; the source of truth is
[`docs/ROADMAP_AGENT_MEMORY.md` Appendix A](../ROADMAP_AGENT_MEMORY.md#appendix-a-memoryv1-endpoint-shapes).

## Common headers

| Header | When | Description |
|--------|------|-------------|
| `Content-Type: application/json` | required on POST | All bodies are JSON. |
| `X-Tenant-Id` | when `CHRONIK_MEMORY_REQUIRE_AUTH=true` | Tenant identifier (e.g. `acme`). |
| `X-API-Key` | when `CHRONIK_MEMORY_REQUIRE_AUTH=true` | Tenant API key. Validated against `mem.tenants` in AM-2.5 (currently passthrough). |
| `X-Request-Id` | optional | Echoed in error responses for log correlation. |

## Common error envelope

Every non-2xx response carries a JSON body. Never empty.

```json
{
  "error": {
    "code": "unauthorized",
    "message": "X-Tenant-Id and X-API-Key headers are required",
    "request_id": "req_abc123"
  }
}
```

Stable codes: `unauthorized` (401), `forbidden` (403), `bad_request` (400),
`not_found` (404), `not_implemented` (501), `rate_limited` (429),
`service_unavailable` (503), `internal` (500).

## Endpoints

### `POST /memory/v1/ingest`

Ingest raw conversation turns. **Asynchronous**: returns 202 immediately;
typed memories are produced by the background worker (target lag < 30 s p99).

Request: see [Appendix A](../ROADMAP_AGENT_MEMORY.md#post-memoryv1ingest-raw-conversation-in-async-extraction).
Response: 202 with per-turn acks.

Idempotency: SHA-256 of `(namespace, role, content)` is the default Kafka
record key. Supply `external_id` to use your own. A 5-minute LRU dedups exact
duplicates.

### `POST /memory/v1/remember`

Write a typed memory directly, bypassing extraction. Use when you already have
structured data and don't want LLM cost or extraction lag.

The `type` field must be one of `fact`, `event`, `instruction`, `task`,
`concept`. The `body` shape is type-specific — see
[`crates/chronik-memory/src/schema.rs`](../../crates/chronik-memory/src/schema.rs)
for the full struct definitions.

### `POST /memory/v1/forget`

Tombstone a memory. Exactly one of `key` or `memory_id` is required. Concept
memories aren't directly forgettable in Phase 1 — instead, forget the underlying
atomic memories and re-synthesize.

### `POST /memory/v1/recall`

The main query API. Multi-channel fan-out + RRF + optional synthesis.

**Channel defaults**: `["bm25", "vector"]`. Opt-in to the rest:

| Channel | What it does | Cost |
|---------|--------------|------|
| `bm25` | Full-text search via Tantivy. | cheap, always on |
| `vector` | Embedding ANN via HNSW. | cheap once embeddings cached |
| `key_match` | Subject-extracted SQL lookup. | 1 small LLM call (or rule-based) |
| `hyde` | Hypothetical answer → embed → ANN. | 1 cheap LLM call |
| `sql` | Structured filter via DataFusion. | cheap |

**Default weights**: `bm25=1.0, vector=1.0, key_match=2.0, hyde=0.5, sql=1.5`.

**Synthesis**: `synthesize: true` runs the top-`k` results through a fusion
prompt. Requires `CHRONIK_MEMORY_SYNTHESIS_PROVIDER=anthropic` +
`ANTHROPIC_API_KEY` server-side; returns 503 otherwise. Answer is short, with
an explicit `"I don't know."` abstention literal when no relevant memory exists.

**Time travel**: `as_of: "..."` drops memories with `valid_from > as_of`.

### `GET /memory/v1/{memory_id}/source`

**Not implemented in Phase 1** (returns 501). Provenance walk requires a
`memory_id → (topic, offset)` index that lands as part of AM-2.6.

### `POST /memory/v1/feedback`

Record agent feedback for AM-3.3 offline reranker training. In Phase 1 the
handler logs the signal and returns 200; the full pipeline to
`mem.feedback.{tenant}` topic + weekly training cron lands in Phase 3.

### `POST /memory/v1/admin/init-namespace`

Provision the topic set for a `(tenant, agent)` pair. Idempotent — safe to
call repeatedly. Creates `mem.fact.{tenant}`, `mem.event.{tenant}`,
`mem.instruction.{tenant}`, `mem.task.{tenant}` with the correct compaction +
indexing configs.

Optional `types: [...]` restricts which topics the response reports (the
handler always creates the full set; the filter is cosmetic for now).

### `GET /memory/v1/health`

Liveness + sanity. Returns:

```json
{
  "status": "ok",                    // or "disabled"
  "namespaces_cached": 3,
  "extractor_provider": "anthropic", // or null
  "extractor_version": "haiku-4.5"   // or null
}
```

`status: "disabled"` means `CHRONIK_MEMORY_ENABLED` is not set (or
`CHRONIK_MEMORY_KAFKA` + `CHRONIK_MEMORY_API` are missing). The endpoint is
mounted regardless so monitoring is uniform across deployments.

## Server configuration

| Env var | Default | Purpose |
|---------|---------|---------|
| `CHRONIK_MEMORY_ENABLED` | auto | Force-enable / disable. Auto-enables when both KAFKA + API are set. |
| `CHRONIK_MEMORY_KAFKA` | `localhost:9092` | Bootstrap servers for the agent-memory producers. |
| `CHRONIK_MEMORY_API` | `http://127.0.0.1:6092` | Where the registry's Memory instances reach the Unified API for recall fan-out. |
| `CHRONIK_MEMORY_REQUIRE_AUTH` | `false` | When true, every `/memory/v1/*` request must carry `X-Tenant-Id` and `X-API-Key`. |
| `CHRONIK_MEMORY_SYNTHESIS_PROVIDER` | unset | `anthropic` enables `synthesize: true` recall. Requires `ANTHROPIC_API_KEY`. |
| `CHRONIK_UNIFIED_API_PORT` | `6092` | Port the Unified API binds. |

## Phase 1 limitations

- No multi-tenant isolation enforced (AM-2.5 ships that)
- `GET /memory/v1/{memory_id}/source` returns 501 (AM-2.6)
- Feedback logged but not persisted (AM-3.3)
- Concept-page direct forgetting unsupported — forget atomic memories instead
- No rate limiting (AM-2.5)
- No Prometheus metrics on the /memory path yet (AM-1.8)
