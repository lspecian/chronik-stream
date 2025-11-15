//! Fetch request handler for serving data to Kafka consumers.

use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition};
use chronik_storage::{SegmentReader, RecordBatch, Record, Segment, ObjectStoreTrait, SegmentIndex};
use chronik_storage::kafka_records::{KafkaRecordBatch, KafkaRecord, RecordHeader as KafkaRecordHeader, CompressionType};
use chronik_storage::tantivy_segment::TantivySegmentReader;
use chronik_storage::canonical_record::CanonicalRecord;
use chronik_wal::{WalManager, WalRecord};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// In-memory buffer for recent records
/// CRITICAL v1.3.32: Store RAW Kafka batch bytes to preserve CRC
#[derive(Debug)]
struct PartitionBuffer {
    /// Raw Kafka RecordBatch bytes in wire format (preserves original CRC)
    raw_batches: Vec<bytes::Bytes>,
    /// Metadata about batches for offset tracking
    batch_metadata: Vec<BatchMetadata>,
    base_offset: i64,
    high_watermark: i64,
    /// Highest offset that has been flushed to segments
    flushed_offset: i64,
    /// Minimum offset actually present in buffer after trimming (v1.3.48)
    /// Used to detect gaps and trigger WAL fallback when old batches are trimmed
    min_offset_in_buffer: i64,
}

/// Metadata about a batch in the buffer
#[derive(Debug, Clone)]
struct BatchMetadata {
    base_offset: i64,
    last_offset: i64,
    record_count: i32,
    size_bytes: usize,
}

/// Fetch handler state
struct FetchState {
    /// In-memory buffers for recent data
    buffers: HashMap<(String, i32), PartitionBuffer>,
    /// Cached segment metadata
    segment_cache: HashMap<(String, i32), Vec<SegmentInfo>>,
}

/// Segment metadata for fetch operations
#[derive(Clone, Debug)]
struct SegmentInfo {
    segment_id: String,
    base_offset: i64,
    last_offset: i64,
    object_key: String,
}

/// Configuration for FetchHandler behavior
#[derive(Debug, Clone)]
pub struct FetchHandlerConfig {
    /// Enable follower reads (default: true)
    /// If false, only leaders can serve fetches
    pub allow_follower_reads: bool,

    /// Maximum wait time for commit in follower reads (ms)
    pub follower_read_max_wait_ms: u64,

    /// Node ID (for preferred_read_replica)
    pub node_id: i32,
}

impl Default for FetchHandlerConfig {
    fn default() -> Self {
        let allow_follower_reads = std::env::var("CHRONIK_FETCH_FROM_FOLLOWERS")
            .unwrap_or_else(|_| "true".to_string())
            .parse()
            .unwrap_or(true);

        let follower_read_max_wait_ms = std::env::var("CHRONIK_FETCH_FOLLOWER_MAX_WAIT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000); // 1 second default

        Self {
            allow_follower_reads,
            follower_read_max_wait_ms,
            node_id: 0,
        }
    }
}

/// Fetch request handler
/// v1.3.47+: Uses Arc<WalManager> directly (no RwLock - WalManager uses DashMap internally)
/// v1.3.66+: Raft-aware with follower read support using RaftReplicaManager
/// v2.0.0+: Read-your-writes consistency with ReadIndex protocol
pub struct FetchHandler {
    segment_reader: Arc<SegmentReader>,
    metadata_store: Arc<dyn MetadataStore>,
    object_store: Arc<dyn ObjectStoreTrait>,
    wal_manager: Option<Arc<WalManager>>,
    segment_index: Option<Arc<SegmentIndex>>,
    produce_handler: Option<Arc<crate::produce_handler::ProduceHandler>>,
    state: Arc<RwLock<FetchState>>,
    /// Configuration for fetch behavior
    config: FetchHandlerConfig,
}

impl FetchHandler {
    /// Create a new fetch handler
    pub fn new(
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
    ) -> Self {
        Self {
            segment_reader,
            metadata_store,
            object_store,
            wal_manager: None,
            segment_index: None,
            produce_handler: None,
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
                segment_cache: HashMap::new(),
            })),
            config: FetchHandlerConfig::default(),
        }
    }

    /// Create a new fetch handler with WAL and ProduceHandler integration (v1.3.39+)
    /// v1.3.47+: Accepts Arc<WalManager> directly (no RwLock - WalManager uses DashMap internally)
    pub fn new_with_wal(
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        wal_manager: Arc<WalManager>,
        produce_handler: Arc<crate::produce_handler::ProduceHandler>,
    ) -> Self {
        Self {
            segment_reader,
            metadata_store,
            object_store,
            wal_manager: Some(wal_manager),
            segment_index: None,
            produce_handler: Some(produce_handler),
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
                segment_cache: HashMap::new(),
            })),
            config: FetchHandlerConfig::default(),
        }
    }

    /// Create a new fetch handler with WAL and segment index
    /// v1.3.47+: Accepts Arc<WalManager> directly (no RwLock - WalManager uses DashMap internally)
    pub fn new_with_wal_and_index(
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        wal_manager: Arc<WalManager>,
        segment_index: Arc<SegmentIndex>,
    ) -> Self {
        Self {
            segment_reader,
            metadata_store,
            object_store,
            wal_manager: Some(wal_manager),
            segment_index: Some(segment_index),
            produce_handler: None, // Not provided in this constructor
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
                segment_cache: HashMap::new(),
            })),
            config: FetchHandlerConfig::default(),
        }
    }

    /// Create a new fetch handler with WAL and ProduceHandler (v2.2.7)
    pub fn new_with_wal_and_produce(
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        wal_manager: Arc<WalManager>,
        produce_handler: Arc<crate::produce_handler::ProduceHandler>,
        config: FetchHandlerConfig,
    ) -> Self {
        info!(
            "FetchHandler initialized with Raft: allow_follower_reads={}, follower_read_max_wait_ms={}, node_id={}",
            config.allow_follower_reads, config.follower_read_max_wait_ms, config.node_id
        );

        Self {
            segment_reader,
            metadata_store,
            object_store,
            wal_manager: Some(wal_manager),
            segment_index: None,
            produce_handler: Some(produce_handler),
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
                segment_cache: HashMap::new(),
            })),
            config,
        }
    }
    /// Handle a fetch request
    pub async fn handle_fetch(
        &self,
        request: FetchRequest,
        correlation_id: i32,
    ) -> Result<FetchResponse> {
        let mut response_topics = Vec::new();
        
        for topic_request in request.topics {
            let mut response_partitions = Vec::new();
            
            for partition_request in topic_request.partitions {
                let partition_response = self.fetch_partition(
                    &topic_request.name,
                    partition_request.partition,
                    partition_request.fetch_offset,
                    partition_request.partition_max_bytes,
                    request.max_wait_ms,
                    request.min_bytes,
                ).await?;
                
                response_partitions.push(partition_response);
            }
            
            response_topics.push(FetchResponseTopic {
                name: topic_request.name,
                partitions: response_partitions,
            });
        }
        
        Ok(FetchResponse {
            header: chronik_protocol::parser::ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: 0,
            session_id: 0,
            topics: response_topics,
        })
    }
    
    /// Fetch data from a specific partition
    async fn fetch_partition(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
        max_wait_ms: i32,
        min_bytes: i32,
    ) -> Result<FetchResponsePartition> {
        // v2.2.7.2: Enhanced tracing to debug large batch consumption stalls
        let fetch_start = Instant::now();
        info!(
            "🔍 FETCH START: topic={}, partition={}, offset={}, max_bytes={}, max_wait_ms={}, min_bytes={}",
            topic, partition, fetch_offset, max_bytes, max_wait_ms, min_bytes
        );


        // First check if topic exists
        let topic_metadata = match self.metadata_store.get_topic(topic).await? {
            Some(meta) => meta,
            None => {
                return Ok(FetchResponsePartition {
                    partition,
                    error_code: 3, // UNKNOWN_TOPIC_OR_PARTITION
                    high_watermark: -1,
                    last_stable_offset: -1,
                    log_start_offset: -1,
                    aborted: None,
                    preferred_read_replica: -1,
                    records: vec![],
                });
            }
        };
        
        // Check partition exists
        if partition < 0 || partition >= topic_metadata.config.partition_count as i32 {
            return Ok(FetchResponsePartition {
                partition,
                error_code: 3, // UNKNOWN_TOPIC_OR_PARTITION
                high_watermark: -1,
                last_stable_offset: -1,
                log_start_offset: -1,
                aborted: None,
                preferred_read_replica: -1,
                records: vec![],
            });
        }
        
        // NEW ARCHITECTURE (v1.3.39+): Get high watermark from ProduceHandler (source of truth)
        // This is the next offset that will be assigned = current high watermark
        let high_watermark = if let Some(ref produce_handler) = self.produce_handler {
            produce_handler.get_high_watermark(topic, partition).await
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to get high watermark from ProduceHandler for {}-{}: {}, defaulting to 0", topic, partition, e);
                    0
                })
        } else {
            // Fallback: try segments (OLD architecture path)
            let segments = self.metadata_store.list_segments(topic, Some(partition as u32)).await?;
            tracing::info!("Found {} segments for {}-{}", segments.len(), topic, partition);
            segments.iter()
                .map(|s| s.end_offset + 1)
                .max()
                .unwrap_or(0)
        };
        
        // v2.2.7.2: Log watermark details for debugging
        info!(
            "📊 WATERMARK: topic={}, partition={}, high_watermark={}, fetch_offset={}, gap={} (from ProduceHandler)",
            topic, partition, high_watermark, fetch_offset, high_watermark - fetch_offset
        );

        // Log start offset is always 0 in NEW architecture (WAL-based)
        let log_start_offset = 0;

        // Check if offset is out of range
        if fetch_offset < log_start_offset {
            return Ok(FetchResponsePartition {
                partition,
                error_code: 1, // OFFSET_OUT_OF_RANGE
                high_watermark,
                last_stable_offset: high_watermark,
                log_start_offset,
                aborted: None,
                preferred_read_replica: -1,
                records: vec![],
            });
        }
        
        // If we have data available (fetch_offset < high_watermark), fetch it
        if fetch_offset < high_watermark {
            // v2.2.7.2: Log data available path
            info!(
                "✅ DATA AVAILABLE: topic={}, partition={}, fetch_offset={}, high_watermark={}, available={}",
                topic, partition, fetch_offset, high_watermark, high_watermark - fetch_offset
            );

            // Data is available, fetch it
            let fetch_timeout = if max_wait_ms > 0 {
                Duration::from_millis(max_wait_ms as u64)
            } else {
                Duration::from_secs(30) // Default timeout
            };

            // CRITICAL CRC FIX v1.3.32: Try to fetch raw Kafka bytes first to preserve CRC
            info!("📦 FETCH RAW: Trying raw bytes (CRC-preserving) for {}-{}", topic, partition);
            let raw_bytes_result = timeout(fetch_timeout, async {
                self.fetch_raw_bytes(
                    topic,
                    partition,
                    fetch_offset,
                    high_watermark,
                    max_bytes,
                ).await
            }).await;

            let records_bytes = match raw_bytes_result {
                Ok(Ok(Some(raw_bytes))) => {
                    tracing::info!("✓ CRC-PRESERVED: Fetched {} bytes of raw Kafka data for {}-{}",
                        raw_bytes.len(), topic, partition);

                    // HEX DUMP: Outgoing raw bytes to consumer
                    if raw_bytes.len() >= 61 {
                        let hex_first_64: String = raw_bytes.iter().take(64).map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                        tracing::debug!("FETCH OUTGOING (first 64 bytes): {}", hex_first_64);
                    }

                    raw_bytes
                }
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
                    // Fall back to parsed records (will recompute CRC)
                    tracing::warn!("⚠ CRC-RECOMPUTED: No raw bytes available, falling back to parsed records for {}-{}",
                        topic, partition);

                    let fetch_result = timeout(fetch_timeout, async {
                        self.fetch_records(
                            topic,
                            partition,
                            fetch_offset,
                            high_watermark,
                            max_bytes,
                        ).await
                    }).await;

                    let records = match fetch_result {
                        Ok(Ok(recs)) => {
                            tracing::info!("Fetched {} records from {}-{}", recs.len(), topic, partition);
                            recs
                        },
                        Ok(Err(e)) => {
                            tracing::warn!("Error fetching records from {}-{}: {:?}", topic, partition, e);
                            vec![]
                        }
                        Err(_) => {
                            tracing::warn!("Fetch timeout after {}ms for {}-{}", max_wait_ms, topic, partition);
                            vec![]
                        }
                    };

                    // Encode the records - will recompute CRC
                    self.encode_kafka_records(&records, 0)?
                }
            };

            // v2.2.7.2: Log successful fetch completion
            let fetch_elapsed = fetch_start.elapsed();
            info!(
                "✅ FETCH SUCCESS: topic={}, partition={}, offset={}, bytes={}, elapsed={:?}",
                topic, partition, fetch_offset, records_bytes.len(), fetch_elapsed
            );

            return Ok(FetchResponsePartition {
                partition,
                error_code: 0,
                high_watermark,
                last_stable_offset: high_watermark,
                log_start_offset,
                aborted: None,
                preferred_read_replica: -1,
                records: records_bytes,
            });
        }

        // No data available yet (fetch_offset >= high_watermark)
        if fetch_offset >= high_watermark {
            // v2.2.7.2: Log no data available case
            info!(
                "⏸️  NO DATA: topic={}, partition={}, fetch_offset={}, high_watermark={} (waiting...)",
                topic, partition, fetch_offset, high_watermark
            );
            // No data available yet - implement wait logic
            if max_wait_ms > 0 && min_bytes > 0 {
                // Wait for new data or timeout
                let start_time = Instant::now();
                let wait_duration = Duration::from_millis(max_wait_ms as u64);
                
                // Try to wait for data with timeout
                let result = timeout(wait_duration, async {
                    // Poll for new data periodically
                    let poll_interval = Duration::from_millis(10);
                    let mut accumulated_bytes = 0;
                    
                    while start_time.elapsed() < wait_duration {
                        // Check if new data is available
                        if let Ok(new_segments) = self.metadata_store.list_segments(topic, Some(partition as u32)).await {
                            let new_high_watermark = new_segments.iter()
                                .map(|s| s.end_offset + 1)
                                .max()
                                .unwrap_or(high_watermark);
                            
                            if new_high_watermark > fetch_offset {
                                // New data available, fetch it
                                if let Ok(records) = self.fetch_records(
                                    topic,
                                    partition,
                                    fetch_offset,
                                    new_high_watermark,
                                    max_bytes,
                                ).await {
                                    // Calculate approximate bytes
                                    accumulated_bytes = records.iter()
                                        .map(|r| r.value.len() + r.key.as_ref().map(|k| k.len()).unwrap_or(0))
                                        .sum();
                                    
                                    if accumulated_bytes >= min_bytes as usize {
                                        return Ok((records, new_high_watermark));
                                    }
                                }
                            }
                        }
                        
                        tokio::time::sleep(poll_interval).await;
                    }
                    
                    Err::<(Vec<chronik_storage::Record>, i64), Error>(
                        Error::Internal("Timeout waiting for min_bytes".into())
                    )
                }).await;
                
                match result {
                    Ok(Ok((records, new_hw))) => {
                        // Got enough data within timeout
                        let records_bytes = self.encode_kafka_records(&records, 0)?;
                        
                        return Ok(FetchResponsePartition {
                            partition,
                            error_code: 0,
                            high_watermark: new_hw,
                            last_stable_offset: new_hw,
                            log_start_offset,
                            aborted: None,
                            preferred_read_replica: -1,
                            records: records_bytes,
                        });
                    }
                    _ => {
                        // Timeout or error - return empty response with proper empty batch
                        let empty_records = self.encode_kafka_records(&[], 0)?;
                        
                        return Ok(FetchResponsePartition {
                            partition,
                            error_code: 0,
                            high_watermark,
                            last_stable_offset: high_watermark,
                            log_start_offset,
                            aborted: None,
                            preferred_read_replica: -1,
                            records: empty_records,
                        });
                    }
                }
            } else {
                // No wait requested, return empty immediately with proper empty batch
                let empty_records = self.encode_kafka_records(&[], 0)?;
                
                return Ok(FetchResponsePartition {
                    partition,
                    error_code: 0,
                    high_watermark,
                    last_stable_offset: high_watermark,
                    log_start_offset,
                    aborted: None,
                    preferred_read_replica: -1,
                    records: empty_records,
                });
            }
        }
        
        // Should not reach here - all cases should have returned above
        unreachable!("Fetch handler logic error")
    }
    
    /// Fetch records with proper priority: Buffer → WAL → Segments
    async fn fetch_records(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        info!(
            "fetch_records called - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
            topic, partition, fetch_offset, high_watermark
        );
        
        let mut records = Vec::new();
        let mut bytes_fetched = 0usize;
        
        // PHASE 1: Try in-memory buffer first (fastest path)
        // CRITICAL v1.3.32: Decode records from raw batches to preserve CRC
        // v1.3.48: Also track buffer min_offset to detect gaps from trimming
        let (buffer_highest_offset, buffer_min_offset) = {
            let state = self.state.read().await;
            if let Some(buffer) = state.buffers.get(&(topic.to_string(), partition)) {
                info!(
                    "FETCH→BUFFER: Checking buffer for {}-{}, buffer has {} batches, min_offset={}",
                    topic, partition, buffer.batch_metadata.len(), buffer.min_offset_in_buffer
                );

                let mut buffer_max_offset = -1i64;
                let buffer_min = buffer.min_offset_in_buffer;

                for (batch_idx, metadata) in buffer.batch_metadata.iter().enumerate() {
                    if metadata.last_offset >= fetch_offset && metadata.base_offset < high_watermark {
                        // Decode records from raw batch
                        let raw_batch = &buffer.raw_batches[batch_idx];
                        match self.decode_records_from_raw_batch(raw_batch, fetch_offset, high_watermark) {
                            Ok(batch_records) => {
                                for record in batch_records {
                                    let record_size = record.value.len() +
                                        record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;

                                    if bytes_fetched + record_size > max_bytes as usize && !records.is_empty() {
                                        break;
                                    }

                                    debug!(
                                        "FETCH→BUFFER: Found record at offset {} in buffer",
                                        record.offset
                                    );

                                    records.push(record.clone());
                                    bytes_fetched += record_size;
                                    buffer_max_offset = buffer_max_offset.max(record.offset);
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to decode batch from buffer: {}", e);
                                continue;
                            }
                        }
                    }
                }

                if !records.is_empty() {
                    info!(
                        "FETCH→BUFFER: Fetched {} records from buffer for {}-{}, highest offset: {}",
                        records.len(), topic, partition, buffer_max_offset
                    );
                }
                (buffer_max_offset, buffer_min)
            } else {
                (-1i64, -1i64)
            }
        };

        // If we got records from buffer, check if we need more from WAL/segments
        if !records.is_empty() && bytes_fetched >= max_bytes as usize {
            // We have enough data from buffer alone
            return Ok(records);
        }

        // v1.3.48: Improved WAL fallback logic to detect buffer gaps from trimming
        // Check if we need to read from WAL for missing data
        let need_earlier_records = records.is_empty() ||
            (buffer_highest_offset >= 0 && fetch_offset < buffer_highest_offset);

        // NEW (v1.3.48): Detect if fetch_offset is before buffer's min_offset (trimmed region)
        let fetch_in_trimmed_region = buffer_min_offset >= 0 && fetch_offset < buffer_min_offset;

        // PHASE 2: Try WAL for any missing records or to continue the fetch
        // WAL should have records that were trimmed from buffer
        if let Some(wal_manager) = &self.wal_manager {
            // Try WAL if:
            // 1. Buffer is empty, OR
            // 2. Need earlier records than buffer has, OR
            // 3. Fetch offset is in trimmed region (gap in buffer)
            let should_try_wal = records.is_empty() || need_earlier_records || fetch_in_trimmed_region;

            if should_try_wal {
                if fetch_in_trimmed_region {
                    info!(
                        "FETCH→WAL_FALLBACK: fetch_offset={} is before buffer min_offset={} (trimmed), reading from WAL",
                        fetch_offset, buffer_min_offset
                    );
                }
                match self.fetch_from_wal(wal_manager, topic, partition, fetch_offset, max_bytes).await {
                    Ok(wal_records) => {
                        if !wal_records.is_empty() {
                            info!(
                                "FETCH→WAL: Successfully fetched {} records from WAL for {}-{}",
                                wal_records.len(), topic, partition
                            );
                            // Merge WAL records with buffer records
                            for wal_rec in wal_records {
                                if !records.iter().any(|r| r.offset == wal_rec.offset) {
                                    records.push(wal_rec);
                                }
                            }
                            // Sort by offset to maintain order
                            records.sort_by_key(|r| r.offset);

                            // If we now have enough data, return
                            if records.len() > 0 && bytes_fetched >= max_bytes as usize {
                                return Ok(records);
                            }
                        } else {
                            debug!(
                                "FETCH→WAL: No records found in WAL for {}-{} at offset {}",
                                topic, partition, fetch_offset
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            "FETCH→WAL: Failed to fetch from WAL for {}-{}: {} - will try segments",
                            topic, partition, e
                        );
                    }
                }
            }
        }
        
        // PHASE 3: Try downloading raw segments from S3 (Tier 2: warm storage)
        // This is the NEW v1.3.64 flow where sealed WAL segments are uploaded to S3
        if records.is_empty() || need_earlier_records {
            info!(
                "FETCH→S3_RAW_SEGMENTS: Trying to download raw segment from S3 for {}-{} (have {} records so far)",
                topic, partition, records.len()
            );

            match self.fetch_from_s3_raw_segments(
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes
            ).await {
                Ok(s3_records) if !s3_records.is_empty() => {
                    info!(
                        "FETCH→S3_RAW_SEGMENTS: Successfully fetched {} records from S3 for {}-{}",
                        s3_records.len(), topic, partition
                    );

                    // Merge S3 records with existing records
                    for s3_rec in s3_records {
                        if !records.iter().any(|r| r.offset == s3_rec.offset) {
                            records.push(s3_rec);
                        }
                    }

                    // Sort by offset to maintain order
                    records.sort_by_key(|r| r.offset);

                    // If we now have enough data, return
                    if !records.is_empty() {
                        return Ok(records);
                    }
                }
                Ok(_) => {
                    debug!(
                        "FETCH→S3_RAW_SEGMENTS: No records found in S3 raw segments for {}-{} at offset {}",
                        topic, partition, fetch_offset
                    );
                }
                Err(e) => {
                    warn!(
                        "FETCH→S3_RAW_SEGMENTS: Failed to fetch from S3 for {}-{}: {} - will try legacy segments",
                        topic, partition, e
                    );
                }
            }
        }

        // PHASE 4: Try Tantivy archives (cold storage) as final fallback
        // This provides searchable indexed archives for very old data
        if (records.is_empty() || need_earlier_records) && self.segment_index.is_some() {
            info!(
                "FETCH→TANTIVY: Trying Tantivy archives for {}-{} (have {} records so far)",
                topic, partition, records.len()
            );

            // Try to get records via Tantivy fetch (will download tar.gz, search index, return results)
            // This is a fallback for very old archived data
            match self.fetch_from_tantivy_for_records(
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes
            ).await {
                Ok(tantivy_records) if !tantivy_records.is_empty() => {
                    info!(
                        "FETCH→TANTIVY: Successfully fetched {} records from Tantivy archives for {}-{}",
                        tantivy_records.len(), topic, partition
                    );

                    // Merge Tantivy records with existing records
                    for t_rec in tantivy_records {
                        if !records.iter().any(|r| r.offset == t_rec.offset) {
                            records.push(t_rec);
                        }
                    }

                    // Sort by offset to maintain order
                    records.sort_by_key(|r| r.offset);
                }
                Ok(_) => {
                    debug!(
                        "FETCH→TANTIVY: No records found in Tantivy archives for {}-{} at offset {}",
                        topic, partition, fetch_offset
                    );
                }
                Err(e) => {
                    warn!(
                        "FETCH→TANTIVY: Failed to fetch from Tantivy archives for {}-{}: {}",
                        topic, partition, e
                    );
                }
            }
        }

        Ok(records)
    }
    
    /// Fetch records from WAL manager
    ///
    /// NEW (v1.3.36): Handle WAL V2 CanonicalRecord format
    /// v1.3.47+: Direct call to WalManager (no RwLock - uses DashMap internally)
    async fn fetch_from_wal(
        &self,
        wal_manager: &Arc<WalManager>,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        use chronik_storage::canonical_record::CanonicalRecord;

        // CRITICAL FIX (v1.3.55): Match fetch_raw_bytes_from_wal limit increase
        // Old limit (max_bytes / 100) caused consumer timeouts at ~75K messages
        let max_records = std::cmp::max(10000, max_bytes as usize / 10);

        let wal_records = wal_manager.read_from(topic, partition, fetch_offset, max_records).await
            .map_err(|e| Error::Internal(format!("WAL read failed: {}", e)))?;

        // Convert WalRecord to chronik_storage::Record
        let mut records = Vec::new();
        for wal_record in wal_records {
            // Process WAL V2 records (CanonicalRecord batches)
            if let chronik_wal::record::WalRecord::V2 { canonical_data, .. } = wal_record {
                // Deserialize CanonicalRecord from WAL
                match bincode::deserialize::<CanonicalRecord>(&canonical_data) {
                    Ok(canonical_record) => {
                        // Extract individual records from the batch
                        for entry in &canonical_record.records {
                            // Filter by offset range
                            if entry.offset >= fetch_offset {
                                let storage_record = chronik_storage::Record {
                                    offset: entry.offset,
                                    timestamp: entry.timestamp,
                                    key: entry.key.clone(),
                                    value: entry.value.clone().unwrap_or_default(),
                                    headers: entry.headers.iter()
                                        .filter_map(|h| h.value.as_ref().map(|v| (h.key.clone(), v.clone())))
                                        .collect(),
                                };
                                records.push(storage_record);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to deserialize CanonicalRecord from WAL V2: {}", e);
                    }
                }
            }
            // V1 records are skipped (legacy format from pre-v1.3.36)
        }

        info!("WAL returned {} records starting from offset {} for {}-{}",
            records.len(), fetch_offset, topic, partition);

        Ok(records)
    }

    /// Fetch records from S3 raw segments (Tier 2: warm storage)
    ///
    /// NEW (v1.3.64): Download and deserialize bincode Vec<CanonicalRecord> from S3
    /// Path: segments/{topic}/{partition}/{min_offset}-{max_offset}.segment
    async fn fetch_from_s3_raw_segments(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        use chronik_storage::canonical_record::CanonicalRecord;

        // Use metadata store to find segments instead of parsing filenames
        info!(
            "METADATA→LIST: Looking for segments for {}-{}",
            topic, partition
        );

        // List segments from metadata store
        let all_segments = match self.metadata_store.list_segments(topic, Some(partition as u32)).await {
            Ok(segs) => segs,
            Err(e) => {
                warn!("METADATA→LIST: Failed to list segments for {}-{}: {}", topic, partition, e);
                return Ok(vec![]);
            }
        };

        if all_segments.is_empty() {
            info!("METADATA→LIST: No segments found for {}-{}", topic, partition);
            return Ok(vec![]);
        }

        info!(
            "METADATA→LIST: Found {} segment(s) for {}-{}",
            all_segments.len(), topic, partition
        );

        // Find segments that overlap with [fetch_offset, high_watermark)
        let mut matching_segments = Vec::new();
        for seg_meta in all_segments {
            let min_offset = seg_meta.start_offset;
            let max_offset = seg_meta.end_offset;

            // Check if this segment overlaps with [fetch_offset, high_watermark)
            if max_offset >= fetch_offset && min_offset < high_watermark {
                info!(
                    "METADATA→MATCH: Segment {} covers offsets {}-{}, overlaps with fetch range {}-{}",
                    seg_meta.path, min_offset, max_offset, fetch_offset, high_watermark
                );
                matching_segments.push((seg_meta.path.clone(), min_offset, max_offset));
            }
        }

        if matching_segments.is_empty() {
            info!(
                "S3→NO_MATCH: No segments overlap with fetch range {}-{} for {}-{}",
                fetch_offset, high_watermark, topic, partition
            );
            return Ok(vec![]);
        }

        // Sort by min_offset to process in order
        matching_segments.sort_by_key(|(_, min, _)| *min);

        let mut all_records = Vec::new();
        let mut bytes_fetched = 0usize;

        for (object_key, segment_min, segment_max) in matching_segments {
            if bytes_fetched >= max_bytes as usize && !all_records.is_empty() {
                break;
            }

            info!(
                "S3→DOWNLOAD: Downloading raw segment {} (offsets {}-{})",
                object_key, segment_min, segment_max
            );

            // Download segment from S3
            let segment_data = match self.object_store.get(&object_key).await {
                Ok(data) => data,
                Err(e) => {
                    warn!(
                        "S3→DOWNLOAD: Failed to download segment {}: {}",
                        object_key, e
                    );
                    continue; // Skip this segment, try others
                }
            };

            info!(
                "S3→DOWNLOAD: Downloaded {} bytes from {}",
                segment_data.len(), object_key
            );

            // Deserialize as Vec<CanonicalRecord> (WalIndexer format)
            // This is bincode-serialized CanonicalRecords from WAL
            let canonical_records: Vec<CanonicalRecord> = match bincode::deserialize(&segment_data) {
                Ok(records) => records,
                Err(e) => {
                    error!(
                        "S3→DESERIALIZE: Failed to deserialize canonical records from {}: {}",
                        object_key, e
                    );
                    continue;
                }
            };

            info!(
                "S3→DESERIALIZE: Segment {} contains {} canonical record batches",
                object_key, canonical_records.len()
            );

            // Extract individual records from CanonicalRecords
            for canonical_record in canonical_records {
                for entry in &canonical_record.records {
                    // Filter by offset range
                    if entry.offset >= fetch_offset && entry.offset < high_watermark {
                        let record_size = entry.value.as_ref().map(|v| v.len()).unwrap_or(0)
                            + entry.key.as_ref().map(|k| k.len()).unwrap_or(0)
                            + 24; // Estimated overhead

                        if bytes_fetched + record_size > max_bytes as usize && !all_records.is_empty() {
                            break;
                        }

                        let storage_record = chronik_storage::Record {
                            offset: entry.offset,
                            timestamp: entry.timestamp,
                            key: entry.key.clone(),
                            value: entry.value.clone().unwrap_or_default(),
                            headers: entry.headers.iter()
                                .filter_map(|h| h.value.as_ref().map(|v| (h.key.clone(), v.clone())))
                                .collect(),
                        };

                        all_records.push(storage_record);
                        bytes_fetched += record_size;
                    }
                }
            }
        }

        info!(
            "S3→COMPLETE: Fetched {} records ({} bytes) from S3 raw segments for {}-{}",
            all_records.len(), bytes_fetched, topic, partition
        );

        Ok(all_records)
    }

    /// Fetch raw RecordBatch bytes from WAL (compressed_records_wire_bytes)
    ///
    /// CRITICAL FIX (v1.3.59): Return ORIGINAL batches AS-IS by concatenation!
    /// Each batch has its ORIGINAL CRC which is only valid for that specific batch.
    /// Java Kafka clients validate CRC and will reject batches with modified CRCs.
    ///
    /// The Kafka protocol ALLOWS concatenating multiple RecordBatches in a Fetch response.
    /// This is the CORRECT approach - return the original batches exactly as stored.
    ///
    /// v1.3.47+: Direct call to WalManager (no RwLock - uses DashMap internally)
    async fn fetch_raw_bytes_from_wal(
        &self,
        wal_manager: &Arc<WalManager>,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        use chronik_storage::canonical_record::CanonicalRecord;

        let max_records = std::cmp::max(10000, max_bytes as usize / 10);

        let wal_records = wal_manager.read_from(topic, partition, fetch_offset, max_records).await
            .map_err(|e| Error::Internal(format!("WAL read failed: {}", e)))?;

        if wal_records.is_empty() {
            return Ok(None);
        }

        // CRITICAL FIX (v1.3.59): Concatenate ORIGINAL batch bytes without modification
        // Each batch's CRC is valid for its own bytes - we cannot re-encode or combine
        let mut concatenated_bytes = Vec::new();
        let mut batches_concatenated = 0;

        for wal_record in wal_records {
            if let chronik_wal::record::WalRecord::V2 { canonical_data, .. } = wal_record {
                match bincode::deserialize::<CanonicalRecord>(&canonical_data) {
                    Ok(canonical_record) => {
                        // Verify this batch's records are in the requested range
                        let base_offset = canonical_record.base_offset;
                        let last_offset = canonical_record.last_offset();

                        if last_offset >= fetch_offset && base_offset < high_watermark {
                            // CRITICAL: Call to_kafka_batch() to reconstruct the full RecordBatch
                            // with 61-byte header + compressed records payload
                            // compressed_records_wire_bytes alone is NOT a valid RecordBatch!
                            match canonical_record.to_kafka_batch() {
                                Ok(kafka_batch_bytes) => {
                                    concatenated_bytes.extend_from_slice(&kafka_batch_bytes);
                                    batches_concatenated += 1;

                                    warn!(
                                        "RAW→WAL: ✓ APPENDED reconstructed batch offsets {}-{} ({} bytes)",
                                        base_offset, last_offset, kafka_batch_bytes.len()
                                    );
                                }
                                Err(e) => {
                                    warn!("Failed to convert CanonicalRecord to Kafka batch: {}", e);
                                    continue;
                                }
                            }
                        } else {
                            warn!(
                                "RAW→WAL: ✗ SKIPPED batch offsets {}-{} (condition failed: last_offset >= fetch_offset: {}, base_offset < high_watermark: {})",
                                base_offset, last_offset,
                                last_offset >= fetch_offset,
                                base_offset < high_watermark
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Failed to deserialize CanonicalRecord from WAL V2: {}", e);
                        continue;
                    }
                }
            }
        }

        if concatenated_bytes.is_empty() {
            return Ok(None);
        }

        info!(
            "RAW→WAL: Concatenated {} original batches, total {} bytes for {}-{}",
            batches_concatenated, concatenated_bytes.len(), topic, partition
        );

        Ok(Some(concatenated_bytes))
    }

    /// Fetch records from segment files (persistent storage)
    /// This is called after checking WAL and in-memory buffers
    async fn fetch_records_from_segments(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        info!(
            "fetch_records_from_segments called - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
            topic, partition, fetch_offset, high_watermark
        );
        
        let mut records = Vec::new();
        let mut current_offset = fetch_offset;
        let mut bytes_fetched = 0;
        
        // First, determine the boundary between segments and buffer
        // Get the highest offset in segments
        let segments = self.get_segments_for_range(topic, partition, fetch_offset, high_watermark).await?;
        let max_segment_offset = segments.iter()
            .map(|s| s.last_offset)
            .max()
            .unwrap_or(-1);
        
        tracing::info!("Max segment offset: {}, fetch_offset: {}", max_segment_offset, fetch_offset);
        
        // PHASE 1: Fetch from segments if needed
        if fetch_offset <= max_segment_offset {
            // We need to fetch from segments
            for segment_info in segments {
                if bytes_fetched >= max_bytes as usize && !records.is_empty() {
                    break;
                }
                
                // Skip segments before our current offset
                if segment_info.last_offset < current_offset {
                    continue;
                }
                
                // Fetch segment data
                let segment_records = self.fetch_from_segment(
                    &segment_info,
                    current_offset,
                    std::cmp::min(high_watermark, max_segment_offset + 1), // Don't fetch beyond segment boundary
                    max_bytes - bytes_fetched as i32,
                ).await?;
                
                for record in segment_records {
                    tracing::debug!(
                        "FETCH from segment: partition={} offset={} value_len={}",
                        partition, record.offset, record.value.len()
                    );
                    
                    records.push(record.clone());
                    current_offset = record.offset + 1;
                    bytes_fetched += record.value.len() + 
                        record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;
                    
                    if bytes_fetched >= max_bytes as usize {
                        break;
                    }
                }
            }
            
            // Update current_offset to continue from after segments
            current_offset = std::cmp::max(current_offset, max_segment_offset + 1);
        }
        
        // PHASE 2: Fetch from buffer ONLY for offsets > max_segment_offset
        // CRITICAL v1.3.32 FIX: Return raw batches from buffer, not re-encoded records
        if current_offset < high_watermark && bytes_fetched < max_bytes as usize {
            let state = self.state.read().await;
            if let Some(buffer) = state.buffers.get(&(topic.to_string(), partition)) {
                tracing::info!("FETCH→BUFFER: Checking buffer for {}-{}, buffer has {} batches, current_offset={}, max_segment_offset={}, high_watermark={}",
                    topic, partition, buffer.batch_metadata.len(), current_offset, max_segment_offset, high_watermark);

                // Iterate through batches and decode records from raw bytes
                for (batch_idx, metadata) in buffer.batch_metadata.iter().enumerate() {
                    // Only include batches that overlap with our range and are not in segments
                    if metadata.last_offset >= current_offset &&
                       metadata.base_offset < high_watermark &&
                       metadata.base_offset > max_segment_offset {

                        if bytes_fetched + metadata.size_bytes > max_bytes as usize && !records.is_empty() {
                            break;
                        }

                        // Decode records from raw batch bytes
                        let raw_batch = &buffer.raw_batches[batch_idx];
                        match self.decode_records_from_raw_batch(raw_batch, current_offset, high_watermark) {
                            Ok(batch_records) => {
                                tracing::info!(
                                    "FETCH from buffer: partition={} batch_base={} decoded {} records",
                                    partition, metadata.base_offset, batch_records.len()
                                );

                                for record in batch_records {
                                    records.push(record.clone());
                                    current_offset = record.offset + 1;
                                    bytes_fetched += record.value.len() +
                                        record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to decode batch from buffer: {}", e);
                                continue;
                            }
                        }
                    }
                }
            } else {
                tracing::info!("FETCH→NO_BUFFER: No buffer found for {}-{}", topic, partition);
            }
        }
        
        tracing::info!(
            "fetch_records complete - fetched {} records from {}-{} starting at offset {} (current_offset: {})",
            records.len(), topic, partition, fetch_offset, current_offset
        );

        Ok(records)
    }

    /// Fetch raw Kafka batch bytes directly (preserves CRC) - try buffer first, then segments
    async fn fetch_raw_bytes(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        tracing::info!(
            "fetch_raw_bytes - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
            topic, partition, fetch_offset, high_watermark
        );

        // PHASE 1: Try buffer first (raw bytes already available)
        {
            let state = self.state.read().await;
            if let Some(buffer) = state.buffers.get(&(topic.to_string(), partition)) {
                tracing::info!(
                    "RAW→BUFFER: Checking buffer for {}-{}, buffer has {} batches",
                    topic, partition, buffer.batch_metadata.len()
                );

                let mut combined_bytes = Vec::new();
                for (batch_idx, metadata) in buffer.batch_metadata.iter().enumerate() {
                    // Check if this batch overlaps with requested range
                    if metadata.last_offset >= fetch_offset && metadata.base_offset < high_watermark {
                        let raw_batch = &buffer.raw_batches[batch_idx];

                        if combined_bytes.len() + raw_batch.len() > max_bytes as usize && !combined_bytes.is_empty() {
                            break;
                        }

                        tracing::info!(
                            "RAW→BUFFER: Adding {} bytes from batch at offsets {}-{}",
                            raw_batch.len(), metadata.base_offset, metadata.last_offset
                        );
                        combined_bytes.extend_from_slice(raw_batch);
                    }
                }

                if !combined_bytes.is_empty() {
                    tracing::info!(
                        "RAW→BUFFER: Returning {} bytes of raw Kafka data from buffer",
                        combined_bytes.len()
                    );
                    return Ok(Some(combined_bytes));
                }
            }
        }

        // PHASE 2: Try WAL (NEW: fetch compressed_records_wire_bytes from CanonicalRecord)
        tracing::info!("RAW→WAL: Buffer empty or no match, trying WAL");
        if let Some(wal_manager) = self.wal_manager.as_ref() {
            let raw_bytes_from_wal = self.fetch_raw_bytes_from_wal(
                wal_manager,
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes,
            ).await?;

            if let Some(bytes) = raw_bytes_from_wal {
                tracing::info!(
                    "RAW→WAL: Returning {} bytes of raw Kafka data from WAL",
                    bytes.len()
                );
                return Ok(Some(bytes));
            }
            tracing::info!("RAW→WAL: No raw bytes found in WAL");
        }

        // PHASE 3: Try segments (read raw_kafka_batches section)
        tracing::info!("RAW→SEGMENTS: Buffer and WAL empty or no match, trying segments");
        let segments = self.get_segments_for_range(topic, partition, fetch_offset, high_watermark).await?;

        if segments.is_empty() {
            tracing::info!("RAW→SEGMENTS: No segments found for range");
            return Ok(None);
        }

        let mut combined_bytes = Vec::new();
        for segment_info in &segments {
            // Read segment and extract raw_kafka_batches
            let segment_data = self.object_store.get(&segment_info.object_key).await?;
            let segment = Segment::deserialize(segment_data)?;

            tracing::info!(
                "RAW→SEGMENT: Segment {} has {} bytes of raw_kafka_batches",
                segment_info.segment_id, segment.raw_kafka_batches.len()
            );

            if segment.raw_kafka_batches.is_empty() {
                // Segment doesn't have raw bytes (v1 format or indexed-only)
                // Cannot preserve CRC, need to fall back to parsed records
                tracing::warn!(
                    "RAW→SEGMENT: Segment {} has no raw_kafka_batches, cannot preserve CRC",
                    segment_info.segment_id
                );
                return Ok(None);
            }

            // CRITICAL FIX: Parse batch headers to filter by offset range
            // We must ONLY include batches that overlap [fetch_offset, high_watermark)
            // Otherwise we return wrong batches and clients see CRC errors!
            let mut cursor_pos = 0;

            while cursor_pos < segment.raw_kafka_batches.len() {
                let batch_start = cursor_pos;

                // Read batch header (minimum 61 bytes for v2 format)
                if (segment.raw_kafka_batches.len() - batch_start) < 61 {
                    // Not enough bytes for a valid batch
                    break;
                }

                // Parse JUST the header to get offsets (without decoding records)
                // Use manual big-endian byte parsing to avoid any trait complications
                let base_offset = i64::from_be_bytes([
                    segment.raw_kafka_batches[batch_start],
                    segment.raw_kafka_batches[batch_start + 1],
                    segment.raw_kafka_batches[batch_start + 2],
                    segment.raw_kafka_batches[batch_start + 3],
                    segment.raw_kafka_batches[batch_start + 4],
                    segment.raw_kafka_batches[batch_start + 5],
                    segment.raw_kafka_batches[batch_start + 6],
                    segment.raw_kafka_batches[batch_start + 7],
                ]);

                let batch_length = i32::from_be_bytes([
                    segment.raw_kafka_batches[batch_start + 8],
                    segment.raw_kafka_batches[batch_start + 9],
                    segment.raw_kafka_batches[batch_start + 10],
                    segment.raw_kafka_batches[batch_start + 11],
                ]);

                // Read last_offset_delta at offset 23 (after base_offset, batch_length, partition_leader_epoch, magic, crc, attributes)
                let last_offset_delta = i32::from_be_bytes([
                    segment.raw_kafka_batches[batch_start + 23],
                    segment.raw_kafka_batches[batch_start + 24],
                    segment.raw_kafka_batches[batch_start + 25],
                    segment.raw_kafka_batches[batch_start + 26],
                ]);
                let last_offset = base_offset + last_offset_delta as i64;

                // Total batch size is: 12 bytes (base_offset + batch_length) + batch_length
                let total_batch_size = 12 + batch_length as usize;

                tracing::debug!(
                    "RAW→BATCH: Found batch at offset {}, base_offset={}, last_offset={}, size={}",
                    batch_start, base_offset, last_offset, total_batch_size
                );

                // Check if this batch overlaps with requested range [fetch_offset, high_watermark)
                if last_offset >= fetch_offset && base_offset < high_watermark {
                    // This batch is in range, include it
                    if combined_bytes.len() + total_batch_size > max_bytes as usize && !combined_bytes.is_empty() {
                        // Would exceed max_bytes, stop here
                        break;
                    }

                    let batch_bytes = &segment.raw_kafka_batches[batch_start..batch_start + total_batch_size];
                    combined_bytes.extend_from_slice(batch_bytes);

                    tracing::info!(
                        "RAW→BATCH: Including {} bytes from batch {}-{}",
                        total_batch_size, base_offset, last_offset
                    );
                } else {
                    tracing::debug!(
                        "RAW→BATCH: Skipping batch {}-{} (outside range {}-{})",
                        base_offset, last_offset, fetch_offset, high_watermark
                    );
                }

                // Move to next batch
                cursor_pos = batch_start + total_batch_size;
            }
        }

        if !combined_bytes.is_empty() {
            tracing::info!(
                "RAW→SEGMENTS: Returning {} bytes of raw Kafka data from {} segments",
                combined_bytes.len(), segments.len()
            );
            return Ok(Some(combined_bytes));
        }

        // PHASE 3: Try Tantivy segments (warm storage)
        if let Some(ref segment_index) = self.segment_index {
            tracing::info!("RAW→TANTIVY: Checking Tantivy segment index for {}-{}", topic, partition);

            match self.fetch_from_tantivy(
                segment_index,
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes,
            ).await {
                Ok(Some(bytes)) => {
                    tracing::info!(
                        "RAW→TANTIVY: Returning {} bytes from Tantivy segments",
                        bytes.len()
                    );
                    return Ok(Some(bytes));
                }
                Ok(None) => {
                    tracing::info!("RAW→TANTIVY: No matching Tantivy segments found");
                }
                Err(e) => {
                    tracing::warn!("RAW→TANTIVY: Error fetching from Tantivy: {}", e);
                }
            }
        }

        Ok(None)
    }

    /// Fetch records from Tantivy archives (cold storage) - for consumption
    /// This is similar to fetch_from_tantivy but returns parsed Record objects instead of raw bytes
    async fn fetch_from_tantivy_for_records(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        _max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        let segment_index = match &self.segment_index {
            Some(idx) => idx,
            None => return Ok(vec![]),
        };

        // Query segment index for matching Tantivy segments
        let tantivy_segments = segment_index.find_segments_by_offset_range(
            topic,
            partition,
            fetch_offset,
            high_watermark,
        ).await?;

        if tantivy_segments.is_empty() {
            debug!("No Tantivy segments found for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(vec![]);
        }

        info!(
            "Found {} Tantivy segments for {}-{} range {}-{}, downloading and reading",
            tantivy_segments.len(), topic, partition, fetch_offset, high_watermark
        );

        // Collect all entries from all matching segments
        let mut all_entries = Vec::new();

        for segment_metadata in tantivy_segments {
            // Download segment from object store
            let segment_data = match self.object_store.get(&segment_metadata.object_store_path).await {
                Ok(data) => data,
                Err(e) => {
                    warn!(
                        "Failed to download Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue; // Skip this segment, try others
                }
            };

            // Deserialize Tantivy segment
            let reader = match TantivySegmentReader::from_tar_gz_bytes(segment_data.as_ref()) {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        "Failed to deserialize Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            // Query for records in the offset range
            let entries = match reader.query_by_offset_range(fetch_offset, high_watermark) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        "Failed to query Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            debug!(
                "Read {} entries from Tantivy segment {}",
                entries.len(), segment_metadata.segment_id
            );

            all_entries.extend(entries);
        }

        if all_entries.is_empty() {
            info!("No entries found in Tantivy segments for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(vec![]);
        }

        // Sort entries by offset
        all_entries.sort_by_key(|e| e.offset);

        // Convert entries to chronik_storage::Record
        let records: Vec<chronik_storage::Record> = all_entries.into_iter()
            .map(|entry| chronik_storage::Record {
                offset: entry.offset,
                timestamp: entry.timestamp,
                key: entry.key,
                value: entry.value.unwrap_or_default(),
                headers: entry.headers.iter()
                    .filter_map(|h| h.value.as_ref().map(|v| (h.key.clone(), v.clone())))
                    .collect(),
            })
            .collect();

        info!(
            "Returning {} records from Tantivy segments for {}-{} range {}-{}",
            records.len(), topic, partition, fetch_offset, high_watermark
        );

        Ok(records)
    }

    /// Fetch data from Tantivy segments (warm storage) - returns raw bytes
    async fn fetch_from_tantivy(
        &self,
        segment_index: &Arc<SegmentIndex>,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        _max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        use chronik_storage::canonical_record::{CanonicalRecord, CompressionType, TimestampType};

        // Query segment index for matching Tantivy segments
        let tantivy_segments = segment_index.find_segments_by_offset_range(
            topic,
            partition,
            fetch_offset,
            high_watermark,
        ).await?;

        if tantivy_segments.is_empty() {
            tracing::debug!("No Tantivy segments found for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(None);
        }

        tracing::info!(
            "Found {} Tantivy segments for {}-{} range {}-{}, downloading and reading",
            tantivy_segments.len(), topic, partition, fetch_offset, high_watermark
        );

        // Collect all entries from all matching segments
        let mut all_entries = Vec::new();

        for segment_metadata in tantivy_segments {
            // Download segment from object store
            let segment_data = match self.object_store.get(&segment_metadata.object_store_path).await {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        "Failed to download Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue; // Skip this segment, try others
                }
            };

            // Deserialize Tantivy segment
            let reader = match TantivySegmentReader::from_tar_gz_bytes(segment_data.as_ref()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "Failed to deserialize Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            // Query for records in the offset range
            let entries = match reader.query_by_offset_range(fetch_offset, high_watermark) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        "Failed to query Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            tracing::debug!(
                "Read {} entries from Tantivy segment {}",
                entries.len(), segment_metadata.segment_id
            );

            all_entries.extend(entries);
        }

        if all_entries.is_empty() {
            tracing::info!("No entries found in Tantivy segments for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(None);
        }

        // Sort entries by offset (should already be sorted, but ensure correctness)
        all_entries.sort_by_key(|e| e.offset);

        // Reconstruct CanonicalRecord from entries
        // Note: We use default compression (None) since we're serving the data uncompressed
        let canonical_record = CanonicalRecord::from_entries(
            all_entries,
            CompressionType::None,
            TimestampType::CreateTime,
        )?;

        // Convert to Kafka wire format
        let kafka_batch = canonical_record.to_kafka_batch()?;

        tracing::info!(
            "Returning {} bytes from Tantivy segments for {}-{} range {}-{}",
            kafka_batch.len(), topic, partition, fetch_offset, high_watermark
        );

        Ok(Some(kafka_batch.to_vec()))
    }

    /// Get segments that contain data in the given offset range
    async fn get_segments_for_range(
        &self,
        topic: &str,
        partition: i32,
        start_offset: i64,
        end_offset: i64,
    ) -> Result<Vec<SegmentInfo>> {
        tracing::info!(
            "get_segments_for_range - topic: {}, partition: {}, range: {}-{}",
            topic, partition, start_offset, end_offset
        );
        // Check cache first
        {
            let state = self.state.read().await;
            if let Some(cached) = state.segment_cache.get(&(topic.to_string(), partition)) {
                let relevant: Vec<_> = cached.iter()
                    .filter(|s| s.last_offset >= start_offset && s.base_offset < end_offset)
                    .cloned()
                    .collect();
                
                if !relevant.is_empty() {
                    return Ok(relevant);
                }
            }
        }
        
        // Query metadata store
        tracing::warn!("SEGMENT→QUERY: Requesting segments from metadata store for {}-{}", topic, partition);
        let segments = self.metadata_store.list_segments(topic, Some(partition as u32)).await?;
        
        tracing::warn!(
            "SEGMENT→QUERY: Retrieved {} total segments from metadata store for {}-{}",
            segments.len(), topic, partition
        );
        
        for seg in &segments {
            tracing::debug!(
                "  Segment {}: offsets {}-{}, path: {}",
                seg.segment_id, seg.start_offset, seg.end_offset, seg.path
            );
        }
        
        let segment_infos: Vec<_> = segments.into_iter()
            .filter(|s| {
                // A segment is relevant if it overlaps with our range
                let overlaps = s.end_offset >= start_offset && s.start_offset < end_offset;
                if overlaps {
                    tracing::info!(
                        "  Including segment {} (offsets {}-{}) for range {}-{}",
                        s.segment_id, s.start_offset, s.end_offset, start_offset, end_offset
                    );
                }
                overlaps
            })
            .map(|s| SegmentInfo {
                segment_id: s.segment_id,
                base_offset: s.start_offset,
                last_offset: s.end_offset,
                object_key: s.path,
            })
            .collect();
        
        tracing::info!(
            "Filtered to {} segments for offset range {}-{}",
            segment_infos.len(), start_offset, end_offset
        );
        
        // Update cache
        {
            let mut state = self.state.write().await;
            state.segment_cache.insert((topic.to_string(), partition), segment_infos.clone());
        }
        
        Ok(segment_infos)
    }
    
    /// Fetch records from a specific segment
    async fn fetch_from_segment(
        &self,
        segment_info: &SegmentInfo,
        start_offset: i64,
        end_offset: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        tracing::info!("Fetching from segment {} (offsets {}-{}) with key: {}",
            segment_info.segment_id, start_offset, end_offset, segment_info.object_key);

        // Debug: Log the full path being accessed
        tracing::warn!("SEGMENT→READ: Attempting to read segment from object store with key: {}",
            segment_info.object_key);

        // Read segment from storage using the correct Segment format (CHRN magic bytes)
        let segment_data = match self.object_store.get(&segment_info.object_key).await {
            Ok(data) => {
                tracing::info!("SEGMENT→READ: Successfully read segment {} ({} bytes)",
                    segment_info.object_key, data.len());
                data
            }
            Err(e) => {
                tracing::error!("SEGMENT→READ: Failed to read segment {}: {:?}",
                    segment_info.object_key, e);

                // Try to understand where the file should be
                tracing::error!("SEGMENT→DEBUG: The segment file was expected at key: {}",
                    segment_info.object_key);
                tracing::error!("SEGMENT→DEBUG: This typically maps to: ./data/segments/{}",
                    segment_info.object_key);

                return Err(e.into());
            }
        };
        
        // Parse using the Segment format - this is what SegmentWriter creates
        let segment = Segment::deserialize(segment_data)?;
        
        tracing::info!("Successfully parsed segment v{} with {} records, raw_kafka: {} bytes, indexed: {} bytes", 
            segment.header.version,
            segment.metadata.record_count, 
            segment.raw_kafka_batches.len(),
            segment.indexed_records.len());
        
        // CRITICAL FIX v1.3.23: Multi-batch decode with v3 length prefixes
        // v3 format: Each batch is prefixed with u32 length
        // v2 format: Batches concatenated without length (only first batch readable - BUG!)
        // This fixes the multi-batch bug where only first batch was deserialized
        let mut all_records = Vec::new();
        let mut batch_count = 0;
        let mut cursor_pos = 0;
        let total_len = segment.indexed_records.len();

        // Check segment version to determine format
        let is_v3_format = segment.header.version >= 3;

        tracing::info!(
            "SEGMENT→DECODE: Starting multi-batch decode from indexed_records ({} bytes, v{} format)",
            total_len, segment.header.version
        );

        while cursor_pos < total_len {
            // v3 format: Read length prefix first
            let batch_data_start = if is_v3_format {
                // Need at least 4 bytes for length prefix
                if cursor_pos + 4 > total_len {
                    tracing::info!("SEGMENT→DECODE: Not enough bytes for length prefix at position {}", cursor_pos);
                    break;
                }

                // Read u32 length prefix (big-endian)
                let batch_len = u32::from_be_bytes([
                    segment.indexed_records[cursor_pos],
                    segment.indexed_records[cursor_pos + 1],
                    segment.indexed_records[cursor_pos + 2],
                    segment.indexed_records[cursor_pos + 3],
                ]) as usize;

                tracing::debug!("SEGMENT→V3: Batch {} has length prefix {} bytes", batch_count + 1, batch_len);

                // Move cursor past length prefix
                cursor_pos + 4
            } else {
                // v2 format: No length prefix, try to decode from current position
                cursor_pos
            };

            // Calculate end position for this batch
            let batch_data_end = if is_v3_format {
                let batch_len = u32::from_be_bytes([
                    segment.indexed_records[cursor_pos],
                    segment.indexed_records[cursor_pos + 1],
                    segment.indexed_records[cursor_pos + 2],
                    segment.indexed_records[cursor_pos + 3],
                ]) as usize;
                batch_data_start + batch_len
            } else {
                total_len  // For v2, decode will determine the end
            };

            // Ensure we have enough data
            if batch_data_end > total_len {
                tracing::error!(
                    "SEGMENT→ERROR: Batch extends beyond segment bounds (pos={}, end={}, total={})",
                    batch_data_start, batch_data_end, total_len
                );
                break;
            }

            // Decode the batch
            match RecordBatch::decode(&segment.indexed_records[batch_data_start..batch_data_end]) {
                Ok((batch, bytes_consumed)) => {
                    let batch_records = batch.records.len();
                    tracing::info!(
                        "SEGMENT→BATCH {}: Decoded {} records, {} bytes at position {}",
                        batch_count + 1,
                        batch_records,
                        if is_v3_format { batch_data_end - batch_data_start } else { bytes_consumed },
                        cursor_pos
                    );

                    all_records.extend(batch.records);

                    // Advance cursor
                    if is_v3_format {
                        // v3: Move to next length prefix
                        cursor_pos = batch_data_end;
                    } else {
                        // v2: Use bytes_consumed from decode
                        cursor_pos += bytes_consumed;

                        // Safety check for v2 format
                        if bytes_consumed == 0 {
                            tracing::error!("SEGMENT→ERROR: Zero bytes consumed in v2 format, breaking");
                            break;
                        }
                    }

                    batch_count += 1;
                }
                Err(e) => {
                    if is_v3_format {
                        // v3: Length prefix told us exact size, decode should not fail
                        tracing::error!(
                            "SEGMENT→ERROR: Failed to decode v3 batch {} at position {}: {}",
                            batch_count + 1, cursor_pos, e
                        );
                        break;
                    } else {
                        // v2: Expected to fail after last batch (no length prefix to know when to stop)
                        tracing::info!(
                            "SEGMENT→DECODE: Finished v2 format at position {} ({} bytes remaining): {}",
                            cursor_pos,
                            total_len - cursor_pos,
                            e
                        );
                        break;
                    }
                }
            }
        }

        tracing::info!(
            "SEGMENT→COMPLETE: Decoded {} batches with {} total records from indexed_records",
            batch_count,
            all_records.len()
        );

        // CRITICAL FIX: Adjust record offsets if they're stored as relative offsets
        // Records in indexed_records may be stored with relative offsets (0, 1, 2...)
        // Need to adjust them to absolute partition offsets by adding base_offset
        let base_offset = segment.metadata.base_offset;
        if base_offset > 0 && !all_records.is_empty() {
            // Check if offsets need adjustment (if first record offset is small, likely relative)
            let first_offset = all_records[0].offset;
            if first_offset < base_offset {
                tracing::info!(
                    "SEGMENT→ADJUST: Adjusting {} record offsets by base_offset={} (first record offset was {})",
                    all_records.len(),
                    base_offset,
                    first_offset
                );
                for record in &mut all_records {
                    record.offset += base_offset;
                }
            }
        }

        if !segment.raw_kafka_batches.is_empty() {
            // ALWAYS prefer raw Kafka batches when available (preserves CRC from original produce request)
            // Dual storage (v2) has both indexed + raw, but raw has correct CRC
            tracing::info!("Using raw Kafka batches for fetch (CRC-preserving format, {} bytes)", segment.raw_kafka_batches.len());
            all_records.clear();  // Clear any indexed records, use raw instead

            let mut cursor_pos = 0;
            let total_len = segment.raw_kafka_batches.len();
            let mut batch_count = 0;
            let mut current_absolute_offset = segment.metadata.base_offset;

            tracing::info!(
                "SEGMENT→KAFKA: Starting multi-batch decode from raw_kafka_batches ({} bytes, base_offset={})",
                total_len,
                current_absolute_offset
            );

            while cursor_pos < total_len {
                // Kafka batches are self-describing - decode will use batch_length from header
                match KafkaRecordBatch::decode(&segment.raw_kafka_batches[cursor_pos..]) {
                    Ok((kafka_batch, bytes_consumed)) => {
                        // CRITICAL: Use segment metadata's base_offset, NOT client's base_offset from Kafka batch header
                        // The client's base_offset in the Kafka batch header is often incorrect (e.g., 1 instead of 0)
                        // We must use the segment's actual offsets which are tracked server-side
                        let records: Vec<Record> = kafka_batch.records.into_iter().enumerate().map(|(i, kr)| {
                            Record {
                                offset: current_absolute_offset + i as i64,
                                timestamp: kafka_batch.header.base_timestamp + kr.timestamp_delta,
                                key: kr.key.map(|k| k.to_vec()),
                                value: kr.value.map(|v| v.to_vec()).unwrap_or_default(),
                                headers: kr.headers.into_iter().map(|h| {
                                    (h.key, h.value.map(|v| v.to_vec()).unwrap_or_default())
                                }).collect(),
                            }
                        }).collect();

                        let batch_records = records.len();
                        tracing::debug!(
                            "Decoded batch {}: {} records (offsets {}-{})",
                            batch_count + 1,
                            batch_records,
                            current_absolute_offset,
                            current_absolute_offset + batch_records as i64 - 1
                        );

                        // Increment absolute offset for next batch
                        current_absolute_offset += batch_records as i64;

                        all_records.extend(records);

                        // Advance cursor using bytes_consumed from Kafka decode
                        cursor_pos += bytes_consumed;

                        // Safety check
                        if bytes_consumed == 0 {
                            tracing::error!("SEGMENT→KAFKA: Zero bytes consumed, breaking to avoid infinite loop");
                            break;
                        }

                        batch_count += 1;
                    }
                    Err(e) => {
                        // Expected to fail when we run out of complete batches
                        tracing::info!(
                            "SEGMENT→KAFKA: Finished decoding at position {} ({} bytes remaining): {}",
                            cursor_pos,
                            total_len - cursor_pos,
                            e
                        );
                        break;
                    }
                }
            }

            tracing::info!(
                "SEGMENT→KAFKA: Decoded {} batches with {} total records from raw_kafka_batches",
                batch_count,
                all_records.len()
            );
        }

        // If still no records after trying both indexed and raw formats, return empty
        if all_records.is_empty() {
            tracing::warn!("SEGMENT→EMPTY: No records decoded from segment");
            return Ok(vec![]);
        }

        let record_batch = RecordBatch { records: all_records };

        // Filter records by offset range
        let mut filtered_records = Vec::new();
        let mut bytes_fetched = 0;

        for record in record_batch.records {
            if record.offset >= start_offset && record.offset < end_offset {
                let record_size = record.value.len() +
                    record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;

                if bytes_fetched + record_size > max_bytes as usize && !filtered_records.is_empty() {
                    break;
                }

                filtered_records.push(record);
                bytes_fetched += record_size;
            }
        }
        
        tracing::info!("Returning {} records after filtering for offset range {}-{}", 
            filtered_records.len(), start_offset, end_offset);
        
        Ok(filtered_records)
    }
    
    /// Fetch raw Kafka batch data from a segment (for compatibility)
    async fn fetch_raw_kafka_batch(
        &self,
        segment_info: &SegmentInfo,
        start_offset: i64,
        end_offset: i64,
    ) -> Result<Vec<u8>> {
        tracing::info!("Fetching raw Kafka batch from segment {} (offsets {}-{})", 
            segment_info.segment_id, start_offset, end_offset);
        
        // Read segment from storage
        let segment_data = self.object_store.get(&segment_info.object_key).await?;
        
        // Parse using the Segment format
        let segment = Segment::deserialize(segment_data)?;
        
        tracing::info!("Segment v{}: raw_kafka_batches={} bytes, indexed_records={} bytes",
            segment.header.version, segment.raw_kafka_batches.len(), segment.indexed_records.len());
        
        // Check if this is a v2 segment with raw Kafka batches
        if segment.header.version >= 2 && !segment.raw_kafka_batches.is_empty() {
            // Return the raw Kafka batch data which preserves the original wire format
            Ok(segment.raw_kafka_batches.to_vec())
        } else {
            // For v1 segments, we don't have raw Kafka batches
            // Return empty to indicate we need to re-encode
            Ok(vec![])
        }
    }
    
    /// Try to fetch raw Kafka batches for a range (for CRC compatibility)
    async fn try_fetch_raw_batches(
        &self,
        topic: &str,
        partition: i32,
        start_offset: i64,
        end_offset: i64,
    ) -> Result<Vec<u8>> {
        // Get segments for this range
        let segments = self.get_segments_for_range(topic, partition, start_offset, end_offset).await?;
        
        if segments.is_empty() {
            return Ok(vec![]);
        }
        
        // Concatenate raw Kafka batches from all segments in the range
        let mut combined_bytes = Vec::new();
        let num_segments = segments.len();
        
        for segment in segments {
            tracing::info!("Fetching raw Kafka batch from segment {} (offsets {}-{})", 
                segment.segment_id, segment.base_offset, segment.last_offset);
            
            // Fetch raw batch for this segment
            match self.fetch_raw_kafka_batch(&segment, 
                start_offset.max(segment.base_offset), 
                end_offset.min(segment.last_offset + 1)).await {
                Ok(raw_bytes) if !raw_bytes.is_empty() => {
                    tracing::info!("Got {} bytes of raw Kafka data from segment {}", 
                        raw_bytes.len(), segment.segment_id);
                    combined_bytes.extend_from_slice(&raw_bytes);
                }
                Ok(_) => {
                    tracing::warn!("No raw Kafka data in segment {} (may be v1 segment)", segment.segment_id);
                    // If any segment doesn't have raw data, we can't preserve CRCs
                    return Ok(vec![]);
                }
                Err(e) => {
                    tracing::error!("Error fetching raw batch from segment {}: {:?}", segment.segment_id, e);
                    return Ok(vec![]);
                }
            }
        }
        
        tracing::info!("Combined {} bytes of raw Kafka data from {} segments", 
            combined_bytes.len(), num_segments);
        Ok(combined_bytes)
    }
    
    /// Get the high watermark and log start offset for a partition (used by ListOffsets)
    pub async fn get_partition_offsets(&self, topic: &str, partition: i32) -> Result<(i64, i64)> {
        // Calculate high watermark from segments
        let segments = self.metadata_store.list_segments(topic, Some(partition as u32)).await?;
        let segment_high_watermark = segments.iter()
            .map(|s| s.end_offset + 1)
            .max()
            .unwrap_or(0);
        
        // Also check buffer for high watermark
        let buffer_high_watermark = {
            let state = self.state.read().await;
            let key = (topic.to_string(), partition);
            state.buffers.get(&key).map(|b| b.high_watermark).unwrap_or(0)
        };
        
        // Use the maximum of segment and buffer high watermarks
        let high_watermark = segment_high_watermark.max(buffer_high_watermark);
        
        // Log start offset is always 0 for now (we don't do deletion)
        let log_start_offset = 0;
        
        Ok((high_watermark, log_start_offset))
    }
    
    /// Update in-memory buffer with raw Kafka batch bytes (v1.3.32 FIX)
    /// CRITICAL: Stores original wire-format bytes to preserve CRC
    pub async fn update_buffer_with_raw_batch(
        &self,
        topic: &str,
        partition: i32,
        raw_bytes: &[u8],
        base_offset: i64,
        last_offset: i64,
        record_count: i32,
        high_watermark: i64,
    ) -> Result<()> {
        if raw_bytes.is_empty() {
            return Ok(());
        }

        tracing::warn!(
            "BUFFER→RAW_UPDATE: Storing {} bytes for {}-{}, offset_range=[{}-{}], count={}, high_watermark={}",
            raw_bytes.len(), topic, partition, base_offset, last_offset, record_count, high_watermark
        );

        let mut state = self.state.write().await;
        let key = (topic.to_string(), partition);

        let buffer = state.buffers.entry(key.clone()).or_insert(PartitionBuffer {
            raw_batches: Vec::new(),
            batch_metadata: Vec::new(),
            base_offset,
            high_watermark,
            flushed_offset: -1,
            min_offset_in_buffer: base_offset,  // v1.3.48: Track actual minimum offset in buffer
        });

        // Check for duplicate batches (same base_offset)
        if !buffer.batch_metadata.iter().any(|m| m.base_offset == base_offset) {
            // Store raw bytes
            buffer.raw_batches.push(bytes::Bytes::copy_from_slice(raw_bytes));

            // Store metadata
            buffer.batch_metadata.push(BatchMetadata {
                base_offset,
                last_offset,
                record_count,
                size_bytes: raw_bytes.len(),
            });

            tracing::info!(
                "BUFFER→RAW_STORED: Added batch to {}-{}, now has {} batches",
                topic, partition, buffer.raw_batches.len()
            );
        } else {
            tracing::warn!(
                "BUFFER→RAW_SKIP: Skipping duplicate batch at offset {} for {}-{}",
                base_offset, topic, partition
            );
        }

        // Update high watermark
        buffer.high_watermark = high_watermark;

        // Update base_offset if this is the first batch or earlier
        if buffer.batch_metadata.is_empty() || base_offset < buffer.base_offset {
            buffer.base_offset = base_offset;
        }

        // Trim old batches if buffer too large (keep last 100 batches)
        // v1.3.48: Track min_offset_in_buffer to detect gaps and trigger WAL fallback
        if buffer.raw_batches.len() > 100 {
            let trim_count = buffer.raw_batches.len() - 100;

            // Track what we're trimming for logging
            let first_trimmed = buffer.batch_metadata[0].base_offset;
            let last_trimmed = buffer.batch_metadata[trim_count - 1].last_offset;

            buffer.raw_batches.drain(0..trim_count);
            buffer.batch_metadata.drain(0..trim_count);

            if let Some(first_meta) = buffer.batch_metadata.first() {
                buffer.base_offset = first_meta.base_offset;
                buffer.min_offset_in_buffer = first_meta.base_offset;  // v1.3.48: Update min after trim
            }

            warn!(
                "BUFFER→TRIM: Trimmed {} old batches from {}-{} (offsets {}-{}), {} batches remain, min_offset_in_buffer={}, data still in WAL",
                trim_count, topic, partition, first_trimmed, last_trimmed,
                buffer.raw_batches.len(), buffer.min_offset_in_buffer
            );
        }

        Ok(())
    }

    /// Decode records from raw Kafka RecordBatch bytes
    /// CRITICAL v1.3.32: Decode original bytes instead of re-encoding
    fn decode_records_from_raw_batch(
        &self,
        raw_bytes: &[u8],
        min_offset: i64,
        max_offset: i64,
    ) -> Result<Vec<chronik_storage::Record>> {
        use bytes::Buf;

        let mut cursor = raw_bytes;
        let mut records = Vec::new();

        // Skip RecordBatch header to get to records
        // RecordBatch format v2:
        // - base_offset (8 bytes)
        // - batch_length (4 bytes)
        // - partition_leader_epoch (4 bytes)
        // - magic (1 byte)
        // - crc (4 bytes)
        // - attributes (2 bytes)
        // - last_offset_delta (4 bytes)
        // - base_timestamp (8 bytes)
        // - max_timestamp (8 bytes)
        // - producer_id (8 bytes)
        // - producer_epoch (2 bytes)
        // - base_sequence (4 bytes)
        // - record_count (4 bytes)
        // Total header: 61 bytes

        if raw_bytes.len() < 61 {
            return Err(Error::Protocol("RecordBatch too small".into()));
        }

        let base_offset = cursor.get_i64();
        cursor.advance(4); // batch_length
        cursor.advance(4); // partition_leader_epoch
        cursor.advance(1); // magic
        cursor.advance(4); // crc
        cursor.advance(2); // attributes
        cursor.advance(4); // last_offset_delta
        let base_timestamp = cursor.get_i64();
        cursor.advance(8); // max_timestamp
        cursor.advance(8); // producer_id
        cursor.advance(2); // producer_epoch
        cursor.advance(4); // base_sequence
        let record_count = cursor.get_i32();

        // Parse individual records
        for _ in 0..record_count {
            if cursor.remaining() == 0 {
                break;
            }

            // Read record length (varint)
            let length = self.read_varint(&mut cursor)?;
            if cursor.remaining() < length as usize {
                break;
            }

            // Read attributes (1 byte)
            cursor.advance(1);

            // Read timestamp delta (varint)
            let timestamp_delta = self.read_varlong(&mut cursor)?;

            // Read offset delta (varint)
            let offset_delta = self.read_varint(&mut cursor)?;
            let record_offset = base_offset + offset_delta as i64;

            // Check if record is in requested range
            if record_offset < min_offset || record_offset >= max_offset {
                // Skip this record
                let key_len = self.read_varint(&mut cursor)?;
                if key_len >= 0 {
                    cursor.advance(key_len as usize);
                }
                let value_len = self.read_varint(&mut cursor)?;
                if value_len >= 0 {
                    cursor.advance(value_len as usize);
                }
                let header_count = self.read_varint(&mut cursor)?;
                for _ in 0..header_count {
                    let key_len = self.read_varint(&mut cursor)?;
                    cursor.advance(key_len as usize);
                    let val_len = self.read_varint(&mut cursor)?;
                    cursor.advance(val_len as usize);
                }
                continue;
            }

            // Read key (varint length + bytes)
            let key_len = self.read_varint(&mut cursor)?;
            let key = if key_len >= 0 {
                let mut key_bytes = vec![0u8; key_len as usize];
                cursor.copy_to_slice(&mut key_bytes);
                Some(key_bytes)
            } else {
                None
            };

            // Read value (varint length + bytes)
            let value_len = self.read_varint(&mut cursor)?;
            let value = if value_len >= 0 {
                let mut value_bytes = vec![0u8; value_len as usize];
                cursor.copy_to_slice(&mut value_bytes);
                value_bytes
            } else {
                vec![]
            };

            // Read headers (varint count, then key-value pairs)
            let header_count = self.read_varint(&mut cursor)?;
            let mut headers = std::collections::HashMap::new();
            for _ in 0..header_count {
                let key_len = self.read_varint(&mut cursor)?;
                let mut key_bytes = vec![0u8; key_len as usize];
                cursor.copy_to_slice(&mut key_bytes);
                let key_str = String::from_utf8_lossy(&key_bytes).to_string();

                let val_len = self.read_varint(&mut cursor)?;
                let mut val_bytes = vec![0u8; val_len as usize];
                cursor.copy_to_slice(&mut val_bytes);

                headers.insert(key_str, val_bytes);
            }

            records.push(chronik_storage::Record {
                offset: record_offset,
                timestamp: base_timestamp + timestamp_delta,
                key,
                value,
                headers,
            });
        }

        Ok(records)
    }

    /// Read varint from byte slice (zigzag encoded)
    fn read_varint(&self, cursor: &mut &[u8]) -> Result<i32> {
        use bytes::Buf;
        let mut result: i32 = 0;
        let mut shift = 0;
        loop {
            if cursor.remaining() == 0 {
                return Err(Error::Protocol("Unexpected end of varint".into()));
            }
            let byte = cursor.get_u8();
            result |= ((byte & 0x7F) as i32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        // Zigzag decode
        Ok((result >> 1) ^ -(result & 1))
    }

    /// Read varlong from byte slice (zigzag encoded)
    fn read_varlong(&self, cursor: &mut &[u8]) -> Result<i64> {
        use bytes::Buf;
        let mut result: i64 = 0;
        let mut shift = 0;
        loop {
            if cursor.remaining() == 0 {
                return Err(Error::Protocol("Unexpected end of varlong".into()));
            }
            let byte = cursor.get_u8();
            result |= ((byte & 0x7F) as i64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        // Zigzag decode
        Ok((result >> 1) ^ -(result & 1))
    }

    /// Update in-memory buffer with new records (DEPRECATED - DO NOT USE)
    /// DEPRECATED v1.3.32: This function re-encodes records and corrupts CRC
    /// Use update_buffer_with_raw_batch instead to preserve wire-format bytes
    #[deprecated(since = "1.3.32", note = "Use update_buffer_with_raw_batch to preserve CRC")]
    pub async fn update_buffer(
        &self,
        _topic: &str,
        _partition: i32,
        _records: Vec<chronik_storage::Record>,
        _high_watermark: i64,
    ) -> Result<()> {
        tracing::error!("DEPRECATED: update_buffer() was called but should not be used. Use update_buffer_with_raw_batch() instead.");
        Err(Error::Internal("update_buffer is deprecated - use update_buffer_with_raw_batch".into()))
    }
    
    /// Clear buffers for a topic
    pub async fn clear_topic_buffers(&self, topic: &str) -> Result<()> {
        let mut state = self.state.write().await;
        state.buffers.retain(|(t, _), _| t != topic);
        state.segment_cache.retain(|(t, _), _| t != topic);
        Ok(())
    }
    
    /// Mark batches as flushed to segment (removes only flushed batches from buffer)
    /// CRITICAL v1.3.32 FIX: Use batch_metadata instead of records to track flushed data
    pub async fn mark_flushed(
        &self,
        topic: &str,
        partition: i32,
        up_to_offset: i64,
    ) -> Result<()> {
        tracing::info!("FLUSH→MARK: Marking batches as flushed for {}-{}, up_to_offset={}",
            topic, partition, up_to_offset);

        let mut state = self.state.write().await;
        let key = (topic.to_string(), partition);

        if let Some(buffer) = state.buffers.get_mut(&key) {
            let initial_count = buffer.raw_batches.len();

            tracing::info!("FLUSH→BEFORE: Buffer for {}-{} has {} batches before flush",
                topic, partition, initial_count);

            // Log which batches will be removed
            for metadata in &buffer.batch_metadata {
                if metadata.last_offset <= up_to_offset {
                    tracing::info!("FLUSH→REMOVE: Will remove batch base_offset={}, last_offset={} from {}-{} (last_offset <= {})",
                        metadata.base_offset, metadata.last_offset, topic, partition, up_to_offset);
                }
            }

            // CRITICAL FIX: Remove batches where last_offset <= up_to_offset
            // Keep batches where last_offset > up_to_offset (not fully flushed yet)
            let mut i = 0;
            let mut removed_count = 0;
            while i < buffer.batch_metadata.len() {
                if buffer.batch_metadata[i].last_offset <= up_to_offset {
                    buffer.raw_batches.remove(i);
                    buffer.batch_metadata.remove(i);
                    removed_count += 1;
                } else {
                    i += 1;
                }
            }

            // Update base_offset if we removed batches
            if !buffer.batch_metadata.is_empty() && removed_count > 0 {
                buffer.base_offset = buffer.batch_metadata[0].base_offset;
            } else if buffer.batch_metadata.is_empty() {
                // If buffer is now empty, set base_offset to continue from where we left off
                buffer.base_offset = up_to_offset + 1;
            }

            // Always update flushed_offset to track progress
            buffer.flushed_offset = up_to_offset;

            tracing::info!("FLUSH→COMPLETE: Removed {} flushed batches (up to offset {}) from buffer for {}-{}, {} batches remain",
                removed_count, up_to_offset, topic, partition, buffer.raw_batches.len());
        }

        Ok(())
    }
    
    /// Encode records in Kafka RecordBatch format
    fn encode_kafka_records(
        &self,
        records: &[chronik_storage::Record],
        _leader_epoch: i32,
    ) -> Result<Vec<u8>> {
        use bytes::Bytes;
        
        // For empty record sets, return an empty vector
        // The protocol handler will write this as a 0-length bytes field
        if records.is_empty() {
            return Ok(vec![]);
        }
        
        // Group records into batches (simple approach: one batch for all)
        let base_offset = records[0].offset;
        let base_timestamp = records[0].timestamp;
        
        let mut batch = KafkaRecordBatch::new(
            base_offset,
            base_timestamp,
            -1, // No producer ID for fetched records
            -1, // No producer epoch
            -1, // No base sequence
            CompressionType::None, // No compression for now
            false, // Not transactional
        );
        
        // Add all records to the batch
        for record in records {
            let headers: Vec<KafkaRecordHeader> = record.headers.iter()
                .map(|(k, v)| KafkaRecordHeader {
                    key: k.clone(),
                    value: Some(Bytes::from(v.clone())),
                })
                .collect();
            
            batch.add_record(
                record.key.as_ref().map(|k| Bytes::from(k.clone())),
                Some(Bytes::from(record.value.clone())),
                headers,
                record.timestamp,
            );
        }
        
        // Encode the batch
        let encoded = batch.encode()?;
        Ok(encoded.to_vec())
    }
}

#[cfg(test)]
#[path = "fetch_handler_test.rs"]
mod fetch_handler_test;