/// Result reporting and formatting

use anyhow::Result;
use chrono::Utc;
use csv::Writer;
use serde::{Deserialize, Serialize};
use std::{fs::File, time::Duration};

use crate::cli::{BenchmarkMode, CompressionType};

/// Benchmark results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResults {
    pub mode: BenchmarkMode,
    pub duration: Duration,
    pub total_messages: u64,
    pub failed_messages: u64,
    pub total_bytes: u64,
    pub latency_p50_us: u64,
    pub latency_p90_us: u64,
    pub latency_p95_us: u64,
    pub latency_p99_us: u64,
    pub latency_p999_us: u64,
    pub latency_max_us: u64,
    pub message_size: usize,
    pub concurrency: usize,
    pub compression: CompressionType,
}

impl BenchmarkResults {
    /// Calculate throughput in messages per second
    pub fn throughput_msg_per_sec(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs > 0.0 {
            self.total_messages as f64 / secs
        } else {
            0.0
        }
    }

    /// Calculate throughput in MB per second
    pub fn throughput_mb_per_sec(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs > 0.0 {
            (self.total_bytes as f64 / secs) / (1024.0 * 1024.0)
        } else {
            0.0
        }
    }

    /// Calculate success rate
    pub fn success_rate(&self) -> f64 {
        let total = self.total_messages + self.failed_messages;
        if total > 0 {
            (self.total_messages as f64 / total as f64) * 100.0
        } else {
            0.0
        }
    }
}

/// Reporter trait
pub trait Reporter {
    fn report(&self, results: &BenchmarkResults) -> Result<()>;
}

/// Console reporter
pub struct ConsoleReporter;

impl Reporter for ConsoleReporter {
    fn report(&self, results: &BenchmarkResults) -> Result<()> {
        println!("\n╔══════════════════════════════════════════════════════════════╗");
        println!("║            Chronik Benchmark Results                        ║");
        println!("╠══════════════════════════════════════════════════════════════╣");
        println!("║ Mode:             {:?}", results.mode);
        println!("║ Duration:         {:.2}s", results.duration.as_secs_f64());
        println!("║ Concurrency:      {}", results.concurrency);
        println!("║ Compression:      {:?}", results.compression);
        println!("╠══════════════════════════════════════════════════════════════╣");
        println!("║ THROUGHPUT                                                   ║");
        println!("╠══════════════════════════════════════════════════════════════╣");
        println!(
            "║ Messages:         {:>12} total",
            format_number(results.total_messages)
        );
        println!(
            "║ Failed:           {:>12} ({:.2}%)",
            format_number(results.failed_messages),
            100.0 - results.success_rate()
        );
        println!(
            "║ Data transferred: {:>12}",
            format_bytes(results.total_bytes)
        );
        println!(
            "║ Message rate:     {:>12} msg/s",
            format_number(results.throughput_msg_per_sec() as u64)
        );
        println!(
            "║ Bandwidth:        {:>12} MB/s",
            format!("{:.2}", results.throughput_mb_per_sec())
        );
        println!("╠══════════════════════════════════════════════════════════════╣");
        println!("║ LATENCY (microseconds → milliseconds)                       ║");
        println!("╠══════════════════════════════════════════════════════════════╣");
        println!(
            "║ p50:              {:>12} μs  ({:>8.2} ms)",
            format_number(results.latency_p50_us),
            results.latency_p50_us as f64 / 1000.0
        );
        println!(
            "║ p90:              {:>12} μs  ({:>8.2} ms)",
            format_number(results.latency_p90_us),
            results.latency_p90_us as f64 / 1000.0
        );
        println!(
            "║ p95:              {:>12} μs  ({:>8.2} ms)",
            format_number(results.latency_p95_us),
            results.latency_p95_us as f64 / 1000.0
        );
        println!(
            "║ p99:              {:>12} μs  ({:>8.2} ms)",
            format_number(results.latency_p99_us),
            results.latency_p99_us as f64 / 1000.0
        );
        println!(
            "║ p99.9:            {:>12} μs  ({:>8.2} ms)",
            format_number(results.latency_p999_us),
            results.latency_p999_us as f64 / 1000.0
        );
        println!(
            "║ max:              {:>12} μs  ({:>8.2} ms)",
            format_number(results.latency_max_us),
            results.latency_max_us as f64 / 1000.0
        );
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!();

        Ok(())
    }
}

/// CSV reporter
pub struct CsvReporter {
    path: String,
}

impl CsvReporter {
    pub fn new(path: &str) -> Result<Self> {
        Ok(Self {
            path: path.to_string(),
        })
    }
}

impl Reporter for CsvReporter {
    fn report(&self, results: &BenchmarkResults) -> Result<()> {
        let file = File::create(&self.path)?;
        let mut writer = Writer::from_writer(file);

        // Write header
        writer.write_record(&[
            "timestamp",
            "mode",
            "duration_secs",
            "total_messages",
            "failed_messages",
            "total_bytes",
            "throughput_msg_per_sec",
            "throughput_mb_per_sec",
            "success_rate_pct",
            "latency_p50_us",
            "latency_p90_us",
            "latency_p95_us",
            "latency_p99_us",
            "latency_p999_us",
            "latency_max_us",
            "latency_p50_ms",
            "latency_p90_ms",
            "latency_p95_ms",
            "latency_p99_ms",
            "latency_p999_ms",
            "latency_max_ms",
            "message_size",
            "concurrency",
            "compression",
        ])?;

        // Write data
        writer.write_record(&[
            Utc::now().to_rfc3339(),
            format!("{:?}", results.mode),
            results.duration.as_secs_f64().to_string(),
            results.total_messages.to_string(),
            results.failed_messages.to_string(),
            results.total_bytes.to_string(),
            results.throughput_msg_per_sec().to_string(),
            results.throughput_mb_per_sec().to_string(),
            results.success_rate().to_string(),
            results.latency_p50_us.to_string(),
            results.latency_p90_us.to_string(),
            results.latency_p95_us.to_string(),
            results.latency_p99_us.to_string(),
            results.latency_p999_us.to_string(),
            results.latency_max_us.to_string(),
            (results.latency_p50_us as f64 / 1000.0).to_string(),
            (results.latency_p90_us as f64 / 1000.0).to_string(),
            (results.latency_p95_us as f64 / 1000.0).to_string(),
            (results.latency_p99_us as f64 / 1000.0).to_string(),
            (results.latency_p999_us as f64 / 1000.0).to_string(),
            (results.latency_max_us as f64 / 1000.0).to_string(),
            results.message_size.to_string(),
            results.concurrency.to_string(),
            format!("{:?}", results.compression),
        ])?;

        writer.flush()?;

        Ok(())
    }
}

/// Format number with thousands separators
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let mut count = 0;

    for c in s.chars().rev() {
        if count > 0 && count % 3 == 0 {
            result.push(',');
        }
        result.push(c);
        count += 1;
    }

    result.chars().rev().collect()
}

/// Format bytes with units (KB, MB, GB)
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let bytes_f = bytes as f64;

    if bytes_f >= GB {
        format!("{:.2} GB", bytes_f / GB)
    } else if bytes_f >= MB {
        format!("{:.2} MB", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.2} KB", bytes_f / KB)
    } else {
        format!("{} bytes", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(123), "123");
        assert_eq!(format_number(1234), "1,234");
        assert_eq!(format_number(1234567), "1,234,567");
        assert_eq!(format_number(1234567890), "1,234,567,890");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(500), "500 bytes");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
    }
}
