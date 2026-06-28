# Chronik Agent Memory — Quickstart

Five-minute path from zero to a working memory backed by Chronik Stream.

## 1. Start Chronik with agent memory enabled

```bash
# Set the two env vars that enable /memory/v1/* on the Unified API.
export CHRONIK_MEMORY_KAFKA="localhost:9092"
export CHRONIK_MEMORY_API="http://localhost:6092"

# (Optional) Require X-Tenant-Id + X-API-Key on every request.
# export CHRONIK_MEMORY_REQUIRE_AUTH=true

# Start single-node.
cargo run --release --bin chronik-server -- start
```

You should see a line like:

```
✓ Agent memory registry wired into Unified API (/memory/v1/*)
```

If you don't, the env vars weren't picked up — the endpoints will be mounted
but return `503 service_unavailable`.

## 2. Provision a namespace

A namespace is `tenant:agent:bot:user:id`. Topics live at `mem.{type}.{tenant}`
so all agents in one tenant share underlying storage.

```bash
curl -sS -X POST http://localhost:6092/memory/v1/admin/init-namespace \
  -H 'Content-Type: application/json' \
  -d '{"tenant": "acme", "agent": "realestate-bot"}'
```

## 3. Ingest a conversation turn

```bash
curl -sS -X POST http://localhost:6092/memory/v1/ingest \
  -H 'Content-Type: application/json' \
  -d '{
    "namespace": "acme:agent:realestate-bot:user:luis",
    "turns": [
      {"role": "user", "content": "I want 2-br in Williamsburg under $4000"}
    ]
  }'
```

You get `202 Accepted` immediately. The actual memory extraction happens
asynchronously in the background worker (target: < 30 s p99 lag).

## 4. Or write a typed memory directly

If you already know the structured fact, skip the extractor:

```bash
curl -sS -X POST http://localhost:6092/memory/v1/remember \
  -H 'Content-Type: application/json' \
  -d '{
    "namespace": "acme:agent:realestate-bot:user:luis",
    "type": "fact",
    "key": "user|budget|max",
    "body": {
      "subject": "user",
      "predicate": "budget_max",
      "object": 4000,
      "text": "max budget $4000/mo"
    },
    "confidence": 1.0
  }'
```

## 5. Recall

```bash
curl -sS -X POST http://localhost:6092/memory/v1/recall \
  -H 'Content-Type: application/json' \
  -d '{
    "namespace": "acme:agent:realestate-bot:user:luis",
    "query": "what is the user'\''s max budget?",
    "k": 5
  }'
```

Default channels are `["bm25", "vector"]`. Add `key_match`, `hyde`, or `sql`
for higher-quality recall on harder queries:

```bash
curl -sS -X POST http://localhost:6092/memory/v1/recall \
  -H 'Content-Type: application/json' \
  -d '{
    "namespace": "acme:agent:realestate-bot:user:luis",
    "query": "what is the user'\''s max budget?",
    "channels": ["bm25", "vector", "key_match"],
    "weights": {"key_match": 2.0}
  }'
```

## 6. Synthesis (single fused answer)

To get one natural-language answer instead of N RRF'd memories, ask for
synthesis. Requires a text generator configured server-side:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
export CHRONIK_MEMORY_SYNTHESIS_PROVIDER=anthropic
# restart chronik-server
```

```bash
curl -sS -X POST http://localhost:6092/memory/v1/recall \
  -H 'Content-Type: application/json' \
  -d '{
    "namespace": "acme:agent:realestate-bot:user:luis",
    "query": "what is the user'\''s max budget?",
    "synthesize": true
  }'
```

Response:

```json
{
  "synthesis": {
    "answer": "The user's maximum budget is $4,000/month for a Williamsburg apartment.",
    "abstained": false,
    "cited_memory_ids": ["01HX..."]
  },
  "results": [...],
  "latency_ms": 380
}
```

## Where to go from here

- Full endpoint reference: [`docs/agent-memory/api-reference.md`](api-reference.md)
- Roadmap + architecture: [`docs/ROADMAP_AGENT_MEMORY.md`](../ROADMAP_AGENT_MEMORY.md)
- More language examples: [`examples/agent-memory/`](../../examples/agent-memory/)
