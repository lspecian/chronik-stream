//! Real-estate WhatsApp assistant — Phase 1 example app.
//!
//! Demonstrates the full Phase 1 SDK surface end-to-end:
//! 1. Build a [`Memory`] client with a [`ChainedExtractor`] (rules + Anthropic).
//! 2. `init_namespace()` — create the topics if they don't exist.
//! 3. `ingest_with_extraction()` — produce raw turns and synchronously extract.
//! 4. `recall()` — hybrid retrieval (BM25 in Phase 1).
//! 5. `remember()` — direct write of agent self-memory.
//! 6. `forget()` — tombstone a fact.
//!
//! # Running
//!
//! Requires a live Chronik cluster (`localhost:9092` Kafka, `localhost:6092` HTTP)
//! and an Anthropic API key:
//!
//! ```bash
//! export ANTHROPIC_API_KEY="sk-ant-..."
//! cargo run -p chronik-memory --example realestate_whatsapp
//! ```
//!
//! Override the cluster:
//!
//! ```bash
//! CHRONIK_KAFKA=node1:9092 CHRONIK_API=http://node1:6092 cargo run ...
//! ```

use chronik_memory::{
    AnthropicExtractor, Body, ChainedExtractor, FactBody, Memory, MemoryType,
    RuleExtractor, Turn,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,chronik_memory=debug".into()),
        )
        .init();

    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("set ANTHROPIC_API_KEY");
    let kafka =
        std::env::var("CHRONIK_KAFKA").unwrap_or_else(|_| "localhost:9092".into());
    let api =
        std::env::var("CHRONIK_API").unwrap_or_else(|_| "http://localhost:6092".into());

    // 1. Build the client. Tenant prefix is "demo"; the rest of the namespace
    //    can be any colon-separated path identifying the agent + user.
    let extractor = ChainedExtractor::new(vec![
        Arc::new(RuleExtractor::new()),
        Arc::new(AnthropicExtractor::new(api_key)),
    ]);
    let mem = Memory::builder()
        .chronik_kafka(kafka)
        .chronik_api(api)
        .namespace("demo:agent:realestate-bot:user:luis")
        .extractor(extractor)
        .build()
        .await?;

    // 2. Create topics (idempotent — fine to call on every start).
    println!("→ initializing namespace…");
    mem.init_namespace().await?;

    // 3. Ingest a small WhatsApp-style conversation and synchronously extract.
    println!("→ ingesting + extracting…");
    let ack = mem
        .ingest_with_extraction(vec![
            Turn {
                role: "user".into(),
                content: "Hi! I'm looking for a 3-bedroom in Lapa, Lisbon. South-facing if possible.".into(),
                ts: None,
                channel: Some("whatsapp".into()),
                external_id: Some("wa-msg-001".into()),
            },
            Turn {
                role: "assistant".into(),
                content: "Got it — T3 in Lapa, south-facing preferred. Any budget?".into(),
                ts: None,
                channel: Some("whatsapp".into()),
                external_id: Some("wa-msg-002".into()),
            },
            Turn {
                role: "user".into(),
                content: "Maximum 800k euros. Need to move in by September.".into(),
                ts: None,
                channel: Some("whatsapp".into()),
                external_id: Some("wa-msg-003".into()),
            },
        ])
        .await?;

    println!(
        "  raw turns produced: {}, typed memories produced: {}",
        ack.raw_acks.len(),
        ack.typed_acks.len()
    );
    for ex in &ack.extracted {
        println!(
            "  · [{:?}] conf={:.2} cites={:?} key={:?}",
            ex.body.kind(),
            ex.confidence,
            ex.source_indexes,
            ex.key
        );
    }

    // Tantivy hot path commits within ~200ms; small wait to be safe across
    // cold-only deployments.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 4. Recall: ask a natural-language question, get ranked memories back.
    println!("\n→ recall: \"what is the user's budget?\"");
    let results = mem
        .recall("what is the user's budget")
        .types(&[MemoryType::Fact])
        .k(5)
        .send()
        .await?;
    for r in &results {
        println!(
            "  [{:.3}] {:?} (key={:?}) cites={:?}",
            r.score,
            r.memory.body,
            r.memory.key,
            r.memory.source.offsets
        );
    }

    // 5. Direct write: agent records an instruction it should follow.
    println!("\n→ remember: instruction \"reply in Portuguese\"");
    mem.remember(
        Body::Fact(FactBody {
            subject: "agent:realestate-bot".into(),
            predicate: "preferred_reply_language".into(),
            object: serde_json::json!("pt-PT"),
            polarity: "asserted".into(),
            text: "This agent should reply in European Portuguese.".into(),
        }),
        Some("agent:realestate-bot|preferred_reply_language".into()),
        1.0,
    )
    .await?;

    // 6. Forget: tombstone a fact by key. Useful when the user corrects a claim.
    println!("\n→ forget: dropping the (incorrect) language fact");
    mem.forget(
        MemoryType::Fact,
        Some("agent:realestate-bot|preferred_reply_language"),
        None,
    )
    .await?;

    println!("\nDone — example completed successfully.");
    Ok(())
}
