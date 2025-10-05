//! Fetch request handler for serving data to Kafka consumers.

use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition};
use chronik_storage::{SegmentReader, RecordBatch, Record, Segment, ObjectStoreTrait};
use chronik_storage::kafka_records::{KafkaRecordBatch, KafkaRecord, RecordHeader as KafkaRecordHeader, CompressionType};
use chronik_wal::{WalManager, WalRecord};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// In-memory buffer for recent records
#[derive(Debug)]
struct PartitionBuffer {
    records: Vec<chronik_storage::Record>,
    base_offset: i64,
    high_watermark: i64,
    /// Highest offset that has been flushed to segments
    flushed_offset: i64,
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

/// Fetch request handler
pub struct FetchHandler {
    segment_reader: Arc<SegmentReader>,
    metadata_store: Arc<dyn MetadataStore>,
    object_store: Arc<dyn ObjectStoreTrait>,
    wal_manager: Option<Arc<RwLock<WalManager>>>,
    state: Arc<RwLock<FetchState>>,
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
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
                segment_cache: HashMap::new(),
            })),
        }
    }

    /// Create a new fetch handler with WAL integration
    pub fn new_with_wal(
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        wal_manager: Arc<RwLock<WalManager>>,
    ) -> Self {
        Self {
            segment_reader,
            metadata_store,
            object_store,
            wal_manager: Some(wal_manager),
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
                segment_cache: HashMap::new(),
            })),
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
        tracing::info!(
            "fetch_partition called - topic: {}, partition: {}, fetch_offset: {}, max_bytes: {}",
            topic, partition, fetch_offset, max_bytes
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
        
        // Get partition segments to determine watermarks
        let segments = self.metadata_store.list_segments(topic, Some(partition as u32)).await?;
        
        tracing::info!("Found {} segments for {}-{}", segments.len(), topic, partition);
        for seg in &segments {
            tracing::info!("  Segment: {} offsets {}-{} path: {}", 
                seg.segment_id, seg.start_offset, seg.end_offset, seg.path);
        }
        
        // Calculate high watermark from segments
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
        
        tracing::info!(
            "High watermark for {}-{}: segment={}, buffer={}, final={}",
            topic, partition, segment_high_watermark, buffer_high_watermark, high_watermark
        );
        
        let log_start_offset = segments.iter()
            .map(|s| s.start_offset)
            .min()
            .unwrap_or(0);
        
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
            tracing::info!(
                "Data available for fetch - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
                topic, partition, fetch_offset, high_watermark
            );
            
            // Data is available, fetch it
            let fetch_timeout = if max_wait_ms > 0 {
                Duration::from_millis(max_wait_ms as u64)
            } else {
                Duration::from_secs(30) // Default timeout
            };
            
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
            
            // Encode the records - always use encode_kafka_records to get proper format
            let records_bytes = self.encode_kafka_records(&records, 0)?;
            
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
        // NOTE: Buffer contains ONLY unflushed records. We need to also check WAL/segments.
        let buffer_highest_offset = {
            let state = self.state.read().await;
            if let Some(buffer) = state.buffers.get(&(topic.to_string(), partition)) {
                info!(
                    "FETCH→BUFFER: Checking buffer for {}-{}, buffer has {} records",
                    topic, partition, buffer.records.len()
                );

                let mut buffer_max_offset = -1i64;
                for record in &buffer.records {
                    if record.offset >= fetch_offset && record.offset < high_watermark {
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

                if !records.is_empty() {
                    info!(
                        "FETCH→BUFFER: Fetched {} records from buffer for {}-{}, highest offset: {}",
                        records.len(), topic, partition, buffer_max_offset
                    );
                }
                buffer_max_offset
            } else {
                -1i64
            }
        };

        // If we got records from buffer, check if we need more from WAL/segments
        if !records.is_empty() && bytes_fetched >= max_bytes as usize {
            // We have enough data from buffer alone
            return Ok(records);
        }

        // If buffer only had partial data (or was empty), we need to check if there are
        // earlier records in WAL/segments that we missed
        let need_earlier_records = records.is_empty() ||
            (buffer_highest_offset >= 0 && fetch_offset < buffer_highest_offset);
        
        // PHASE 2: Try WAL for any missing records or to continue the fetch
        // WAL should have flushed records that are no longer in buffer
        if let Some(wal_manager) = &self.wal_manager {
            // Only try WAL if we don't already have all needed records from buffer
            if records.is_empty() || need_earlier_records {
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
        
        // PHASE 3: Fall back to segment-based storage if we still need more records
        // This ensures we fetch from segments if WAL/buffer didn't have all the data
        if records.is_empty() || need_earlier_records {
            info!(
                "FETCH→SEGMENTS: Fetching from segments for {}-{} (have {} records so far)",
                topic, partition, records.len()
            );

            let segment_records = self.fetch_records_legacy(
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes
            ).await?;

            // Merge segment records with existing records (from buffer/WAL)
            for seg_rec in segment_records {
                if !records.iter().any(|r| r.offset == seg_rec.offset) {
                    records.push(seg_rec);
                }
            }

            // Sort by offset to maintain order
            records.sort_by_key(|r| r.offset);
        }

        Ok(records)
    }
    
    /// Fetch records from WAL manager
    async fn fetch_from_wal(
        &self,
        wal_manager: &Arc<RwLock<WalManager>>,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        let max_records = std::cmp::max(1, max_bytes as usize / 100); // Estimate ~100 bytes per record
        
        let wal_records = {
            let manager = wal_manager.read().await;
            manager.read_from(topic, partition, fetch_offset, max_records).await
                .map_err(|e| Error::Internal(format!("WAL read failed: {}", e)))?
        };
        
        // Convert WalRecord to chronik_storage::Record
        let mut records = Vec::new();
        for wal_record in wal_records {
            let storage_record = chronik_storage::Record {
                offset: wal_record.offset,
                timestamp: wal_record.timestamp,
                key: wal_record.key,
                value: wal_record.value,
                headers: wal_record.headers.into_iter().collect(),
            };
            records.push(storage_record);
        }
        
        info!("WAL returned {} records starting from offset {} for {}-{}", 
            records.len(), fetch_offset, topic, partition);
        
        Ok(records)
    }
    
    /// Legacy method for fetching from segments and buffers (fallback)
    async fn fetch_records_legacy(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        info!(
            "fetch_records_legacy called - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
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
        if current_offset < high_watermark && bytes_fetched < max_bytes as usize {
            let state = self.state.read().await;
            if let Some(buffer) = state.buffers.get(&(topic.to_string(), partition)) {
                tracing::warn!("FETCH→BUFFER: Checking buffer for {}-{}, buffer has {} records, current_offset={}, max_segment_offset={}, high_watermark={}", 
                    topic, partition, buffer.records.len(), current_offset, max_segment_offset, high_watermark);
                
                // Only fetch from buffer for offsets that are NOT in segments
                for record in &buffer.records {
                    // Only include records that are:
                    // 1. At or after current_offset (which is now > max_segment_offset)
                    // 2. Before high_watermark
                    // 3. NOT already in segments (offset > max_segment_offset)
                    if record.offset >= current_offset && 
                       record.offset < high_watermark &&
                       record.offset > max_segment_offset {
                        let record_size = record.value.len() + 
                            record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;
                        
                        if bytes_fetched + record_size > max_bytes as usize && !records.is_empty() {
                            break;
                        }
                        
                        tracing::debug!(
                            "FETCH from buffer: partition={} offset={} value_len={}",
                            partition, record.offset, record.value.len()
                        );
                        
                        records.push(record.clone());
                        current_offset = record.offset + 1;
                        bytes_fetched += record_size;
                    } else {
                        tracing::warn!("FETCH→SKIP: Skipping record offset={} (current={}, max_seg={}, hw={})", 
                            record.offset, current_offset, max_segment_offset, high_watermark);
                    }
                }
            } else {
                tracing::warn!("FETCH→NO_BUFFER: No buffer found for {}-{}", topic, partition);
            }
        }
        
        tracing::info!(
            "fetch_records complete - fetched {} records from {}-{} starting at offset {} (current_offset: {})", 
            records.len(), topic, partition, fetch_offset, current_offset
        );
        
        Ok(records)
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
        
        // CRITICAL FIX: Decode ALL batches from segment, not just the first one
        // A segment can contain multiple concatenated RecordBatch structures
        let mut all_records = Vec::new();
        let mut batch_count = 0;

        if !segment.indexed_records.is_empty() {
            // Decode ALL batches from indexed_records
            use std::io::Cursor;
            let mut cursor = Cursor::new(&segment.indexed_records[..]);
            let total_len = segment.indexed_records.len();

            tracing::warn!("SEGMENT→DECODE: Starting to decode batches from indexed_records ({} bytes)", total_len);

            while (cursor.position() as usize) < total_len {
                let position_before = cursor.position();

                // Try to decode a batch
                match RecordBatch::decode(&segment.indexed_records[(cursor.position() as usize)..]) {
                    Ok(batch) => {
                        let batch_records = batch.records.len();
                        tracing::warn!(
                            "SEGMENT→BATCH {}: Decoded {} records from position {}",
                            batch_count + 1,
                            batch_records,
                            position_before
                        );

                        all_records.extend(batch.records);
                        batch_count += 1;

                        // Advance cursor by the size of this batch
                        // RecordBatch format: 4 bytes count + records
                        // We need to calculate how many bytes were consumed
                        let bytes_consumed = 4 + all_records.iter()
                            .skip(all_records.len() - batch_records)
                            .map(|r| {
                                16 + // offset (8) + timestamp (8)
                                4 + r.key.as_ref().map(|k| k.len()).unwrap_or(0) + // key_len (4) + key
                                4 + r.value.len() + // value_len (4) + value
                                4 + // header_count (4)
                                r.headers.iter().map(|(k, v)| 4 + k.len() + 4 + v.len()).sum::<usize>()
                            })
                            .sum::<usize>();

                        cursor.set_position(position_before + bytes_consumed as u64);

                        // Safety check: if we didn't advance, break to avoid infinite loop
                        if cursor.position() == position_before {
                            tracing::error!("SEGMENT→ERROR: Cursor didn't advance, breaking to avoid infinite loop");
                            break;
                        }
                    }
                    Err(e) => {
                        // If we can't decode more batches, we're done
                        tracing::warn!(
                            "SEGMENT→DECODE: Finished decoding at position {} ({} bytes remaining): {}",
                            cursor.position(),
                            total_len - cursor.position() as usize,
                            e
                        );
                        break;
                    }
                }
            }

            tracing::warn!(
                "SEGMENT→COMPLETE: Decoded {} batches with {} total records from indexed_records",
                batch_count,
                all_records.len()
            );
        } else if !segment.raw_kafka_batches.is_empty() {
            // Decode ALL batches from raw Kafka batches (preserves CRC)
            tracing::info!("Using raw Kafka batches for fetch (no indexed records)");

            use std::io::Cursor;
            let mut cursor = Cursor::new(&segment.raw_kafka_batches[..]);
            let total_len = segment.raw_kafka_batches.len();

            while (cursor.position() as usize) < total_len {
                match KafkaRecordBatch::decode(&segment.raw_kafka_batches[(cursor.position() as usize)..]) {
                    Ok(kafka_batch) => {
                        // Convert Kafka records to storage Records
                        let records: Vec<Record> = kafka_batch.records.into_iter().enumerate().map(|(i, kr)| {
                            Record {
                                offset: kafka_batch.header.base_offset + i as i64,
                                timestamp: kafka_batch.header.base_timestamp + kr.timestamp_delta,
                                key: kr.key.map(|k| k.to_vec()),
                                value: kr.value.map(|v| v.to_vec()).unwrap_or_default(),
                                headers: kr.headers.into_iter().map(|h| {
                                    (h.key, h.value.map(|v| v.to_vec()).unwrap_or_default())
                                }).collect(),
                            }
                        }).collect();

                        let batch_records = records.len();
                        all_records.extend(records);
                        batch_count += 1;

                        // Advance cursor (Kafka batch has known size)
                        // This needs proper implementation based on Kafka batch format
                        // For now, break after first batch to avoid infinite loop
                        tracing::warn!("SEGMENT→KAFKA: Decoded batch {} with {} records (FIXME: multi-batch not implemented for raw Kafka)", batch_count, batch_records);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("SEGMENT→KAFKA: Finished decoding raw batches: {}", e);
                        break;
                    }
                }
            }
        } else {
            tracing::warn!("SEGMENT→EMPTY: No indexed_records or raw_kafka_batches to decode");
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
    
    /// Update in-memory buffer with new records  
    /// This replaces the buffer with only the new unflushed records
    pub async fn update_buffer(
        &self,
        topic: &str,
        partition: i32,
        records: Vec<chronik_storage::Record>,
        high_watermark: i64,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        
        let base_offset = records.first().unwrap().offset;
        let last_offset = records.last().unwrap().offset;
        
        // Log the offset range of records being added
        tracing::warn!("BUFFER→UPDATE: Called for {}-{}, num_records={}, high_watermark={}, offset_range=[{}-{}]", 
            topic, partition, records.len(), high_watermark, base_offset, last_offset);
        
        let mut state = self.state.write().await;
        let key = (topic.to_string(), partition);
        
        // Create or get buffer - we'll REPLACE its contents, not append
        let buffer = state.buffers.entry(key.clone()).or_insert(PartitionBuffer {
            records: Vec::new(),
            base_offset,
            high_watermark,
            flushed_offset: -1,  // Nothing flushed initially
        });
        
        // CRITICAL FIX: Use a HashSet to track existing offsets for proper deduplication
        // This handles non-sequential offsets from multiple partitions correctly
        use std::collections::HashSet;
        
        tracing::warn!("BUFFER→STATE: Buffer for {}-{} currently has {} records before update", 
            topic, partition, buffer.records.len());
        
        let existing_offsets: HashSet<i64> = buffer.records.iter()
            .map(|r| r.offset)
            .collect();
        
        // Only add records that don't already exist in the buffer
        let mut added_count = 0;
        let records_len = records.len();
        for record in records {
            if !existing_offsets.contains(&record.offset) {
                tracing::warn!("BUFFER→ADD: Adding record offset={} to {}-{} buffer", 
                    record.offset, topic, partition);
                buffer.records.push(record);
                added_count += 1;
            } else {
                tracing::warn!("BUFFER→SKIP: Skipping duplicate offset={} for {}-{} (already in buffer)", 
                    record.offset, topic, partition);
            }
        }
        
        if added_count > 0 {
            tracing::debug!("Added {} new records to buffer (deduplicated)", added_count);
            
            // Sort records by offset to maintain order
            buffer.records.sort_by_key(|r| r.offset);
            
            // Update base_offset to the lowest offset in buffer
            if let Some(first) = buffer.records.first() {
                buffer.base_offset = first.offset;
            }
        }
        
        // Always update the high watermark to the latest
        buffer.high_watermark = high_watermark;
        
        tracing::debug!("Buffer updated: now contains {} records total", buffer.records.len());
        
        tracing::info!("Buffer updated: key={:?}, total_records={}, high_watermark={}", 
            key, buffer.records.len(), buffer.high_watermark);
        
        // Trim old records if buffer is too large (keep last 1000 records)
        if buffer.records.len() > 1000 {
            let trim_count = buffer.records.len() - 1000;
            buffer.records.drain(0..trim_count);
            if !buffer.records.is_empty() {
                buffer.base_offset = buffer.records[0].offset;
            }
        }
        
        Ok(())
    }
    
    /// Clear buffers for a topic
    pub async fn clear_topic_buffers(&self, topic: &str) -> Result<()> {
        let mut state = self.state.write().await;
        state.buffers.retain(|(t, _), _| t != topic);
        state.segment_cache.retain(|(t, _), _| t != topic);
        Ok(())
    }
    
    /// Mark records as flushed to segment (removes only flushed records from buffer)
    /// CRITICAL FIX: Only remove records that were actually flushed, not all records
    pub async fn mark_flushed(
        &self,
        topic: &str,
        partition: i32,
        up_to_offset: i64,
    ) -> Result<()> {
        tracing::warn!("FLUSH→MARK: Marking records as flushed for {}-{}, up_to_offset={}",
            topic, partition, up_to_offset);
        
        let mut state = self.state.write().await;
        let key = (topic.to_string(), partition);
        
        if let Some(buffer) = state.buffers.get_mut(&key) {
            let initial_count = buffer.records.len();
            
            tracing::warn!("FLUSH→BEFORE: Buffer for {}-{} has {} records before flush",
                topic, partition, initial_count);
            
            // Log which records will be removed
            for record in &buffer.records {
                if record.offset <= up_to_offset {
                    tracing::warn!("FLUSH→REMOVE: Will remove record offset={} from {}-{} (offset <= {})",
                        record.offset, topic, partition, up_to_offset);
                }
            }
            
            // CRITICAL FIX: Only remove records with offset <= up_to_offset
            // Keep all records with offset > up_to_offset (they haven't been flushed yet)
            let before_count = buffer.records.len();
            buffer.records.retain(|record| record.offset > up_to_offset);
            let removed_count = before_count - buffer.records.len();
            
            // Update base_offset if we removed records from the beginning
            if !buffer.records.is_empty() && removed_count > 0 {
                buffer.base_offset = buffer.records[0].offset;
            } else if buffer.records.is_empty() {
                // If buffer is now empty, set base_offset to continue from where we left off
                buffer.base_offset = up_to_offset + 1;
            }
            
            // Always update flushed_offset to track progress
            buffer.flushed_offset = up_to_offset;
            
            tracing::warn!("FLUSH→COMPLETE: Removed {} flushed records (up to offset {}) from buffer for {}-{}, {} records remain",
                removed_count, up_to_offset, topic, partition, buffer.records.len());
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