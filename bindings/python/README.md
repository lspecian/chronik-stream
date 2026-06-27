# chronik-memory (Python)

Python bindings for the [`chronik-memory`](https://github.com/chronik-stream/chronik-stream/tree/main/crates/chronik-memory)
agent-memory SDK.

`chronik-memory` turns a running [Chronik Stream](https://github.com/chronik-stream/chronik-stream)
cluster into an event-native agent-memory backend: every conversation turn is
durably persisted to Kafka, an extractor distills typed memories
(facts / events / instructions / tasks), and a hybrid recall surface (BM25 +
vector + key-match + HyDE + SQL) returns ranked memories to your agent.

This wheel is a thin PyO3 wrapper around the Rust SDK. Heavy lifting (Kafka
producer, HTTP client, RRF fusion, idempotency cache) happens in Rust; the
Python surface is awaitables that compose with `asyncio`.

## Status

Phase 3 alpha (AMS-3.1 in the
[SDK roadmap](https://github.com/chronik-stream/chronik-stream/blob/main/docs/ROADMAP_AGENT_MEMORY_SDK.md)).
The Rust core is production-grade; the Python surface is currently:

- ✅ `Memory.build(...)` — connects to a Chronik cluster
- ✅ `init_namespace()` / `init_namespace_full()`
- ✅ `ingest_turn(role, content, *, external_id=None, channel=None)`
- ✅ `recall(query, *, k=10, types=None)` — returns `list[dict]`
- ⏳ `ingest_with_extraction(...)` — pending Python-side extractor wrapping
- ⏳ `remember()` / `forget()` — pending typed-class layer
- ⏳ `chronik_memory.types` Pydantic re-exports

## Install

```bash
pip install chronik-memory
```

(Wheels: Linux x86_64/aarch64, macOS x86_64/aarch64. Windows TBD.)

## Quickstart

```python
import asyncio
from chronik_memory import Memory

async def main():
    mem = await Memory.build(
        kafka="localhost:9092",
        api="http://localhost:6092",
        namespace="acme:agent:bot:user:luis",
    )
    await mem.init_namespace()

    await mem.ingest_turn(
        role="user",
        content="I prefer Lapa, max budget 800k euros.",
    )

    results = await mem.recall("what neighborhoods does the user prefer?", k=5)
    for r in results:
        print(r["score"], r["memory"]["key"])

asyncio.run(main())
```

## Why a Rust core?

Per-turn ingestion latency in the single-digit-ms range, RRF fusion across
five recall channels, and 24/7 background workers without GIL contention.
The Python wheel exists so your agent code stays in Python; the
performance-critical path stays in Rust.

## License

Apache-2.0. See `LICENSE` in the repository root.
