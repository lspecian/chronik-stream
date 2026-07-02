# Agent Memory — Python examples

The agent memory surface is HTTP+JSON; any Python HTTP client works. These
examples use the stdlib + `httpx`. No SDK to install.

```python
import httpx

BASE = "http://localhost:6092"
NAMESPACE = "acme:agent:realestate-bot:user:luis"
```

## One-time namespace setup

```python
httpx.post(
    f"{BASE}/memory/v1/admin/init-namespace",
    json={"tenant": "acme", "agent": "realestate-bot"},
).raise_for_status()
```

## Ingest a conversation turn

```python
resp = httpx.post(
    f"{BASE}/memory/v1/ingest",
    json={
        "namespace": NAMESPACE,
        "turns": [
            {"role": "user", "content": "I want 2-br in Williamsburg under $4000"},
        ],
    },
)
resp.raise_for_status()
print(resp.json())  # {"accepted": 1, "skipped_duplicates": 0, "batch_id": "...", "acks": [...]}
```

## Recall

```python
resp = httpx.post(
    f"{BASE}/memory/v1/recall",
    json={
        "namespace": NAMESPACE,
        "query": "what's the user's max budget?",
        "k": 5,
        "channels": ["bm25", "vector", "key_match"],
    },
)
resp.raise_for_status()
data = resp.json()
for r in data["results"]:
    print(f"[{r['type']:<11}] {r['score']:.3f}  {r['body']}")
```

## Recall with synthesis (single answer)

```python
resp = httpx.post(
    f"{BASE}/memory/v1/recall",
    json={
        "namespace": NAMESPACE,
        "query": "what's the user's max budget?",
        "synthesize": True,
    },
)
data = resp.json()
if data.get("synthesis"):
    print(data["synthesis"]["answer"])
```

## Use with an agent framework (LangChain / LlamaIndex / etc.)

The recall response is just JSON; wrap it as a tool:

```python
def chronik_memory_tool(query: str, namespace: str = NAMESPACE) -> str:
    """Recall from Chronik Agent Memory. Returns a single synthesized answer."""
    r = httpx.post(
        f"{BASE}/memory/v1/recall",
        json={"namespace": namespace, "query": query, "synthesize": True},
        timeout=30,
    )
    r.raise_for_status()
    syn = r.json().get("synthesis")
    return syn["answer"] if syn else "I don't know."
```

That's the entire integration. No SDK, no version coupling — when Chronik
ships a new memory type, the same client keeps working.

## Streaming ingest (high-throughput producer pattern)

For sustained ingest, batch turns into a single request rather than one
turn per call. The handler dedups within a 5-minute LRU; `external_id`
overrides the default SHA-256 key when you have your own identifier.

```python
def ingest_batch(turns: list[dict], namespace: str = NAMESPACE) -> None:
    httpx.post(
        f"{BASE}/memory/v1/ingest",
        json={"namespace": namespace, "turns": turns},
        timeout=10,
    ).raise_for_status()

# Drain a queue, 100 turns at a time
buf = []
for turn in source:
    buf.append(turn)
    if len(buf) >= 100:
        ingest_batch(buf)
        buf.clear()
if buf:
    ingest_batch(buf)
```
