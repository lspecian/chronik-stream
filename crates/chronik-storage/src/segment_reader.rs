//! Segment reader for fetching data from storage.

use crate::{ObjectStoreTrait, Segment, Record};
use chronik_common::{Result, types::SegmentId};
use std::sync::Arc;
use std::collections::HashMap;
use bytes::Buf;

/// Segment reader configuration
#[derive(Debug, Clone)]
pub struct SegmentReaderConfig {
    /// Cache size in bytes
    pub cache_size: u64,
    /// Read buffer size
    pub buffer_size: usize,
}

impl Default for SegmentReaderConfig {
    fn default() -> Self {
        Self {
            cache_size: 1024 * 1024 * 1024, // 1GB
            buffer_size: 65536,
        }
    }
}

/// Segment reader for fetching records
pub struct SegmentReader {
    _config: SegmentReaderConfig,
    object_store: Arc<dyn ObjectStoreTrait>,
    // TODO: Add caching layer
}

impl SegmentReader {
    /// Create a new segment reader
    pub fn new(config: SegmentReaderConfig, object_store: Arc<dyn ObjectStoreTrait>) -> Self {
        Self {
            _config: config,
            object_store,
        }
    }
    
    /// Fetch records from a topic partition
    pub async fn fetch(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResult> {
        // List segments for this partition
        let prefix = format!("segments/{}/{}/", topic, partition);
        let segment_keys = self.object_store.list(&prefix).await?;
        
        // Find segments that contain the requested offset
        let mut relevant_segments = Vec::new();
        for metadata in segment_keys {
            if let Some(_segment_id) = extract_segment_id(&metadata.key) {
                // Get segment metadata
                let segment_data = self.object_store.get(&metadata.key).await?;
                let segment = Segment::deserialize(segment_data)?;
                
                // Check if this segment contains our offset range
                if segment.metadata.base_offset <= fetch_offset && 
                   segment.metadata.last_offset >= fetch_offset {
                    relevant_segments.push(segment);
                }
            }
        }
        
        // Sort by base offset
        relevant_segments.sort_by_key(|s| s.metadata.base_offset);
        
        // Read records
        let mut records = Vec::new();
        let mut bytes_read = 0;
        let mut high_watermark = fetch_offset;
        
        for segment in relevant_segments {
            let batch_records = self.read_records_from_segment(
                &segment,
                fetch_offset,
                max_bytes - bytes_read as i32,
            )?;
            
            for record in batch_records {
                if record.offset >= fetch_offset && bytes_read < max_bytes as usize {
                    let record_size = record.size_bytes();
                    if bytes_read + record_size <= max_bytes as usize {
                        bytes_read += record_size;
                        high_watermark = high_watermark.max(record.offset + 1);
                        records.push(record);
                    } else {
                        break;
                    }
                }
            }
            
            if bytes_read >= max_bytes as usize {
                break;
            }
        }
        
        Ok(FetchResult {
            records,
            high_watermark,
            log_start_offset: 0, // TODO: Track actual log start offset
            error_code: 0,
        })
    }
    
    /// Fetch records from a specific segment by object key
    pub async fn fetch_from_segment(
        &self,
        object_key: &str,
        start_offset: i64,
        end_offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResult> {
        // Download segment from object storage
        let segment_data = self.object_store.get(object_key).await?;
        
        // Parse segment using Chronik format
        let mut cursor = std::io::Cursor::new(segment_data.as_ref());
        let chronik_segment = crate::chronik_segment::ChronikSegment::read_from(&mut cursor)?;
        
        // Filter records by offset range
        let mut records = Vec::new();
        let mut bytes_fetched = 0;
        
        for batch in chronik_segment.kafka_data() {
            for record in &batch.records {
                if record.offset >= start_offset && record.offset < end_offset {
                    let record_size = record.size_bytes();
                    
                    if bytes_fetched + record_size > max_bytes as usize && !records.is_empty() {
                        break;
                    }
                    
                    records.push(record.clone());
                    bytes_fetched += record_size;
                }
            }
            
            if bytes_fetched >= max_bytes as usize {
                break;
            }
        }
        
        Ok(FetchResult {
            records,
            high_watermark: chronik_segment.metadata().last_offset + 1,
            log_start_offset: chronik_segment.metadata().base_offset,
            error_code: 0,
        })
    }
    
    /// Read records from a segment
    fn read_records_from_segment(
        &self,
        segment: &Segment,
        start_offset: i64,
        max_bytes: i32,
    ) -> Result<Vec<Record>> {
        // Decode indexed records from segment  
        let mut records = Vec::new();
        let mut buf = std::io::Cursor::new(&segment.indexed_records[..]);
        
        // Simple decoding - read record count first
        if buf.remaining() < 4 {
            return Ok(records);
        }
        
        let record_count = buf.get_u32() as usize;
        let mut bytes_read = 0;
        
        for _ in 0..record_count {
            if buf.remaining() < 16 || bytes_read >= max_bytes as usize {
                break;
            }
            
            // Read record fields
            let offset = buf.get_i64();
            let timestamp = buf.get_i64();
            
            // Skip if before start offset
            if offset < start_offset {
                // Skip this record
                let key_len = buf.get_u32();
                if key_len > 0 {
                    buf.advance(key_len as usize);
                }
                let value_len = buf.get_u32();
                buf.advance(value_len as usize);
                let headers_count = buf.get_u32();
                for _ in 0..headers_count {
                    let key_len = buf.get_u32();
                    buf.advance(key_len as usize);
                    let value_len = buf.get_u32();
                    buf.advance(value_len as usize);
                }
                continue;
            }
            
            // Read key
            let key_len = buf.get_u32();
            let key = if key_len > 0 {
                let mut key_bytes = vec![0u8; key_len as usize];
                buf.copy_to_slice(&mut key_bytes);
                Some(key_bytes)
            } else {
                None
            };
            
            // Read value
            let value_len = buf.get_u32();
            let mut value = vec![0u8; value_len as usize];
            buf.copy_to_slice(&mut value);
            
            // Read headers
            let headers_count = buf.get_u32();
            let mut headers = HashMap::new();
            
            for _ in 0..headers_count {
                let key_len = buf.get_u32();
                let mut header_key = vec![0u8; key_len as usize];
                buf.copy_to_slice(&mut header_key);
                let header_key_str = String::from_utf8_lossy(&header_key).to_string();
                
                let value_len = buf.get_u32();
                let mut header_value = vec![0u8; value_len as usize];
                buf.copy_to_slice(&mut header_value);
                
                headers.insert(header_key_str, header_value);
            }
            
            let record = Record {
                offset,
                timestamp,
                key,
                value,
                headers,
            };
            
            bytes_read += record.size_bytes();
            records.push(record);
        }
        
        Ok(records)
    }
}

/// Result of a fetch operation
pub struct FetchResult {
    pub records: Vec<Record>,
    pub high_watermark: i64,
    pub log_start_offset: i64,
    pub error_code: i16,
}

/// Extract segment ID from object key
fn extract_segment_id(key: &str) -> Option<SegmentId> {
    // Parse segment ID from key like "segments/topic/partition/uuid.segment"
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() >= 4 {
        let filename = parts.last()?;
        let id_str = filename.strip_suffix(".segment")?;
        uuid::Uuid::parse_str(id_str).ok().map(SegmentId)
    } else {
        None
    }
}

