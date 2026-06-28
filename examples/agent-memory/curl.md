# Agent Memory — curl examples

All endpoints live on the Unified API (default port 6092). Replace
`HOST=http://localhost:6092` with your deployment's URL.

```bash
HOST=http://localhost:6092
NS="acme:agent:realestate-bot:user:luis"
```

## Provision the namespace

```bash
curl -sS -X POST "$HOST/memory/v1/admin/init-namespace" \
  -H 'Content-Type: application/json' \
  -d '{"tenant": "acme", "agent": "realestate-bot"}'
```

Response:

```json
{
  "namespace": "acme:realestate-bot:bot:_:_",
  "topics_created": [
    "mem.fact.acme",
    "mem.event.acme",
    "mem.instruction.acme",
    "mem.task.acme"
  ]
}
```

## Ingest raw conversation turns

```bash
curl -sS -X POST "$HOST/memory/v1/ingest" \
  -H 'Content-Type: application/json' \
  -d "{
    \"namespace\": \"$NS\",
    \"turns\": [
      {\"role\": \"user\",      \"content\": \"I prefer 2-bedroom apartments in Williamsburg under \$4000\"},
      {\"role\": \"assistant\", \"content\": \"Got it — any deal-breakers on amenities?\"}
    ]
  }"
```

The handler returns `202 Accepted` immediately; extraction is asynchronous (a
background worker turns these into typed memories within ~30 s).

## Write a typed memory directly (skip extraction)

```bash
curl -sS -X POST "$HOST/memory/v1/remember" \
  -H 'Content-Type: application/json' \
  -d "{
    \"namespace\": \"$NS\",
    \"type\": \"fact\",
    \"key\": \"user|budget|max\",
    \"body\": {
      \"subject\": \"user\",
      \"predicate\": \"budget_max\",
      \"object\": 4000,
      \"text\": \"max budget \$4000\"
    },
    \"confidence\": 1.0
  }"
```

## Recall — single-channel default (BM25 + vector)

```bash
curl -sS -X POST "$HOST/memory/v1/recall" \
  -H 'Content-Type: application/json' \
  -d "{
    \"namespace\": \"$NS\",
    \"query\": \"what is the user's max budget?\"
  }"
```

## Recall — full multi-channel + synthesis

Synthesis requires a text generator configured server-side
(`CHRONIK_MEMORY_SYNTHESIS_PROVIDER=anthropic` + `ANTHROPIC_API_KEY`). Without
it, the handler returns 503.

```bash
curl -sS -X POST "$HOST/memory/v1/recall" \
  -H 'Content-Type: application/json' \
  -d "{
    \"namespace\": \"$NS\",
    \"query\": \"what's the user's max budget?\",
    \"k\": 10,
    \"channels\": [\"bm25\", \"vector\", \"key_match\"],
    \"weights\": {\"bm25\": 1.0, \"vector\": 1.0, \"key_match\": 2.0},
    \"synthesize\": true,
    \"min_confidence\": 0.5
  }"
```

## Forget a memory (compaction tombstone)

```bash
curl -sS -X POST "$HOST/memory/v1/forget" \
  -H 'Content-Type: application/json' \
  -d "{
    \"namespace\": \"$NS\",
    \"type\": \"fact\",
    \"key\": \"user|budget|max\"
  }"
```

## Health

```bash
curl -sS "$HOST/memory/v1/health"
```

```json
{
  "status": "ok",
  "namespaces_cached": 3,
  "extractor_provider": "anthropic",
  "extractor_version": "haiku-4.5"
}
```

`status` is `"disabled"` when `CHRONIK_MEMORY_ENABLED` is not set.

## Auth (when enabled)

If the server is started with `CHRONIK_MEMORY_REQUIRE_AUTH=true`, every
request must carry `X-Tenant-Id` and `X-API-Key`:

```bash
curl -sS -X POST "$HOST/memory/v1/recall" \
  -H "X-Tenant-Id: acme" \
  -H "X-API-Key: $CHRONIK_API_KEY" \
  -H 'Content-Type: application/json' \
  -d "{ \"namespace\": \"$NS\", \"query\": \"...\" }"
```

In Phase 1 (current), the headers are advisory and pass through unchecked.
AM-2.5 will validate the tuple against `mem.tenants`.
