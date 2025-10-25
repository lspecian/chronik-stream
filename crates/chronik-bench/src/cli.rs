/// CLI argument parsing using clap

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "chronik-bench",
    about = "High-performance benchmark harness for Chronik",
    version
)]
pub struct Args {
    /// Kafka bootstrap servers (comma-separated)
    #[arg(
        short = 'b',
        long,
        env = "KAFKA_BOOTSTRAP_SERVERS",
        default_value = "localhost:9092"
    )]
    pub bootstrap_servers: String,

    /// Topic to produce/consume from
    #[arg(short = 't', long, env = "KAFKA_TOPIC", default_value = "chronik-bench")]
    pub topic: String,

    /// Number of concurrent producer tasks
    #[arg(short = 'c', long, default_value = "64")]
    pub concurrency: usize,

    /// Message size in bytes
    #[arg(short = 's', long, default_value = "1024")]
    pub message_size: usize,

    /// Benchmark duration (e.g., "60s", "5m", "1h")
    /// If not specified or 0, runs until interrupted
    #[arg(short = 'd', long, value_parser = parse_duration, default_value = "60s")]
    pub duration: Duration,

    /// Warmup duration before collecting metrics (e.g., "5s", "1m")
    #[arg(short = 'w', long, value_parser = parse_duration, default_value = "5s")]
    pub warmup_duration: Duration,

    /// Benchmark mode
    #[arg(short = 'm', long, default_value = "produce")]
    pub mode: BenchmarkMode,

    /// Compression codec
    #[arg(long, default_value = "none")]
    pub compression: CompressionType,

    /// Number of partitions (for topic creation)
    #[arg(short = 'p', long, default_value = "1")]
    pub partitions: i32,

    /// Replication factor (for topic creation)
    #[arg(short = 'r', long, default_value = "1")]
    pub replication_factor: i16,

    /// Acks setting for producer (0, 1, or -1/all)
    #[arg(long, default_value = "1")]
    pub acks: i32,

    /// Linger time for producer batching (milliseconds)
    #[arg(long, default_value = "0")]
    pub linger_ms: u64,

    /// Batch size for producer (bytes)
    #[arg(long, default_value = "16384")]
    pub batch_size: usize,

    /// Request timeout (milliseconds)
    #[arg(long, default_value = "30000")]
    pub request_timeout_ms: u64,

    /// Message send timeout (milliseconds)
    #[arg(long, default_value = "60000")]
    pub message_timeout_ms: u64,

    /// Report interval for periodic console updates (seconds)
    #[arg(long, default_value = "5")]
    pub report_interval_secs: u64,

    /// CSV output file path
    #[arg(long)]
    pub csv_output: Option<String>,

    /// JSON output file path
    #[arg(long)]
    pub json_output: Option<String>,

    /// Prometheus metrics port (optional)
    #[arg(long)]
    pub prometheus_port: Option<u16>,

    /// Enable verbose logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Consumer group ID (for consume mode)
    #[arg(long, default_value = "chronik-bench-consumer")]
    pub consumer_group: String,

    /// Number of messages to produce (0 = unlimited, based on duration)
    #[arg(short = 'n', long, default_value = "0")]
    pub message_count: u64,

    /// Target throughput rate (messages/sec, 0 = unlimited)
    #[arg(long, default_value = "0")]
    pub rate_limit: u64,

    /// Key pattern for messages (random, sequential, or fixed)
    #[arg(long, default_value = "random")]
    pub key_pattern: KeyPattern,

    /// Payload pattern for messages (random, zeros, or text)
    #[arg(long, default_value = "random")]
    pub payload_pattern: PayloadPattern,

    /// Create topic before benchmark (requires admin privileges)
    #[arg(long)]
    pub create_topic: bool,

    /// Delete topic after benchmark (requires admin privileges)
    #[arg(long)]
    pub delete_topic_after: bool,

    /// Validate message correctness (for round-trip tests)
    #[arg(long)]
    pub validate: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum BenchmarkMode {
    /// Producer benchmark (write only)
    Produce,
    /// Consumer benchmark (read only)
    Consume,
    /// Round-trip benchmark (produce + consume)
    RoundTrip,
    /// Metadata operations benchmark
    Metadata,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum CompressionType {
    None,
    Gzip,
    Snappy,
    Lz4,
    Zstd,
}

impl CompressionType {
    pub fn to_rdkafka_str(&self) -> &'static str {
        match self {
            CompressionType::None => "none",
            CompressionType::Gzip => "gzip",
            CompressionType::Snappy => "snappy",
            CompressionType::Lz4 => "lz4",
            CompressionType::Zstd => "zstd",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum KeyPattern {
    Random,
    Sequential,
    Fixed,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize)]
pub enum PayloadPattern {
    Random,
    Zeros,
    Text,
}

/// Parse duration from string (e.g., "60s", "5m", "1h")
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Duration::from_secs(0));
    }

    let (value_str, unit) = if let Some(pos) = s.chars().position(|c| c.is_alphabetic()) {
        (&s[..pos], &s[pos..])
    } else {
        // No unit specified, assume seconds
        return s
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| format!("Invalid duration value: {}", e));
    };

    let value: u64 = value_str
        .parse()
        .map_err(|e| format!("Invalid duration value: {}", e))?;

    match unit.to_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => Ok(Duration::from_secs(value)),
        "m" | "min" | "mins" | "minute" | "minutes" => Ok(Duration::from_secs(value * 60)),
        "h" | "hr" | "hrs" | "hour" | "hours" => Ok(Duration::from_secs(value * 3600)),
        "ms" | "millis" | "millisecond" | "milliseconds" => Ok(Duration::from_millis(value)),
        _ => Err(format!("Unknown duration unit: {}", unit)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("120").unwrap(), Duration::from_secs(120));
    }
}
