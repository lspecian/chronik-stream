//! `chronik-memory-worker` — standalone background extraction worker.
//!
//! Reads config from environment variables, builds a [`Memory`] client + an
//! [`AnthropicExtractor`], spawns a [`Worker`], and runs until `Ctrl-C`.
//!
//! # Required env vars
//!
//! - `ANTHROPIC_API_KEY` — Anthropic Messages API key
//! - `CHRONIK_MEM_NAMESPACE` — full namespace (e.g. `acme:agent:bot:user:luis`)
//!
//! # Optional env vars
//!
//! - `CHRONIK_KAFKA` — bootstrap servers (default: `localhost:9092`)
//! - `CHRONIK_API` — unified API URL (default: `http://localhost:6092`)
//! - `ANTHROPIC_MODEL` — Claude model id (default: `claude-haiku-4-5`)
//! - `CHRONIK_MEM_GROUP_ID` — consumer group id (default: derived from namespace)
//! - `CHRONIK_MEM_BATCH_SIZE` — max messages per batch (default: 200)
//! - `CHRONIK_MEM_BATCH_WINDOW_MS` — max ms per batch (default: 5000)
//! - `CHRONIK_MEM_INIT_NAMESPACE` — `1` to call `init_namespace_full()` on start
//!   (default: `1`)
//! - `RUST_LOG` — log filter (default: `info,chronik_memory=info`)

use chronik_memory::{
    AnthropicExtractor, ChainedExtractor, Memory, RuleExtractor, Worker, WorkerConfig,
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

    // Build the extractor: rule pre-pass + Anthropic v2 LLM pass.
    let extractor = ChainedExtractor::new(vec![
        Arc::new(RuleExtractor::new()),
        Arc::new(AnthropicExtractor::new(api_key).with_model(model)),
    ]);

    let memory = Memory::builder()
        .chronik_kafka(kafka)
        .chronik_api(api)
        .namespace(&namespace)
        .extractor(extractor)
        .build()
        .await?;

    if env_truthy("CHRONIK_MEM_INIT_NAMESPACE", true) {
        memory.init_namespace_full().await?;
    }

    let mut config = WorkerConfig::default();
    if let Ok(g) = std::env::var("CHRONIK_MEM_GROUP_ID") {
        if !g.is_empty() {
            config.group_id = Some(g);
        }
    }
    if let Ok(s) = std::env::var("CHRONIK_MEM_BATCH_SIZE") {
        if let Ok(n) = s.parse() {
            config.max_batch_size = n;
        }
    }
    if let Ok(s) = std::env::var("CHRONIK_MEM_BATCH_WINDOW_MS") {
        if let Ok(n) = s.parse::<u64>() {
            config.max_batch_window = Duration::from_millis(n);
        }
    }

    tracing::info!(
        namespace = %namespace,
        max_batch_size = config.max_batch_size,
        max_batch_window_ms = config.max_batch_window.as_millis() as u64,
        "starting chronik-memory-worker"
    );

    let worker = Worker::new(memory, config);
    let stats = tokio::select! {
        r = worker.run() => r?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl-C received; shutting down");
            Default::default()
        }
    };

    tracing::info!(
        batches_processed = stats.batches_processed,
        batches_committed = stats.batches_committed,
        turns_consumed = stats.turns_consumed,
        memories_produced = stats.memories_produced,
        extractor_errors = stats.extractor_errors,
        produce_errors = stats.produce_errors,
        "worker stopped"
    );
    Ok(())
}

fn env_truthy(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}
