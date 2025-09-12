//! Fetch request handler for serving data to Kafka consumers.

use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition};
use chronik_storage::{SegmentReader, RecordBatch, Record, Segment, ObjectStoreTrait};
use chronik_storage::kafka_records::{KafkaRecordBatch, KafkaRecord, RecordHeader as KafkaRecordHeader, CompressionType};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, error, info};

/// In-memory buffer for recent records
#[derive(Debug)]
struct PartitionBuffer {
    records: Vec<chronik_storage::Record>,
    base_offset: i64,
    high_watermark: i64,
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
    
    /// Fetch records from memory or segments
    async fn fetch_records(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        tracing::info!(
            "fetch_records called - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
            topic, partition, fetch_offset, high_watermark
        );
        
        let mut records = Vec::new();
        let mut current_offset = fetch_offset;
        let mut bytes_fetched = 0;
        
        // First check in-memory buffer
        {
            let state = self.state.read().await;
            if let Some(buffer) = state.buffers.get(&(topic.to_string(), partition)) {
                if fetch_offset >= buffer.base_offset && fetch_offset < buffer.high_watermark {
                    // Fetch from buffer
                    for record in &buffer.records {
                        if record.offset >= fetch_offset && record.offset < high_watermark {
                            let record_size = record.value.len() + 
                                record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;
                            
                            if bytes_fetched + record_size > max_bytes as usize && !records.is_empty() {
                                break;
                            }
                            
                            records.push(record.clone());
                            current_offset = record.offset + 1;
                            bytes_fetched += record_size;
                        }
                    }
                    
                    // Don't return early - we may need to fetch more from segments
                    // Only return early if we've reached the high watermark
                    if current_offset >= high_watermark {
                        tracing::info!(
                            "Reached high watermark from buffer - returning {} records",
                            records.len()
                        );
                        return Ok(records);
                    }
                    // Continue to check segments even if we hit max_bytes in buffer
                }
            }
        }
        
        // If we need more data, fetch from segments
        if current_offset < high_watermark && bytes_fetched < max_bytes as usize {
            tracing::info!(
                "Fetching from segments - current_offset: {}, high_watermark: {}, bytes_fetched: {}",
                current_offset, high_watermark, bytes_fetched
            );
            
            let segments = self.get_segments_for_range(topic, partition, current_offset, high_watermark).await?;
            
            tracing::info!(
                "Found {} segments for range {}-{}", 
                segments.len(), current_offset, high_watermark
            );
            
            for segment_info in segments {
                if bytes_fetched >= max_bytes as usize && !records.is_empty() {
                    break;
                }
                
                // Skip segments before our offset
                if segment_info.last_offset < current_offset {
                    continue;
                }
                
                // Fetch segment data
                let segment_records = self.fetch_from_segment(
                    &segment_info,
                    current_offset,
                    high_watermark,
                    max_bytes - bytes_fetched as i32,
                ).await?;
                
                for record in segment_records {
                    // Log what we're fetching for debugging
                    tracing::info!(
                        "FETCH: partition={} offset={} value_len={} value_preview={}",
                        partition,
                        record.offset,
                        record.value.len(),
                        String::from_utf8_lossy(&record.value[..std::cmp::min(50, record.value.len())])
                    );
                    
                    records.push(record.clone());
                    current_offset = record.offset + 1;
                    bytes_fetched += record.value.len() + 
                        record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;
                }
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
        let segments = self.metadata_store.list_segments(topic, Some(partition as u32)).await?;
        
        tracing::info!(
            "Retrieved {} total segments from metadata store for {}-{}",
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
        
        // Read segment from storage using the correct Segment format (CHRN magic bytes)
        let segment_data = self.object_store.get(&segment_info.object_key).await?;
        
        // Parse using the Segment format - this is what SegmentWriter creates
        let segment = Segment::deserialize(segment_data)?;
        
        tracing::info!("Successfully parsed segment v{} with {} records, raw_kafka: {} bytes, indexed: {} bytes", 
            segment.header.version,
            segment.metadata.record_count, 
            segment.raw_kafka_batches.len(),
            segment.indexed_records.len());
        
        // Use indexed records if available, otherwise decode from raw Kafka batches
        let record_batch = if !segment.indexed_records.is_empty() {
            // Decode the RecordBatch from indexed_records
            RecordBatch::decode(&segment.indexed_records)?
        } else if !segment.raw_kafka_batches.is_empty() {
            // Decode from raw Kafka batches (preserves CRC)
            tracing::info!("Using raw Kafka batches for fetch (no indexed records)");
            
            // Parse the raw Kafka batch to extract records
            let kafka_batch = KafkaRecordBatch::decode(&segment.raw_kafka_batches)?;
            
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
            
            RecordBatch { records }
        } else {
            return Ok(vec![]);
        };
        
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
    
    /// Update in-memory buffer with new records
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
        
        tracing::info!("update_buffer called: topic={}, partition={}, num_records={}, high_watermark={}, base_offset={}", 
            topic, partition, records.len(), high_watermark, base_offset);
        
        let mut state = self.state.write().await;
        let key = (topic.to_string(), partition);
        
        // Create or update buffer
        let buffer = state.buffers.entry(key.clone()).or_insert(PartitionBuffer {
            records: Vec::new(),
            base_offset,
            high_watermark,
        });
        
        // Add new records
        buffer.records.extend(records);
        buffer.high_watermark = high_watermark;
        
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