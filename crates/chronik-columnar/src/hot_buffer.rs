//! Hot Buffer for In-Memory SQL Queries (v2.2.23)
//!
//! Provides sub-second SQL query latency by maintaining an in-memory cache
//! of recent records that haven't yet been written to Parquet files.
//!
//! ## Architecture
//!
//! ```text
//! Producer → WAL (sync) → HotDataBuffer (in-memory) → Immediately queryable
//!                      → [30-60s] → Parquet files → Cold data queryable
//!
//! SQL Query = UNION(hot_data, cold_data) with offset-based deduplication
//! ```
//!
//! ## How It Works
//!
//! 1. HotDataBuffer reads recent records from WAL using WalManager.read_from()
//! 2. Deserializes CanonicalRecord from WAL V2 format
//! 3. Converts each record to Arrow RecordBatch (using DataFusion's arrow types)
//! 4. Creates a DataFusion MemTable for zero-copy querying
//!
//! ## Configuration
//!
//! ```bash
//! CHRONIK_HOT_BUFFER_ENABLED=true           # default: true
//! CHRONIK_HOT_BUFFER_MAX_RECORDS=100000     # default: 100,000 per partition
//! CHRONIK_HOT_BUFFER_REFRESH_MS=1000        # default: 1000ms
//! ```

use std::sync::Arc;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use tracing::{debug, trace};

// Use DataFusion's re-exported arrow types to avoid version conflicts
// DataFusion 44 uses arrow 53.x, while direct arrow deps are 54.x
use datafusion::arrow::array::{
    ArrayRef, BinaryBuilder, Int32Array, Int64Array, Int8Array, RecordBatch, StringBuilder,
    TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use datafusion::datasource::MemTable;
use serde::{Deserialize, Serialize};

use chronik_wal::{WalManager, WalRecord};

// ============================================================================
// Local CanonicalRecord types for bincode deserialization
// (Copied from chronik-storage to avoid cyclic dependency)
// ============================================================================

/// Compression types supported by Kafka
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum CompressionType {
    None = 0,
    Gzip = 1,
    Snappy = 2,
    Lz4 = 3,
    Zstd = 4,
}

/// Timestamp type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum TimestampType {
    CreateTime = 0,
    LogAppendTime = 1,
}

/// Record header (key-value pair)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordHeader {
    key: String,
    value: Option<Vec<u8>>,
}

/// Single record entry within a batch
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CanonicalRecordEntry {
    offset: i64,
    timestamp: i64,
    key: Option<Vec<u8>>,
    value: Option<Vec<u8>>,
    headers: Vec<RecordHeader>,
    attributes: i8,
}

/// Canonical internal record format (bincode-compatible with chronik-storage)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CanonicalRecord {
    base_offset: i64,
    partition_leader_epoch: i32,
    producer_id: i64,
    producer_epoch: i16,
    base_sequence: i32,
    is_transactional: bool,
    is_control: bool,
    compression: CompressionType,
    timestamp_type: TimestampType,
    base_timestamp: i64,
    max_timestamp: i64,
    records: Vec<CanonicalRecordEntry>,
    compressed_records_wire_bytes: Option<Vec<u8>>,
    original_v1_wire_format: Option<Vec<u8>>,
    original_v2_wire_format: Option<Vec<u8>>,
}

/// Configuration for the hot buffer
#[derive(Debug, Clone)]
pub struct HotBufferConfig {
    /// Whether hot buffer is enabled
    pub enabled: bool,
    /// Maximum records to include in hot buffer per partition
    pub max_records_per_partition: usize,
    /// Refresh interval in milliseconds (how often to check for new records)
    pub refresh_interval_ms: u64,
}

impl Default for HotBufferConfig {
    fn default() -> Self {
        Self {
            enabled: std::env::var("CHRONIK_HOT_BUFFER_ENABLED")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(true), // Enabled by default
            max_records_per_partition: std::env::var("CHRONIK_HOT_BUFFER_MAX_RECORDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100_000),
            refresh_interval_ms: std::env::var("CHRONIK_HOT_BUFFER_REFRESH_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000),
        }
    }
}

/// Key for topic-partition identification
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

impl TopicPartition {
    pub fn new(topic: impl Into<String>, partition: i32) -> Self {
        Self {
            topic: topic.into(),
            partition,
        }
    }
}

/// Cached RecordBatch for a topic-partition
#[derive(Debug, Clone)]
pub struct CachedBatch {
    /// The Arrow RecordBatch containing hot data
    pub batch: RecordBatch,
    /// The minimum offset in this batch
    pub min_offset: i64,
    /// The maximum offset in this batch
    pub max_offset: i64,
    /// Number of records in the batch
    pub record_count: usize,
    /// Last refresh timestamp (Unix millis)
    pub last_refresh_ms: u64,
}

/// Simple record representation for conversion to Arrow
struct HotRecord {
    topic: String,
    partition: i32,
    offset: i64,
    timestamp_ms: i64,
    timestamp_type: i8,
    key: Option<Vec<u8>>,
    value: Vec<u8>,
}

/// Hot Data Buffer for in-memory SQL queries
///
/// Reads recent records from WAL and converts them to Arrow format
/// for immediate SQL querying before Parquet files are created.
pub struct HotDataBuffer {
    /// WAL manager to read recent records from
    wal_manager: Arc<WalManager>,
    /// Cached batches per topic-partition
    cache: DashMap<TopicPartition, CachedBatch>,
    /// Configuration
    config: HotBufferConfig,
    /// Flushed offsets per topic-partition (data already in Parquet)
    /// This is updated by the WalIndexer when Parquet files are created
    flushed_offsets: DashMap<TopicPartition, i64>,
}

impl HotDataBuffer {
    /// Create a new hot data buffer
    pub fn new(wal_manager: Arc<WalManager>, config: HotBufferConfig) -> Self {
        Self {
            wal_manager,
            cache: DashMap::new(),
            config,
            flushed_offsets: DashMap::new(),
        }
    }

    /// Check if hot buffer is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Create the simplified Kafka message schema using DataFusion's arrow types
    fn hot_buffer_schema() -> Schema {
        Schema::new(vec![
            Field::new("_topic", DataType::Utf8, false),
            Field::new("_partition", DataType::Int32, false),
            Field::new("_offset", DataType::Int64, false),
            Field::new(
                "_timestamp",
                DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
                false,
            ),
            Field::new("_timestamp_type", DataType::Int8, false),
            Field::new("_key", DataType::Binary, true),
            Field::new("_value", DataType::Binary, false),
        ])
    }

    /// Get the hot data as a MemTable for DataFusion
    ///
    /// Returns None if the topic-partition has no hot data or hot buffer is disabled.
    pub async fn get_mem_table(&self, topic: &str, partition: i32) -> Result<Option<MemTable>> {
        if !self.config.enabled {
            return Ok(None);
        }

        let tp = TopicPartition::new(topic, partition);

        // Get the flushed offset (data already in Parquet)
        let flushed_offset = self.flushed_offsets.get(&tp).map(|v| *v).unwrap_or(0);

        // Read recent records from WAL starting from flushed offset
        let wal_records = self
            .wal_manager
            .read_from(topic, partition, flushed_offset, self.config.max_records_per_partition)
            .await
            .map_err(|e| anyhow!("Failed to read WAL records: {}", e))?;

        if wal_records.is_empty() {
            debug!(
                "No hot data for {}-{} (flushed_offset={})",
                topic, partition, flushed_offset
            );
            return Ok(None);
        }

        // Convert WAL records to simple HotRecord format
        let hot_records = self.wal_records_to_hot_records(topic, partition, &wal_records)?;

        if hot_records.is_empty() {
            return Ok(None);
        }

        // Convert to Arrow RecordBatch using DataFusion's arrow types
        let batch = self.records_to_batch(&hot_records)?;

        debug!(
            "Created hot buffer for {}-{}: {} records, offsets {}..{}",
            topic,
            partition,
            hot_records.len(),
            hot_records.first().map(|r| r.offset).unwrap_or(0),
            hot_records.last().map(|r| r.offset).unwrap_or(0),
        );

        // Create MemTable from the batch
        let schema = Arc::new(Self::hot_buffer_schema());
        let mem_table = MemTable::try_new(schema, vec![vec![batch]])?;

        Ok(Some(mem_table))
    }

    /// Get hot data for all partitions of a topic
    ///
    /// Returns a combined MemTable containing data from all partitions.
    pub async fn get_topic_mem_table(&self, topic: &str) -> Result<Option<MemTable>> {
        if !self.config.enabled {
            return Ok(None);
        }

        // Get all partitions for this topic from WAL manager
        // Filter the global partition list by topic name
        let all_partitions = self.wal_manager.get_partitions();
        let partitions: Vec<i32> = all_partitions
            .into_iter()
            .filter(|tp| tp.topic == topic)
            .map(|tp| tp.partition)
            .collect();

        if partitions.is_empty() {
            return Ok(None);
        }

        let mut all_batches: Vec<RecordBatch> = Vec::new();

        for partition in partitions {
            let tp = TopicPartition::new(topic, partition);
            let flushed_offset = self.flushed_offsets.get(&tp).map(|v| *v).unwrap_or(0);

            let wal_records = self
                .wal_manager
                .read_from(topic, partition, flushed_offset, self.config.max_records_per_partition)
                .await
                .map_err(|e| anyhow!("Failed to read WAL records: {}", e))?;

            if wal_records.is_empty() {
                continue;
            }

            let hot_records = self.wal_records_to_hot_records(topic, partition, &wal_records)?;
            if hot_records.is_empty() {
                continue;
            }

            let batch = self.records_to_batch(&hot_records)?;
            all_batches.push(batch);
        }

        if all_batches.is_empty() {
            return Ok(None);
        }

        let schema = Arc::new(Self::hot_buffer_schema());
        let mem_table = MemTable::try_new(schema, vec![all_batches])?;

        Ok(Some(mem_table))
    }

    /// Set the flushed offset for a topic-partition
    ///
    /// Called by WalIndexer when Parquet files are created.
    /// Records up to this offset are in Parquet and don't need to be in hot buffer.
    pub fn set_flushed_offset(&self, topic: &str, partition: i32, offset: i64) {
        let tp = TopicPartition::new(topic, partition);
        self.flushed_offsets.insert(tp, offset);
        debug!("Set flushed offset for {}-{}: {}", topic, partition, offset);
    }

    /// Get the flushed offset for a topic-partition
    pub fn get_flushed_offset(&self, topic: &str, partition: i32) -> i64 {
        let tp = TopicPartition::new(topic, partition);
        self.flushed_offsets.get(&tp).map(|v| *v).unwrap_or(0)
    }

    /// Convert HotRecords to Arrow RecordBatch using DataFusion's arrow types
    fn records_to_batch(&self, records: &[HotRecord]) -> Result<RecordBatch> {
        if records.is_empty() {
            return Err(anyhow!("Cannot convert empty record batch"));
        }

        // Build topic column
        let mut topic_builder = StringBuilder::new();
        for record in records {
            topic_builder.append_value(&record.topic);
        }
        let topics: ArrayRef = Arc::new(topic_builder.finish());

        // Build partition column
        let partitions: ArrayRef = Arc::new(Int32Array::from(
            records.iter().map(|r| r.partition).collect::<Vec<_>>(),
        ));

        // Build offset column
        let offsets: ArrayRef = Arc::new(Int64Array::from(
            records.iter().map(|r| r.offset).collect::<Vec<_>>(),
        ));

        // Build timestamp column with timezone
        let mut ts_builder = TimestampMillisecondBuilder::new().with_timezone("UTC");
        for record in records {
            ts_builder.append_value(record.timestamp_ms);
        }
        let timestamps: ArrayRef = Arc::new(ts_builder.finish());

        // Build timestamp_type column
        let timestamp_types: ArrayRef = Arc::new(Int8Array::from(
            records.iter().map(|r| r.timestamp_type).collect::<Vec<_>>(),
        ));

        // Build key column (nullable binary)
        let mut key_builder = BinaryBuilder::new();
        for record in records {
            match &record.key {
                Some(k) => key_builder.append_value(k),
                None => key_builder.append_null(),
            }
        }
        let keys: ArrayRef = Arc::new(key_builder.finish());

        // Build value column
        let mut value_builder = BinaryBuilder::new();
        for record in records {
            value_builder.append_value(&record.value);
        }
        let values: ArrayRef = Arc::new(value_builder.finish());

        let schema = Arc::new(Self::hot_buffer_schema());
        let columns = vec![topics, partitions, offsets, timestamps, timestamp_types, keys, values];

        RecordBatch::try_new(schema, columns)
            .map_err(|e| anyhow!("Failed to create RecordBatch: {}", e))
    }

    /// Convert WAL records to HotRecord format for Arrow conversion
    fn wal_records_to_hot_records(
        &self,
        topic: &str,
        partition: i32,
        wal_records: &[WalRecord],
    ) -> Result<Vec<HotRecord>> {
        let mut hot_records = Vec::new();

        for wal_record in wal_records {
            match wal_record {
                WalRecord::V2 {
                    canonical_data,
                    topic: wal_topic,
                    partition: wal_partition,
                    ..
                } => {
                    // Verify topic/partition match
                    if wal_topic != topic || *wal_partition != partition {
                        trace!(
                            "Skipping WAL record for {}-{} (looking for {}-{})",
                            wal_topic,
                            wal_partition,
                            topic,
                            partition
                        );
                        continue;
                    }

                    // Deserialize CanonicalRecord from bincode
                    let canonical: CanonicalRecord = bincode::deserialize(canonical_data)
                        .map_err(|e| anyhow!("Failed to deserialize CanonicalRecord: {}", e))?;

                    // Convert each record entry to HotRecord
                    for entry in &canonical.records {
                        let hot_record = HotRecord {
                            topic: topic.to_string(),
                            partition,
                            offset: entry.offset,
                            timestamp_ms: entry.timestamp,
                            timestamp_type: match canonical.timestamp_type {
                                TimestampType::CreateTime => 0,
                                TimestampType::LogAppendTime => 1,
                            },
                            key: entry.key.clone(),
                            value: entry.value.clone().unwrap_or_default(),
                        };
                        hot_records.push(hot_record);
                    }
                }
                WalRecord::V1 {
                    offset,
                    timestamp,
                    key,
                    value,
                    ..
                } => {
                    // V1 format (backward compatibility)
                    let hot_record = HotRecord {
                        topic: topic.to_string(),
                        partition,
                        offset: *offset,
                        timestamp_ms: *timestamp,
                        timestamp_type: 0, // V1 doesn't have timestamp type
                        key: key.clone(),
                        value: value.clone(),
                    };
                    hot_records.push(hot_record);
                }
            }
        }

        Ok(hot_records)
    }

    /// Clear the cache for a topic-partition
    pub fn clear_cache(&self, topic: &str, partition: i32) {
        let tp = TopicPartition::new(topic, partition);
        self.cache.remove(&tp);
    }

    /// Clear all caches
    pub fn clear_all_caches(&self) {
        self.cache.clear();
    }

    /// Get statistics about the hot buffer
    pub fn stats(&self) -> HotBufferStats {
        let mut total_records = 0;
        let mut total_partitions = 0;

        for entry in self.cache.iter() {
            total_partitions += 1;
            total_records += entry.value().record_count;
        }

        HotBufferStats {
            enabled: self.config.enabled,
            total_partitions,
            total_records,
            max_records_per_partition: self.config.max_records_per_partition,
            refresh_interval_ms: self.config.refresh_interval_ms,
        }
    }
}

/// Statistics about the hot buffer
#[derive(Debug, Clone)]
pub struct HotBufferStats {
    /// Whether hot buffer is enabled
    pub enabled: bool,
    /// Number of partitions with cached data
    pub total_partitions: usize,
    /// Total records across all partitions
    pub total_records: usize,
    /// Maximum records per partition configuration
    pub max_records_per_partition: usize,
    /// Refresh interval in milliseconds
    pub refresh_interval_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_partition_equality() {
        let tp1 = TopicPartition::new("test", 0);
        let tp2 = TopicPartition::new("test", 0);
        let tp3 = TopicPartition::new("test", 1);
        let tp4 = TopicPartition::new("other", 0);

        assert_eq!(tp1, tp2);
        assert_ne!(tp1, tp3);
        assert_ne!(tp1, tp4);
    }

    #[test]
    fn test_config_default() {
        let config = HotBufferConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_records_per_partition, 100_000);
        assert_eq!(config.refresh_interval_ms, 1000);
    }

    #[test]
    fn test_hot_buffer_schema() {
        let schema = HotDataBuffer::hot_buffer_schema();
        assert_eq!(schema.fields().len(), 7);
        assert!(schema.field_with_name("_topic").is_ok());
        assert!(schema.field_with_name("_partition").is_ok());
        assert!(schema.field_with_name("_offset").is_ok());
        assert!(schema.field_with_name("_timestamp").is_ok());
        assert!(schema.field_with_name("_key").is_ok());
        assert!(schema.field_with_name("_value").is_ok());
    }
}
