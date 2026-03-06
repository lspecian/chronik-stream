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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use tracing::{debug, trace};

// Use DataFusion's re-exported arrow types to avoid version conflicts
// DataFusion 44 uses arrow 53.x, while direct arrow deps are 54.x
use datafusion::arrow::array::{
    ArrayRef, BinaryBuilder, Int32Array, Int64Array, Int8Array, RecordBatch, StringBuilder,
    TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::logical_expr::{Operator, TableProviderFilterPushDown};
use datafusion::error::Result as DFResult;
use datafusion::physical_plan::memory::MemoryExec;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::*;
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
    /// Maximum individual Kafka messages to include in hot buffer per partition.
    /// Each WAL batch may contain 100-2000 messages. This limit applies to
    /// the total message count, not the number of WAL batches.
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

// ============================================================================
// PartitionedMemTable: custom TableProvider with _partition filter pushdown
// ============================================================================

/// A partition-aware in-memory table that supports `_partition` filter pushdown.
///
/// Unlike DataFusion's MemTable which stores all batches in a flat list,
/// this stores per-partition RecordBatches and only scans partitions that
/// match `WHERE _partition = N` or `WHERE _partition IN (...)` predicates.
///
/// At 43.9M records across 24 partitions (~1.8M per partition), this reduces
/// scan size by ~24x for single-partition queries.
#[derive(Debug)]
pub struct PartitionedMemTable {
    schema: SchemaRef,
    partition_batches: HashMap<i32, RecordBatch>,
}

impl PartitionedMemTable {
    /// Create a new partitioned mem table from per-partition batches.
    pub fn new(schema: SchemaRef, partition_batches: HashMap<i32, RecordBatch>) -> Self {
        Self { schema, partition_batches }
    }
}

#[async_trait::async_trait]
impl TableProvider for PartitionedMemTable {
    fn as_any(&self) -> &dyn std::any::Any { self }

    fn schema(&self) -> SchemaRef { self.schema.clone() }

    fn table_type(&self) -> TableType { TableType::Temporary }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        _limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        // Extract _partition = N from filters
        let target_partitions = extract_partition_filters(filters);

        let batches: Vec<RecordBatch> = if target_partitions.is_empty() {
            // No partition filter — scan all
            self.partition_batches.values().cloned().collect()
        } else {
            target_partitions.iter()
                .filter_map(|p| self.partition_batches.get(p))
                .cloned()
                .collect()
        };

        debug!(
            "PartitionedMemTable scan: {} target partitions, {} of {} batches selected",
            if target_partitions.is_empty() { "all".to_string() } else { format!("{:?}", target_partitions) },
            batches.len(),
            self.partition_batches.len()
        );

        MemoryExec::try_new(
            &[batches],
            self.schema.clone(),
            projection.cloned(),
        ).map(|e| Arc::new(e) as Arc<dyn ExecutionPlan>)
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DFResult<Vec<TableProviderFilterPushDown>> {
        // Return Inexact for all filters — DataFusion still applies them post-scan
        // but our scan() uses them for partition pruning (loading fewer batches).
        // Using Exact would skip post-scan filtering, which is risky if our
        // extract_partition_filters misses an edge case.
        Ok(filters.iter().map(|_| TableProviderFilterPushDown::Inexact).collect())
    }
}

/// Extract partition IDs from WHERE _partition = N or WHERE _partition IN (a, b, c).
fn extract_partition_filters(filters: &[Expr]) -> Vec<i32> {
    let mut partitions = Vec::new();
    for filter in filters {
        match filter {
            Expr::BinaryExpr(binary) => {
                if let (Expr::Column(col), Expr::Literal(lit)) = (binary.left.as_ref(), binary.right.as_ref()) {
                    if col.name == "_partition" && binary.op == Operator::Eq {
                        if let Some(val) = scalar_to_i32(lit) {
                            partitions.push(val);
                        }
                    }
                }
                // Also handle N = _partition (reversed)
                if let (Expr::Literal(lit), Expr::Column(col)) = (binary.left.as_ref(), binary.right.as_ref()) {
                    if col.name == "_partition" && binary.op == Operator::Eq {
                        if let Some(val) = scalar_to_i32(lit) {
                            partitions.push(val);
                        }
                    }
                }
            }
            Expr::InList(in_list) => {
                if let Expr::Column(col) = &*in_list.expr {
                    if col.name == "_partition" && !in_list.negated {
                        for val in &in_list.list {
                            if let Expr::Literal(lit) = val {
                                if let Some(v) = scalar_to_i32(lit) {
                                    partitions.push(v);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    partitions
}

/// Convert a DataFusion ScalarValue to i32 (handles Int32, Int64, UInt32 etc.)
fn scalar_to_i32(scalar: &datafusion::scalar::ScalarValue) -> Option<i32> {
    use datafusion::scalar::ScalarValue;
    match scalar {
        ScalarValue::Int32(Some(v)) => Some(*v),
        ScalarValue::Int64(Some(v)) => Some(*v as i32),
        ScalarValue::UInt32(Some(v)) => Some(*v as i32),
        ScalarValue::Int16(Some(v)) => Some(*v as i32),
        ScalarValue::Int8(Some(v)) => Some(*v as i32),
        _ => None,
    }
}

// ============================================================================
// HotDataBuffer
// ============================================================================

/// Hot Data Buffer for in-memory SQL queries
///
/// Reads recent records from WAL and converts them to Arrow format
/// for immediate SQL querying before Parquet files are created.
///
/// **JSON columns**: When the WalIndexer infers a JSON schema for a topic,
/// Parquet files include typed JSON columns (e.g., `name`, `age`).
/// The hot buffer provides base columns only (`_value` as binary).
/// Users can query `{topic}_cold` for typed columns, or use
/// `json_extract()` UDFs on `{topic}_hot` for the same data.
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
    /// Uses cached RecordBatch when available and fresh (within refresh_interval_ms).
    pub async fn get_mem_table(&self, topic: &str, partition: i32) -> Result<Option<MemTable>> {
        if !self.config.enabled {
            return Ok(None);
        }

        let tp = TopicPartition::new(topic, partition);
        let now_ms = Self::current_time_ms();

        // Check cache first — return cached batch if fresh
        if let Some(cached) = self.cache.get(&tp) {
            if now_ms.saturating_sub(cached.last_refresh_ms) < self.config.refresh_interval_ms {
                let schema = Arc::new(Self::hot_buffer_schema());
                let mem_table = MemTable::try_new(schema, vec![vec![cached.batch.clone()]])?;
                trace!(
                    "Cache hit for {}-{}: {} records, age={}ms",
                    topic, partition, cached.record_count,
                    now_ms.saturating_sub(cached.last_refresh_ms)
                );
                return Ok(Some(mem_table));
            }
        }

        // Cache miss or stale — read from WAL
        let flushed_offset = self.flushed_offsets.get(&tp).map(|v| *v).unwrap_or(0);

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
            // Cache empty result to avoid repeated WAL reads
            self.cache.remove(&tp);
            return Ok(None);
        }

        let hot_records = self.wal_records_to_hot_records(topic, partition, &wal_records)?;

        if hot_records.is_empty() {
            self.cache.remove(&tp);
            return Ok(None);
        }

        let batch = self.records_to_batch(&hot_records)?;

        // Store in cache
        let cached = CachedBatch {
            batch: batch.clone(),
            min_offset: hot_records.first().map(|r| r.offset).unwrap_or(0),
            max_offset: hot_records.last().map(|r| r.offset).unwrap_or(0),
            record_count: hot_records.len(),
            last_refresh_ms: now_ms,
        };
        debug!(
            "Cache refresh for {}-{}: {} records, offsets {}..{}",
            topic, partition, cached.record_count, cached.min_offset, cached.max_offset,
        );
        self.cache.insert(tp, cached);

        let schema = Arc::new(Self::hot_buffer_schema());
        let mem_table = MemTable::try_new(schema, vec![vec![batch]])?;

        Ok(Some(mem_table))
    }

    /// Get hot data for all partitions of a topic
    ///
    /// Returns a combined MemTable containing data from all partitions.
    /// Uses cached RecordBatch per partition when available and fresh.
    pub async fn get_topic_mem_table(&self, topic: &str) -> Result<Option<PartitionedMemTable>> {
        if !self.config.enabled {
            return Ok(None);
        }

        let all_partitions = self.wal_manager.get_partitions();
        let partitions: Vec<i32> = all_partitions
            .into_iter()
            .filter(|tp| tp.topic == topic)
            .map(|tp| tp.partition)
            .collect();

        if partitions.is_empty() {
            return Ok(None);
        }

        let now_ms = Self::current_time_ms();
        let mut partition_batches: HashMap<i32, RecordBatch> = HashMap::new();
        let mut cache_hits = 0u32;
        let mut cache_misses = 0u32;

        for partition in partitions {
            let tp = TopicPartition::new(topic, partition);

            // Check cache first
            if let Some(cached) = self.cache.get(&tp) {
                if now_ms.saturating_sub(cached.last_refresh_ms) < self.config.refresh_interval_ms {
                    partition_batches.insert(partition, cached.batch.clone());
                    cache_hits += 1;
                    continue;
                }
            }

            // Cache miss or stale — read from WAL
            cache_misses += 1;
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

            // Update cache
            let cached = CachedBatch {
                batch: batch.clone(),
                min_offset: hot_records.first().map(|r| r.offset).unwrap_or(0),
                max_offset: hot_records.last().map(|r| r.offset).unwrap_or(0),
                record_count: hot_records.len(),
                last_refresh_ms: now_ms,
            };
            self.cache.insert(tp, cached);

            partition_batches.insert(partition, batch);
        }

        if partition_batches.is_empty() {
            return Ok(None);
        }

        debug!(
            "Topic {} hot buffer: {} cache hits, {} misses, {} total partitions",
            topic, cache_hits, cache_misses, partition_batches.len()
        );

        let schema = Arc::new(Self::hot_buffer_schema());
        let table = PartitionedMemTable::new(schema, partition_batches);

        Ok(Some(table))
    }

    /// Set the flushed offset for a topic-partition
    ///
    /// Called by WalIndexer when Parquet files are created.
    /// Records up to this offset are in Parquet and don't need to be in hot buffer.
    /// Invalidates the cached RecordBatch so the next query re-reads with the new offset range.
    pub fn set_flushed_offset(&self, topic: &str, partition: i32, offset: i64) {
        let tp = TopicPartition::new(topic, partition);
        self.flushed_offsets.insert(tp.clone(), offset);
        // Invalidate cache — flushed offset changed, cached batch covers wrong range
        self.cache.remove(&tp);
        debug!("Set flushed offset for {}-{}: {} (cache invalidated)", topic, partition, offset);
    }

    /// Get the flushed offset for a topic-partition
    pub fn get_flushed_offset(&self, topic: &str, partition: i32) -> i64 {
        let tp = TopicPartition::new(topic, partition);
        self.flushed_offsets.get(&tp).map(|v| *v).unwrap_or(0)
    }

    /// Get current time in milliseconds since Unix epoch
    fn current_time_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
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

    /// Helper: create a RecordBatch with N rows for a given partition
    fn make_batch(schema: &SchemaRef, partition: i32, num_rows: usize) -> RecordBatch {
        use datafusion::arrow::array::{StringBuilder, BinaryBuilder, Int32Array, Int64Array, Int8Array, TimestampMillisecondBuilder};
        let mut topic_b = StringBuilder::new();
        let mut part_b = Int32Array::builder(num_rows);
        let mut off_b = Int64Array::builder(num_rows);
        let mut ts_b = TimestampMillisecondBuilder::new().with_timezone("UTC");
        let mut tstype_b = Int8Array::builder(num_rows);
        let mut key_b = BinaryBuilder::new();
        let mut val_b = BinaryBuilder::new();
        for i in 0..num_rows {
            topic_b.append_value("test");
            part_b.append_value(partition);
            off_b.append_value(i as i64);
            ts_b.append_value(1000 + i as i64);
            tstype_b.append_value(0);
            key_b.append_value(format!("k{}", i).as_bytes());
            val_b.append_value(format!("v{}", i).as_bytes());
        }
        RecordBatch::try_new(schema.clone(), vec![
            Arc::new(topic_b.finish()),
            Arc::new(part_b.finish()),
            Arc::new(off_b.finish()),
            Arc::new(ts_b.finish()),
            Arc::new(tstype_b.finish()),
            Arc::new(key_b.finish()),
            Arc::new(val_b.finish()),
        ]).unwrap()
    }

    #[test]
    fn test_extract_partition_filters_eq() {
        use datafusion::prelude::col;
        use datafusion::prelude::lit;
        let filters = vec![col("_partition").eq(lit(2i32))];
        let result = extract_partition_filters(&filters);
        assert_eq!(result, vec![2]);
    }

    #[test]
    fn test_extract_partition_filters_in_list() {
        use datafusion::logical_expr::Expr;
        use datafusion::prelude::{col, lit};
        let in_list = Expr::InList(datafusion::logical_expr::expr::InList::new(
            Box::new(col("_partition")),
            vec![lit(0i32), lit(3i32), lit(7i32)],
            false,
        ));
        let result = extract_partition_filters(&[in_list]);
        assert_eq!(result, vec![0, 3, 7]);
    }

    #[test]
    fn test_extract_partition_filters_no_match() {
        use datafusion::prelude::{col, lit};
        // Filter on a non-partition column
        let filters = vec![col("_offset").eq(lit(42i64))];
        let result = extract_partition_filters(&filters);
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_partitioned_mem_table_scan_all() {
        let schema = Arc::new(HotDataBuffer::hot_buffer_schema());
        let mut batches = HashMap::new();
        batches.insert(0, make_batch(&schema, 0, 100));
        batches.insert(1, make_batch(&schema, 1, 200));
        batches.insert(2, make_batch(&schema, 2, 300));
        let table = PartitionedMemTable::new(schema, batches);

        let ctx = datafusion::prelude::SessionContext::new();
        let plan = table.scan(&ctx.state(), None, &[], None).await.unwrap();
        let results = datafusion::physical_plan::collect(plan, ctx.task_ctx()).await.unwrap();
        let total_rows: usize = results.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 600); // 100 + 200 + 300
    }

    #[tokio::test]
    async fn test_partitioned_mem_table_scan_single_partition() {
        use datafusion::prelude::{col, lit};
        let schema = Arc::new(HotDataBuffer::hot_buffer_schema());
        let mut batches = HashMap::new();
        batches.insert(0, make_batch(&schema, 0, 100));
        batches.insert(1, make_batch(&schema, 1, 200));
        batches.insert(2, make_batch(&schema, 2, 300));
        let table = PartitionedMemTable::new(schema, batches);

        let filters = vec![col("_partition").eq(lit(1i32))];
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = table.scan(&ctx.state(), None, &filters, None).await.unwrap();
        let results = datafusion::physical_plan::collect(plan, ctx.task_ctx()).await.unwrap();
        let total_rows: usize = results.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 200); // Only partition 1
    }

    #[tokio::test]
    async fn test_partitioned_mem_table_scan_missing_partition() {
        use datafusion::prelude::{col, lit};
        let schema = Arc::new(HotDataBuffer::hot_buffer_schema());
        let mut batches = HashMap::new();
        batches.insert(0, make_batch(&schema, 0, 100));
        let table = PartitionedMemTable::new(schema, batches);

        let filters = vec![col("_partition").eq(lit(99i32))];
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = table.scan(&ctx.state(), None, &filters, None).await.unwrap();
        let results = datafusion::physical_plan::collect(plan, ctx.task_ctx()).await.unwrap();
        let total_rows: usize = results.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 0); // Non-existent partition
    }

    #[test]
    fn test_supports_filters_pushdown() {
        use datafusion::prelude::{col, lit};
        let schema = Arc::new(HotDataBuffer::hot_buffer_schema());
        let table = PartitionedMemTable::new(schema, HashMap::new());

        let partition_filter = col("_partition").eq(lit(1i32));
        let other_filter = col("_offset").eq(lit(42i64));
        let filters: Vec<&Expr> = vec![&partition_filter, &other_filter];

        let result = table.supports_filters_pushdown(&filters).unwrap();
        // All filters return Inexact — scan() uses them for pruning but
        // DataFusion still applies them post-scan for correctness
        assert_eq!(result[0], TableProviderFilterPushDown::Inexact);
        assert_eq!(result[1], TableProviderFilterPushDown::Inexact);
    }
}
