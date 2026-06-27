# chronik-memory

Event-native agent-memory SDK for [Chronik](https://github.com/chronik-stream/chronik-stream).

Status: **Phase 1 (MVP) complete in code**. End-to-end eval against a live cluster pending — see [`docs/ROADMAP_AGENT_MEMORY_SDK.md`](../../docs/ROADMAP_AGENT_MEMORY_SDK.md).

## What this is

A Rust crate that turns a running Chronik cluster into an agent-memory backend. Customers run their own Chronik; this crate is the client. Hybrid retrieval (BM25 + vector + SQL) over Kafka-compatible event streams, with structured extraction of facts / events / instructions / tasks from raw conversation turns.

## What this is **not**

- Not a managed service.
- Not a competitor to Mem0/Letta/Zep on storage — different substrate.
- Not a foundation model (BYO LLM: Anthropic, OpenAI, Ollama, vLLM).
- Not an agent framework (LangGraph / CrewAI / Mastra integrate this; we don't compete with them).

## Quickstart

```rust
use chronik_memory::{
    AnthropicExtractor, ChainedExtractor, Memory, MemoryType, RuleExtractor, Turn,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let extractor = ChainedExtractor::new(vec![
        Arc::new(RuleExtractor::new()),
        Arc::new(AnthropicExtractor::new(std::env::var("ANTHROPIC_API_KEY")?)),
    ]);

    let mem = Memory::builder()
        .chronik_kafka("localhost:9092")
        .chronik_api("http://localhost:6092")
        .namespace("demo:agent:bot:user:luis")
        .extractor(extractor)
        .build()
        .await?;

    mem.init_namespace().await?;

    mem.ingest_with_extraction(vec![Turn {
        role: "user".into(),
        content: "I want a 3BR in Lapa, max 800k euros".into(),
        ts: None,
        channel: None,
        external_id: None,
    }])
    .await?;

    // Tantivy hot path commits within ~200ms; small wait to be safe.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let results = mem
        .recall("what neighborhoods does this user prefer?")
        .types(&[MemoryType::Fact])
        .k(10)
        .send()
        .await?;

    for r in results {
        println!("[{:.3}] {:?}", r.score, r.memory.body);
    }

    Ok(())
}
```

For a full working example see [`examples/realestate_whatsapp.rs`](examples/realestate_whatsapp.rs):

```bash
ANTHROPIC_API_KEY=sk-ant-... cargo run -p chronik-memory --example realestate_whatsapp
```

## Idempotency

Three layers of duplicate-protection, choose by use case:

| Layer | Strength | Where it lives |
|---|---|---|
| `Turn::external_id` | Cross-process strict | Used as Kafka record key on `mem.raw.{ns}` |
| In-process LRU + 5-min TTL | Per-process best-effort | SDK builder-configurable cache |
| Compaction by `{namespace}\|key` | Eventually consistent | Server-side log compaction on `mem.fact.*` etc. |

```rust
// Webhook with idempotent delivery — set external_id to the upstream message id:
mem.ingest_turn(Turn {
    role: "user".into(),
    content: "I want a 3BR in Lapa".into(),
    ts: None,
    channel: Some("whatsapp".into()),
    external_id: Some("wa-msg-001".into()),
}).await?;
```

`IngestAck::deduped == true` means the SDK's LRU short-circuited before the
Kafka produce. For inspection in tests, `Memory::dedup_state()` exposes the
cache directly.

## Phase 1 status

| Task | Status | Notes |
|------|--------|-------|
| AMS-1.1: Crate skeleton + envelope schema | ✅ | Serde envelope, 4 type bodies, ULID ids, topic naming |
| AMS-1.2: `Memory` client | ✅ | Builder, `ingest`/`remember`/`forget`, idempotency LRU, `init_namespace` |
| AMS-1.3: Extractor v1 | ✅ | `RuleExtractor` (regex pre-pass), `AnthropicExtractor` (tool-use), `ChainedExtractor`, source-cite verification, `ingest_with_extraction` |
| AMS-1.4: Recall (BM25 channel) | ✅ | Parallel `/_search` fan-out across types, RRF + decay + dedup, version-wins compaction semantics. **Vector channel deferred to AMS-2.5** |
| AMS-1.5: Eval harness | ✅ (framework) | Fixture types, P/R/NDCG@10/P@5/MRR scoring, 3 starter fixtures, integration test scaffolds |
| AMS-1.6: Example app | ✅ | `examples/realestate_whatsapp.rs` |
| Phase 1 eval against live cluster | ⏳ | Pending (requires `ANTHROPIC_API_KEY` + running Chronik) |

## Running tests

Unit + doc tests (offline, no external deps):

```bash
cargo test -p chronik-memory --lib
```

Currently 84 lib tests + 2 doc tests, all passing.

Integration eval (requires live Chronik + Anthropic key):

```bash
ANTHROPIC_API_KEY=sk-ant-... CHRONIK_INTEGRATION=1 \
  cargo test -p chronik-memory --test eval_extraction -- --ignored --nocapture

ANTHROPIC_API_KEY=sk-ant-... CHRONIK_INTEGRATION=1 \
  cargo test -p chronik-memory --test eval_recall -- --ignored --nocapture

ANTHROPIC_API_KEY=sk-ant-... CHRONIK_INTEGRATION=1 \
  cargo test -p chronik-memory --test bench_freshness -- --ignored --nocapture
```

## Versioning

Independent semver track from the chronik-stream platform. Currently `0.1.0`.

## License

Apache-2.0 — same as chronik-stream.
