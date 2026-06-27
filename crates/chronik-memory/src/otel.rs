//! OpenTelemetry / tracing setup guidance for `chronik-memory`.
//!
//! The SDK uses the [`tracing`] crate throughout — every public hot-path
//! method is wrapped in a `#[tracing::instrument]` span, and key events emit
//! structured `tracing::info!` / `tracing::warn!` records. We do **not** ship a
//! tracer/exporter dependency; customers wire up their own collector.
//!
//! # Span inventory
//!
//! | Span | Source | Key fields |
//! |------|--------|-----------|
//! | `Memory::ingest_turn` | `client.rs` | `topic`, `partition`, `offset`, `deduped` |
//! | `Memory::ingest_with_extraction` | `client.rs` | `n_turns`, `n_extracted` |
//! | `Memory::remember` | `client.rs` | `kind`, `key` |
//! | `Memory::forget` | `client.rs` | `topic`, `key` |
//! | `Memory::recall` | `recall.rs` (via `RecallBuilder::send`) | `query`, `k`, `n_results` |
//! | `Worker::run` | `worker.rs` | `namespace`, `group_id` |
//! | `AnthropicExtractor::extract` | `providers/anthropic.rs` | `model`, `n_turns` |
//! | `OpenAIExtractor::extract` | `providers/openai.rs` | `model`, `n_turns` |
//!
//! # Recommended customer setup
//!
//! ```ignore
//! // Customer adds these deps in their app's Cargo.toml — we don't ship them.
//! use opentelemetry::global;
//! use opentelemetry_otlp::WithExportConfig;
//! use tracing_subscriber::layer::SubscriberExt;
//! use tracing_subscriber::util::SubscriberInitExt;
//!
//! fn init_otel() -> Result<(), Box<dyn std::error::Error>> {
//!     let tracer = opentelemetry_otlp::new_pipeline()
//!         .tracing()
//!         .with_exporter(
//!             opentelemetry_otlp::new_exporter()
//!                 .tonic()
//!                 .with_endpoint(std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")?)
//!         )
//!         .install_batch(opentelemetry_sdk::runtime::Tokio)?;
//!
//!     tracing_subscriber::registry()
//!         .with(tracing_subscriber::EnvFilter::from_default_env())
//!         .with(tracing_opentelemetry::layer().with_tracer(tracer))
//!         .init();
//!     Ok(())
//! }
//! ```
//!
//! # Why no metrics crate
//!
//! Metrics (counters, gauges, histograms for latency p50/p99) are not shipped
//! from this crate. Customers who want Prometheus-style metrics should derive
//! them in their OTEL collector pipeline from the spans we already emit
//! (every span is timed; latency histograms are a one-line collector
//! transformation). This avoids hardcoding a `metrics` crate dependency that
//! would become a customer-facing version constraint.
//!
//! Phase 3 may revisit if customers ask for metric gauges out of the box.
