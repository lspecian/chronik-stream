//! Columnar storage configuration.
//!
//! Configuration is extracted from topic config HashMap keys prefixed with `columnar.`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Columnar storage configuration for a topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnarConfig {
    /// Whether columnar storage is enabled for this topic.
    pub enabled: bool,

    /// Output format (parquet, arrow).
    pub format: ColumnarFormat,

    /// Compression codec.
    pub compression: CompressionCodec,

    /// Compression level (1-22 for zstd).
    pub compression_level: u8,

    /// Number of rows per row group.
    pub row_group_size: usize,

    /// Page size in bytes.
    pub page_size: usize,

    /// Enable bloom filters.
    pub bloom_filter_enabled: bool,

    /// Columns to include in bloom filter.
    pub bloom_filter_columns: Vec<String>,

    /// Enable dictionary encoding.
    pub dictionary_enabled: bool,

    /// Statistics collection level.
    pub statistics: StatisticsLevel,

    /// Time-based partitioning strategy.
    pub partitioning: PartitioningStrategy,
}

impl Default for ColumnarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            format: ColumnarFormat::Parquet,
            compression: CompressionCodec::Zstd,
            compression_level: 3,
            row_group_size: 100_000,
            page_size: 1_048_576, // 1MB
            bloom_filter_enabled: true,
            bloom_filter_columns: vec!["_offset".to_string(), "_timestamp".to_string()],
            dictionary_enabled: true,
            statistics: StatisticsLevel::Full,
            partitioning: PartitioningStrategy::None,
        }
    }
}

impl ColumnarConfig {
    /// Parse columnar configuration from topic config HashMap.
    ///
    /// Keys are expected to be prefixed with `columnar.`, e.g.:
    /// - `columnar.enabled` = "true"
    /// - `columnar.format` = "parquet"
    /// - `columnar.compression` = "zstd"
    pub fn from_topic_config(config: &HashMap<String, String>) -> Result<Self> {
        let mut result = Self::default();

        // columnar.enabled
        if let Some(v) = config.get("columnar.enabled") {
            result.enabled = v.eq_ignore_ascii_case("true");
        }

        // columnar.format
        if let Some(v) = config.get("columnar.format") {
            result.format = v.parse()?;
        }

        // columnar.compression
        if let Some(v) = config.get("columnar.compression") {
            result.compression = v.parse()?;
        }

        // columnar.compression.level
        if let Some(v) = config.get("columnar.compression.level") {
            result.compression_level = v.parse()
                .map_err(|_| anyhow!("Invalid compression level: {}", v))?;
            if result.compression_level > 22 {
                return Err(anyhow!("Compression level must be 1-22, got {}", result.compression_level));
            }
        }

        // columnar.row_group_size
        if let Some(v) = config.get("columnar.row_group_size") {
            result.row_group_size = v.parse()
                .map_err(|_| anyhow!("Invalid row_group_size: {}", v))?;
            if result.row_group_size < 1000 {
                return Err(anyhow!("row_group_size must be at least 1000, got {}", result.row_group_size));
            }
        }

        // columnar.page_size
        if let Some(v) = config.get("columnar.page_size") {
            result.page_size = v.parse()
                .map_err(|_| anyhow!("Invalid page_size: {}", v))?;
        }

        // columnar.bloom_filter
        if let Some(v) = config.get("columnar.bloom_filter") {
            result.bloom_filter_enabled = v.eq_ignore_ascii_case("true");
        }

        // columnar.bloom_filter.columns
        if let Some(v) = config.get("columnar.bloom_filter.columns") {
            result.bloom_filter_columns = v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        // columnar.dictionary
        if let Some(v) = config.get("columnar.dictionary") {
            result.dictionary_enabled = v.eq_ignore_ascii_case("true");
        }

        // columnar.statistics
        if let Some(v) = config.get("columnar.statistics") {
            result.statistics = v.parse()?;
        }

        // columnar.partitioning
        if let Some(v) = config.get("columnar.partitioning") {
            result.partitioning = v.parse()?;
        }

        Ok(result)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        if self.compression_level > 22 {
            return Err(anyhow!("Compression level must be 1-22"));
        }

        if self.row_group_size < 1000 {
            return Err(anyhow!("row_group_size must be at least 1000"));
        }

        if self.page_size < 1024 {
            return Err(anyhow!("page_size must be at least 1024"));
        }

        Ok(())
    }

    /// Get all valid columnar config keys for validation.
    pub fn valid_keys() -> &'static [&'static str] {
        &[
            "columnar.enabled",
            "columnar.format",
            "columnar.compression",
            "columnar.compression.level",
            "columnar.row_group_size",
            "columnar.page_size",
            "columnar.bloom_filter",
            "columnar.bloom_filter.columns",
            "columnar.dictionary",
            "columnar.dictionary.columns",
            "columnar.statistics",
            "columnar.partitioning",
        ]
    }
}

/// Columnar output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColumnarFormat {
    #[default]
    Parquet,
    Arrow,
}

impl std::str::FromStr for ColumnarFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "parquet" => Ok(Self::Parquet),
            "arrow" => Ok(Self::Arrow),
            _ => Err(anyhow!("Invalid columnar format: {}. Valid: parquet, arrow", s)),
        }
    }
}

impl std::fmt::Display for ColumnarFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parquet => write!(f, "parquet"),
            Self::Arrow => write!(f, "arrow"),
        }
    }
}

/// Compression codec for columnar files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompressionCodec {
    None,
    Snappy,
    Gzip,
    Lz4,
    #[default]
    Zstd,
}

impl std::str::FromStr for CompressionCodec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "snappy" => Ok(Self::Snappy),
            "gzip" => Ok(Self::Gzip),
            "lz4" => Ok(Self::Lz4),
            "zstd" => Ok(Self::Zstd),
            _ => Err(anyhow!("Invalid compression: {}. Valid: none, snappy, gzip, lz4, zstd", s)),
        }
    }
}

impl std::fmt::Display for CompressionCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Snappy => write!(f, "snappy"),
            Self::Gzip => write!(f, "gzip"),
            Self::Lz4 => write!(f, "lz4"),
            Self::Zstd => write!(f, "zstd"),
        }
    }
}

/// Statistics collection level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StatisticsLevel {
    /// No statistics.
    None,
    /// Statistics per chunk/page.
    Chunk,
    /// Full statistics including column-level.
    #[default]
    Full,
}

impl std::str::FromStr for StatisticsLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "chunk" => Ok(Self::Chunk),
            "full" => Ok(Self::Full),
            _ => Err(anyhow!("Invalid statistics level: {}. Valid: none, chunk, full", s)),
        }
    }
}

/// Time-based partitioning strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PartitioningStrategy {
    /// No time-based partitioning.
    #[default]
    None,
    /// Partition by hour.
    Hourly,
    /// Partition by day.
    Daily,
}

impl std::str::FromStr for PartitioningStrategy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "hourly" => Ok(Self::Hourly),
            "daily" => Ok(Self::Daily),
            _ => Err(anyhow!("Invalid partitioning: {}. Valid: none, hourly, daily", s)),
        }
    }
}

impl std::fmt::Display for PartitioningStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Hourly => write!(f, "hourly"),
            Self::Daily => write!(f, "daily"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ColumnarConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.format, ColumnarFormat::Parquet);
        assert_eq!(config.compression, CompressionCodec::Zstd);
        assert_eq!(config.row_group_size, 100_000);
    }

    #[test]
    fn test_from_topic_config() {
        let mut topic_config = HashMap::new();
        topic_config.insert("columnar.enabled".to_string(), "true".to_string());
        topic_config.insert("columnar.format".to_string(), "parquet".to_string());
        topic_config.insert("columnar.compression".to_string(), "snappy".to_string());
        topic_config.insert("columnar.row_group_size".to_string(), "50000".to_string());

        let config = ColumnarConfig::from_topic_config(&topic_config).unwrap();
        assert!(config.enabled);
        assert_eq!(config.format, ColumnarFormat::Parquet);
        assert_eq!(config.compression, CompressionCodec::Snappy);
        assert_eq!(config.row_group_size, 50_000);
    }

    #[test]
    fn test_bloom_filter_columns() {
        let mut topic_config = HashMap::new();
        topic_config.insert("columnar.bloom_filter.columns".to_string(), "offset, timestamp, key".to_string());

        let config = ColumnarConfig::from_topic_config(&topic_config).unwrap();
        assert_eq!(config.bloom_filter_columns, vec!["offset", "timestamp", "key"]);
    }

    #[test]
    fn test_invalid_compression_level() {
        let mut topic_config = HashMap::new();
        topic_config.insert("columnar.compression.level".to_string(), "30".to_string());

        let result = ColumnarConfig::from_topic_config(&topic_config);
        assert!(result.is_err());
    }

    #[test]
    fn test_format_parsing() {
        assert_eq!("parquet".parse::<ColumnarFormat>().unwrap(), ColumnarFormat::Parquet);
        assert_eq!("arrow".parse::<ColumnarFormat>().unwrap(), ColumnarFormat::Arrow);
        assert!("invalid".parse::<ColumnarFormat>().is_err());
    }

    #[test]
    fn test_compression_parsing() {
        assert_eq!("zstd".parse::<CompressionCodec>().unwrap(), CompressionCodec::Zstd);
        assert_eq!("snappy".parse::<CompressionCodec>().unwrap(), CompressionCodec::Snappy);
        assert_eq!("none".parse::<CompressionCodec>().unwrap(), CompressionCodec::None);
    }

    #[test]
    fn test_partitioning_parsing() {
        assert_eq!("none".parse::<PartitioningStrategy>().unwrap(), PartitioningStrategy::None);
        assert_eq!("hourly".parse::<PartitioningStrategy>().unwrap(), PartitioningStrategy::Hourly);
        assert_eq!("daily".parse::<PartitioningStrategy>().unwrap(), PartitioningStrategy::Daily);
    }
}
