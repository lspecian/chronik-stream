//! Fetch request handler for serving data to Kafka consumers.

use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition};
use chronik_storage::{SegmentReader, RecordBatch};
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
    state: Arc<RwLock<FetchState>>,
}

impl FetchHandler {
    /// Create a new fetch handler
    pub fn new(
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
    ) -> Self {
        Self {
            segment_reader,
            metadata_store,
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
        
        // Calculate high watermark from segments
        let high_watermark = segments.iter()
            .map(|s| s.end_offset + 1)
            .max()
            .unwrap_or(0);
        
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
                        let records_bytes = if records.is_empty() {
                            vec![]
                        } else {
                            self.encode_kafka_records(&records, 0)?
                        };
                        
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
                        // Timeout or error - return empty response
                        return Ok(FetchResponsePartition {
                            partition,
                            error_code: 0,
                            high_watermark,
                            last_stable_offset: high_watermark,
                            log_start_offset,
                            aborted: None,
                            preferred_read_replica: -1,
                            records: vec![],
                        });
                    }
                }
            } else {
                // No wait requested, return empty immediately
                return Ok(FetchResponsePartition {
                    partition,
                    error_code: 0,
                    high_watermark,
                    last_stable_offset: high_watermark,
                    log_start_offset,
                    aborted: None,
                    preferred_read_replica: -1,
                    records: vec![],
                });
            }
        }
        
        // Data is available, fetch with timeout
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
            Ok(Ok(recs)) => recs,
            Ok(Err(e)) => {
                debug!("Error fetching records: {:?}", e);
                vec![]
            }
            Err(_) => {
                debug!("Fetch timeout after {}ms", max_wait_ms);
                vec![]
            }
        };
        
        // Check if we have enough bytes (partial response support)
        let total_bytes: usize = records.iter()
            .map(|r| r.value.len() + r.key.as_ref().map(|k| k.len()).unwrap_or(0))
            .sum();
        
        // If min_bytes is specified and we don't have enough, wait or return partial
        let final_records = if min_bytes > 0 && total_bytes < min_bytes as usize && max_wait_ms > 0 {
            // Try to fetch more data within the timeout
            // For now, just return what we have (partial response)
            records
        } else {
            records
        };
        
        // Encode records in Kafka format
        let records_bytes = if final_records.is_empty() {
            vec![]
        } else {
            self.encode_kafka_records(&final_records, 0)? // TODO: Get actual leader epoch
        };
        
        Ok(FetchResponsePartition {
            partition,
            error_code: 0,
            high_watermark,
            last_stable_offset: high_watermark,
            log_start_offset,
            aborted: None,
            preferred_read_replica: -1,
            records: records_bytes,
        })
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
                    
                    if current_offset >= high_watermark || bytes_fetched >= max_bytes as usize {
                        return Ok(records);
                    }
                }
            }
        }
        
        // If we need more data, fetch from segments
        if current_offset < high_watermark {
            let segments = self.get_segments_for_range(topic, partition, current_offset, high_watermark).await?;
            
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
                    records.push(record.clone());
                    current_offset = record.offset + 1;
                    bytes_fetched += record.value.len() + 
                        record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;
                }
            }
        }
        
        debug!(
            "Fetched {} records from {}-{} starting at offset {}", 
            records.len(), topic, partition, fetch_offset
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
        
        let segment_infos: Vec<_> = segments.into_iter()
            .filter(|s| s.end_offset >= start_offset && s.start_offset < end_offset)
            .map(|s| SegmentInfo {
                segment_id: s.segment_id,
                base_offset: s.start_offset,
                last_offset: s.end_offset,
                object_key: s.path,
            })
            .collect();
        
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
        // Use segment reader to fetch data
        let fetch_result = self.segment_reader.fetch_from_segment(
            &segment_info.object_key,
            start_offset,
            end_offset,
            max_bytes,
        ).await?;
        
        Ok(fetch_result.records)
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
        
        let mut state = self.state.write().await;
        let key = (topic.to_string(), partition);
        
        // Create or update buffer
        let buffer = state.buffers.entry(key).or_insert(PartitionBuffer {
            records: Vec::new(),
            base_offset,
            high_watermark,
        });
        
        // Add new records
        buffer.records.extend(records);
        buffer.high_watermark = high_watermark;
        
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