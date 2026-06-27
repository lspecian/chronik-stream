//! `chronik-memory-concept-worker` — synthesize concept pages for entities.
//!
//! AMS-3.7 path B (one-shot mode). Reads a list of `entity_id`s from the
//! command line (or stdin), and for each one runs
//! [`chronik_memory::synthesize_concept`] against the configured Memory
//! cluster, then writes the result back via
//! [`chronik_memory::Memory::remember_concept`].
//!
//! This is the *manually-triggered* form of the worker. The
//! periodic-trigger version (every N hours, threshold-driven by new memory
//! arrivals per entity) is a follow-up — for now, ops drives synthesis
//! directly when they want to refresh a page.
//!
//! # Required env vars
//!
//! - `ANTHROPIC_API_KEY`
//! - `CHRONIK_MEM_NAMESPACE` — full namespace (e.g. `acme:agent:bot:user:luis`)
//!
//! # Optional env vars
//!
//! - `CHRONIK_KAFKA` (default: `localhost:9092`)
//! - `CHRONIK_API` (default: `http://localhost:6092`)
//! - `ANTHROPIC_MODEL` (default: `claude-haiku-4-5`)
//! - `CHRONIK_CONCEPT_DRY_RUN=1` — synthesize and print, don't write back
//! - `RUST_LOG`
//!
//! # Usage
//!
//! ```bash
//! # Single entity
//! chronik-memory-concept-worker user:luis
//!
//! # Multiple entities, comma-separated:
//! chronik-memory-concept-worker user:luis andy place:lapa_lisbon
//!
//! # Dry-run (print only, don't write):
//! CHRONIK_CONCEPT_DRY_RUN=1 chronik-memory-concept-worker user:luis
//! ```

use chronik_memory::{
    synthesize_concept, AnthropicExtractor, ConceptSynthesisError, Memory,
};
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,chronik_memory=info,rdkafka=warn".into()
            }),
        )
        .init();

    let entity_ids: Vec<String> = std::env::args().skip(1).collect();
    if entity_ids.is_empty() {
        eprintln!(
            "usage: chronik-memory-concept-worker <entity_id> [entity_id ...]\n\
             example: chronik-memory-concept-worker user:luis andy place:lapa_lisbon"
        );
        std::process::exit(2);
    }

    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY is required")?;
    let namespace = std::env::var("CHRONIK_MEM_NAMESPACE")
        .map_err(|_| "CHRONIK_MEM_NAMESPACE is required")?;
    let kafka =
        std::env::var("CHRONIK_KAFKA").unwrap_or_else(|_| "localhost:9092".into());
    let api =
        std::env::var("CHRONIK_API").unwrap_or_else(|_| "http://localhost:6092".into());
    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5".to_string());
    let dry_run = std::env::var("CHRONIK_CONCEPT_DRY_RUN")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);

    let extractor = AnthropicExtractor::new(api_key.clone()).with_model(model.clone());
    // Same struct serves both as the extractor (not used here) and as a
    // `TextGenerator` for the synthesis call.
    let generator: Arc<dyn chronik_memory::embeddings::TextGenerator> = Arc::new(extractor);

    let memory = Memory::builder()
        .chronik_kafka(kafka)
        .chronik_api(api)
        .namespace(&namespace)
        .request_timeout(Duration::from_secs(60))
        .build()
        .await?;

    if dry_run {
        tracing::info!("DRY-RUN mode: pages will be synthesized + printed but NOT written back");
    }

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for entity_id in &entity_ids {
        tracing::info!(entity_id = %entity_id, "synthesizing concept page");
        let t0 = std::time::Instant::now();
        match synthesize_concept(&memory, entity_id, generator.clone()).await {
            Ok(body) => {
                let elapsed = t0.elapsed();
                tracing::info!(
                    entity_id = %entity_id,
                    title = %body.title,
                    bytes = body.markdown.len(),
                    links_out = body.links_out.len(),
                    source_memory_count = body.source_memory_count,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "synthesized"
                );
                println!("\n--- {entity_id} ---");
                println!("{}", body.markdown);
                println!("\n(links_out={:?}, sources={})\n", body.links_out, body.source_memory_count);

                if !dry_run {
                    match memory.remember_concept(body, 1.0).await {
                        Ok(ack) => {
                            tracing::info!(
                                entity_id = %entity_id,
                                topic = %ack.topic,
                                partition = ack.partition,
                                offset = ack.offset,
                                "wrote concept page"
                            );
                            ok += 1;
                        }
                        Err(e) => {
                            tracing::error!(
                                entity_id = %entity_id,
                                error = %e,
                                "failed to write concept page"
                            );
                            failed += 1;
                        }
                    }
                } else {
                    ok += 1;
                }
            }
            Err(ConceptSynthesisError::NoMemories(_)) => {
                tracing::warn!(entity_id = %entity_id, "no atomic memories — skipping");
                skipped += 1;
            }
            Err(e) => {
                tracing::error!(entity_id = %entity_id, error = %e, "synthesis failed");
                failed += 1;
            }
        }
    }

    tracing::info!(
        ok, skipped, failed, total = entity_ids.len(),
        "concept-worker run complete"
    );
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
