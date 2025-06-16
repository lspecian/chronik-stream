//! Integration between object storage and ChronikSegment format.

use bytes::Bytes;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, instrument};

use crate::{
    object_store::{
        errors::{ObjectStoreError, ObjectStoreResult},
        storage::{ObjectStore, PutOptions},
    },
    chronik_segment::{ChronikSegment, SegmentMetadata},
};

/// Adapter for storing ChronikSegments in object storage
pub struct ChronikStorageAdapter {
    store: Box<dyn ObjectStore>,
    cache: Option<Arc<RwLock<SegmentCache>>>,
}

impl ChronikStorageAdapter {
    /// Create a new ChronikStorageAdapter
    pub fn new(store: Box<dyn ObjectStore>) -> Self {
        Self {
            store,
            cache: None,
        }
    }

    /// Create a new ChronikStorageAdapter with caching
    pub fn with_cache(store: Box<dyn ObjectStore>, cache_size: usize) -> Self {
        Self {
            store,
            cache: Some(Arc::new(RwLock::new(SegmentCache::new(cache_size)))),
        }
    }

    /// Store a ChronikSegment
    #[instrument(skip(self, segment))]
    pub async fn store_segment(
        &self,
        location: &SegmentLocation,
        mut segment: ChronikSegment,
    ) -> ObjectStoreResult<()> {
        let key = location.to_storage_key();
        
        // Serialize segment to bytes
        let mut buffer = Cursor::new(Vec::new());
        segment
            .write_to(&mut buffer)
            .map_err(|e| ObjectStoreError::SerializationError {
                message: e.to_string(),
            })?;

        let data = Bytes::from(buffer.into_inner());
        
        // Store in object storage
        let options = PutOptions {
            content_type: Some("application/octet-stream".to_string()),
            metadata: Some(self.build_metadata(segment.metadata())),
            ..Default::default()
        };

        self.store.put_with_options(&key, data.clone(), options).await?;

        // Update cache if enabled
        if let Some(cache) = &self.cache {
            let mut cache_guard = cache.write().await;
            cache_guard.put(location.clone(), data.clone());
        }

        info!(
            "Stored segment: topic={}, partition={}, base_offset={}, size={}",
            segment.metadata().topic,
            segment.metadata().partition_id,
            segment.metadata().base_offset,
            data.len()
        );

        Ok(())
    }

    /// Load a ChronikSegment
    #[instrument(skip(self))]
    pub async fn load_segment(&self, location: &SegmentLocation) -> ObjectStoreResult<ChronikSegment> {
        // Check cache first
        if let Some(cache) = &self.cache {
            let mut cache_guard = cache.write().await;
            if let Some(data) = cache_guard.get(location) {
                debug!("Cache hit for segment: {}", location.to_storage_key());
                return self.deserialize_segment(data);
            }
        }

        let key = location.to_storage_key();
        let data = self.store.get(&key).await?;

        // Update cache
        if let Some(cache) = &self.cache {
            let mut cache_guard = cache.write().await;
            cache_guard.put(location.clone(), data.clone());
        }

        debug!("Loaded segment from storage: {}", key);
        self.deserialize_segment(&data)
    }

    /// Load only segment metadata (header + metadata section)
    #[instrument(skip(self))]
    pub async fn load_segment_metadata(&self, location: &SegmentLocation) -> ObjectStoreResult<SegmentMetadata> {
        let key = location.to_storage_key();
        
        // Read first 1KB which should contain header + metadata for most segments
        let header_data = self.store.get_range(&key, 0, 1024).await?;
        let mut cursor = Cursor::new(header_data.as_ref());
        
        let (_, metadata) = ChronikSegment::read_metadata(&mut cursor)
            .map_err(|e| ObjectStoreError::SerializationError {
                message: e.to_string(),
            })?;

        debug!("Loaded segment metadata: {}", key);
        Ok(metadata)
    }

    /// Check if a segment exists
    #[instrument(skip(self))]
    pub async fn segment_exists(&self, location: &SegmentLocation) -> ObjectStoreResult<bool> {
        let key = location.to_storage_key();
        self.store.exists(&key).await
    }

    /// Delete a segment
    #[instrument(skip(self))]
    pub async fn delete_segment(&self, location: &SegmentLocation) -> ObjectStoreResult<()> {
        let key = location.to_storage_key();
        
        self.store.delete(&key).await?;

        // Remove from cache
        if let Some(cache) = &self.cache {
            let mut cache_guard = cache.write().await;
            cache_guard.remove(location);
        }

        info!("Deleted segment: {}", key);
        Ok(())
    }

    /// List segments with a given prefix
    #[instrument(skip(self))]
    pub async fn list_segments(&self, topic: &str, partition_id: Option<i32>) -> ObjectStoreResult<Vec<SegmentLocation>> {
        let prefix = if let Some(partition) = partition_id {
            format!("segments/{}/partition-{}/", topic, partition)
        } else {
            format!("segments/{}/", topic)
        };

        let objects = self.store.list(&prefix).await?;
        let mut locations = Vec::new();

        for obj in objects {
            if let Ok(location) = SegmentLocation::from_storage_key(&obj.key) {
                locations.push(location);
            }
        }

        // Sort by base offset
        locations.sort_by_key(|l| l.base_offset);

        debug!("Listed {} segments with prefix: {}", locations.len(), prefix);
        Ok(locations)
    }

    /// Get segment statistics
    #[instrument(skip(self))]
    pub async fn segment_stats(&self, location: &SegmentLocation) -> ObjectStoreResult<SegmentStats> {
        let key = location.to_storage_key();
        let obj_metadata = self.store.head(&key).await?;
        let segment_metadata = self.load_segment_metadata(location).await?;

        Ok(SegmentStats {
            storage_size: obj_metadata.size,
            record_count: segment_metadata.record_count,
            compression_ratio: segment_metadata.compression_ratio,
            last_modified: obj_metadata.last_modified,
        })
    }

    /// Build object metadata from segment metadata
    fn build_metadata(&self, segment_meta: &SegmentMetadata) -> HashMap<String, String> {
        let mut metadata = HashMap::new();
        metadata.insert("chronik-segment-version".to_string(), "1".to_string());
        metadata.insert("topic".to_string(), segment_meta.topic.clone());
        metadata.insert("partition-id".to_string(), segment_meta.partition_id.to_string());
        metadata.insert("base-offset".to_string(), segment_meta.base_offset.to_string());
        metadata.insert("last-offset".to_string(), segment_meta.last_offset.to_string());
        metadata.insert("record-count".to_string(), segment_meta.record_count.to_string());
        metadata.insert("created-at".to_string(), segment_meta.created_at.to_string());
        metadata
    }

    /// Deserialize segment from bytes
    fn deserialize_segment(&self, data: &Bytes) -> ObjectStoreResult<ChronikSegment> {
        let mut cursor = Cursor::new(data.as_ref());
        ChronikSegment::read_from(&mut cursor).map_err(|e| ObjectStoreError::SerializationError {
            message: e.to_string(),
        })
    }
}

/// Location identifier for a segment in storage
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SegmentLocation {
    pub topic: String,
    pub partition_id: i32,
    pub base_offset: i64,
}

impl SegmentLocation {
    /// Create a new segment location
    pub fn new(topic: String, partition_id: i32, base_offset: i64) -> Self {
        Self {
            topic,
            partition_id,
            base_offset,
        }
    }

    /// Convert to storage key
    pub fn to_storage_key(&self) -> String {
        format!(
            "segments/{}/partition-{}/segment-{:020}.chronik",
            self.topic, self.partition_id, self.base_offset
        )
    }

    /// Parse from storage key
    pub fn from_storage_key(key: &str) -> Result<Self, String> {
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 4 || parts[0] != "segments" {
            return Err(format!("Invalid segment key format: {}", key));
        }

        let topic = parts[1].to_string();
        
        let partition_str = parts[2];
        if !partition_str.starts_with("partition-") {
            return Err(format!("Invalid partition format: {}", partition_str));
        }
        let partition_id: i32 = partition_str[10..].parse()
            .map_err(|_| format!("Invalid partition ID: {}", partition_str))?;

        let segment_file = parts[3];
        if !segment_file.starts_with("segment-") || !segment_file.ends_with(".chronik") {
            return Err(format!("Invalid segment file format: {}", segment_file));
        }
        let offset_str = &segment_file[8..segment_file.len() - 8]; // Remove "segment-" and ".chronik"
        let base_offset: i64 = offset_str.parse()
            .map_err(|_| format!("Invalid base offset: {}", offset_str))?;

        Ok(Self {
            topic,
            partition_id,
            base_offset,
        })
    }
}

/// Simple LRU cache for segment data
pub struct SegmentCache {
    capacity: usize,
    cache: HashMap<SegmentLocation, Bytes>,
    access_order: Vec<SegmentLocation>,
}

impl SegmentCache {
    /// Create a new cache with the given capacity
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            cache: HashMap::new(),
            access_order: Vec::new(),
        }
    }

    /// Get data from cache
    pub fn get(&mut self, location: &SegmentLocation) -> Option<&Bytes> {
        if self.cache.contains_key(location) {
            // Move to end (most recently used)
            self.access_order.retain(|l| l != location);
            self.access_order.push(location.clone());
            self.cache.get(location)
        } else {
            None
        }
    }

    /// Put data into cache
    pub fn put(&mut self, location: SegmentLocation, data: Bytes) {
        // Remove if already exists
        if self.cache.contains_key(&location) {
            self.access_order.retain(|l| l != &location);
        }

        // Evict if at capacity
        while self.cache.len() >= self.capacity && !self.access_order.is_empty() {
            let oldest = self.access_order.remove(0);
            self.cache.remove(&oldest);
        }

        // Insert new entry
        self.cache.insert(location.clone(), data);
        self.access_order.push(location);
    }

    /// Remove from cache
    pub fn remove(&mut self, location: &SegmentLocation) {
        self.cache.remove(location);
        self.access_order.retain(|l| l != location);
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let total_size: usize = self.cache.values().map(|data| data.len()).sum();
        CacheStats {
            entries: self.cache.len(),
            total_size,
            capacity: self.capacity,
        }
    }
}

/// Statistics about segment storage
#[derive(Debug, Clone)]
pub struct SegmentStats {
    pub storage_size: u64,
    pub record_count: u64,
    pub compression_ratio: f64,
    pub last_modified: chrono::DateTime<chrono::Utc>,
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub entries: usize,
    pub total_size: usize,
    pub capacity: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_location_key_conversion() {
        let location = SegmentLocation::new("test-topic".to_string(), 0, 12345);
        let key = location.to_storage_key();
        assert_eq!(key, "segments/test-topic/partition-0/segment-00000000000000012345.chronik");

        let parsed = SegmentLocation::from_storage_key(&key).unwrap();
        assert_eq!(parsed, location);
    }

    #[test]
    fn test_invalid_segment_keys() {
        assert!(SegmentLocation::from_storage_key("invalid").is_err());
        assert!(SegmentLocation::from_storage_key("segments/topic/invalid/segment-123.chronik").is_err());
        assert!(SegmentLocation::from_storage_key("segments/topic/partition-0/invalid.chronik").is_err());
    }

    #[test]
    fn test_segment_cache() {
        let mut cache = SegmentCache::new(2);
        
        let loc1 = SegmentLocation::new("topic1".to_string(), 0, 100);
        let loc2 = SegmentLocation::new("topic2".to_string(), 0, 200);
        let loc3 = SegmentLocation::new("topic3".to_string(), 0, 300);
        
        let data1 = Bytes::from("data1");
        let data2 = Bytes::from("data2");
        let data3 = Bytes::from("data3");

        // Insert first two
        cache.put(loc1.clone(), data1.clone());
        cache.put(loc2.clone(), data2.clone());
        
        assert!(cache.get(&loc1).is_some());
        assert!(cache.get(&loc2).is_some());
        
        // Insert third (should evict first)
        cache.put(loc3.clone(), data3.clone());
        
        assert!(cache.get(&loc1).is_none()); // Evicted
        assert!(cache.get(&loc2).is_some());
        assert!(cache.get(&loc3).is_some());
        
        let stats = cache.stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.capacity, 2);
    }
}