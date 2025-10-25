/// Chronik Benchmark Harness
///
/// High-performance load testing tool for Chronik streaming platform.
/// Measures throughput (msgs/sec), latency percentiles (p50-p99.9), and tail behavior.

mod benchmark;
mod cli;
mod metrics;
mod reporter;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::{
    benchmark::BenchmarkRunner,
    cli::Args,
    reporter::{ConsoleReporter, CsvReporter, Reporter},
};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    // Parse CLI arguments
    let args = Args::parse();

    info!("Chronik Benchmark Harness v{}", env!("CARGO_PKG_VERSION"));
    info!("Configuration:");
    info!("  Bootstrap servers: {}", args.bootstrap_servers);
    info!("  Topic: {}", args.topic);
    info!("  Concurrency: {}", args.concurrency);
    info!("  Message size: {} bytes", args.message_size);
    info!("  Duration: {:?}", args.duration);
    info!("  Warmup: {:?}", args.warmup_duration);
    info!("  Compression: {:?}", args.compression);

    // Validate arguments
    if args.concurrency == 0 {
        anyhow::bail!("Concurrency must be > 0");
    }
    if args.message_size == 0 {
        anyhow::bail!("Message size must be > 0");
    }
    if args.duration.as_secs() == 0 {
        warn!("Duration is 0, benchmark will run indefinitely until Ctrl+C");
    }

    // Create benchmark runner
    let mut runner = BenchmarkRunner::new(args.clone())?;

    // Start Prometheus exporter if enabled
    #[cfg(feature = "prometheus")]
    let _prometheus_handle = if args.prometheus_port.is_some() {
        Some(crate::metrics::start_prometheus_exporter(args.prometheus_port.unwrap()).await?)
    } else {
        None
    };

    // Run benchmark
    info!("Starting benchmark...");
    let results = runner.run().await?;

    // Report results
    info!("Benchmark complete, generating reports...");

    // Console report
    let console_reporter = ConsoleReporter;
    console_reporter.report(&results)?;

    // CSV report
    if let Some(csv_path) = &args.csv_output {
        let csv_reporter = CsvReporter::new(csv_path)?;
        csv_reporter.report(&results)?;
        info!("CSV output written to: {}", csv_path);
    }

    // JSON report
    if let Some(json_path) = &args.json_output {
        let json = serde_json::to_string_pretty(&results)?;
        std::fs::write(json_path, json)?;
        info!("JSON output written to: {}", json_path);
    }

    Ok(())
}
