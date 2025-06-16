//! OpenTelemetry tracing configuration.

use anyhow::Result;
use opentelemetry::{
    global,
    KeyValue,
};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    runtime,
    trace::{self, RandomIdGenerator, Sampler},
    Resource,
};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use tracing_subscriber::{
    filter::EnvFilter,
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

/// Tracing configuration
#[derive(Debug, Clone)]
pub struct TracingConfig {
    /// OTLP endpoint
    pub otlp_endpoint: String,
    
    /// Service version
    pub service_version: String,
    
    /// Sampling ratio (0.0-1.0)
    pub sampling_ratio: f64,
    
    /// Log level filter
    pub log_level: String,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: "http://localhost:4317".to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            sampling_ratio: 1.0,
            log_level: "info".to_string(),
        }
    }
}

/// Initialize tracing with OpenTelemetry
pub fn init_tracing(service_name: &str, config: TracingConfig) -> Result<()> {
    // Set global propagator
    global::set_text_map_propagator(TraceContextPropagator::new());
    
    // Create OTLP tracer
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(&config.otlp_endpoint)
                .with_protocol(Protocol::Grpc)
        )
        .with_trace_config(
            trace::config()
                .with_sampler(Sampler::TraceIdRatioBased(config.sampling_ratio))
                .with_id_generator(RandomIdGenerator::default())
                .with_resource(Resource::new(vec![
                    KeyValue::new("service.name", service_name.to_string()),
                    KeyValue::new("service.version", config.service_version),
                ]))
        )
        .install_batch(runtime::Tokio)?;
    
    // Create telemetry layer
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    
    // Create fmt layer for console output
    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true);
    
    // Create env filter
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.log_level));
    
    // Build subscriber
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(telemetry)
        .init();
    
    Ok(())
}

/// Shutdown tracing
pub fn shutdown_tracing() {
    global::shutdown_tracer_provider();
}