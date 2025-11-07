//! Coordinator management for consumer groups and transactions
//!
//! This module implements coordinator assignment logic using consistent hashing
//! to distribute consumer groups across available brokers.

use chronik_common::{Result, Error};
use chronik_common::metadata::{MetadataStore, BrokerMetadata};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::{HashMap, BTreeMap};
use std::hash::{Hash, Hasher};
use murmur2::murmur2;
use tracing::{debug, info, warn, error};
use chrono::{DateTime, Utc};

/// Coordinator types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoordinatorType {
    Group = 0,
    Transaction = 1,
}

impl TryFrom<i8> for CoordinatorType {
    type Error = Error;
    
    fn try_from(value: i8) -> Result<Self> {
        match value {
            0 => Ok(CoordinatorType::Group),
            1 => Ok(CoordinatorType::Transaction),
            _ => Err(Error::Protocol(format!("Invalid coordinator type: {}", value))),
        }
    }
}

/// Coordinator assignment information
#[derive(Debug, Clone)]
pub struct CoordinatorAssignment {
    pub key: String,
    pub coordinator_type: CoordinatorType,
    pub broker_id: i32,
    pub broker_host: String,
    pub broker_port: i32,
    pub assigned_at: DateTime<Utc>,
}

/// Coordinator manager configuration
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Number of virtual nodes per broker for consistent hashing
    pub virtual_nodes: u32,
    /// Rebalance delay when brokers join/leave
    pub rebalance_delay_ms: u64,
    /// Enable coordinator caching
    pub enable_cache: bool,
    /// Cache TTL in seconds
    pub cache_ttl_seconds: u64,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            virtual_nodes: 100,
            rebalance_delay_ms: 5000,
            enable_cache: true,
            cache_ttl_seconds: 300, // 5 minutes
        }
    }
}

/// Coordinator manager
pub struct CoordinatorManager {
    metadata_store: Arc<dyn MetadataStore>,
    config: CoordinatorConfig,
    
    /// Cache of coordinator assignments
    assignment_cache: Arc<RwLock<HashMap<(String, CoordinatorType), CachedAssignment>>>,
    
    /// Consistent hash ring for coordinator assignment
    hash_ring: Arc<RwLock<ConsistentHashRing>>,
    
    /// Current broker ID
    current_broker_id: i32,
}

/// Cached assignment with expiration
#[derive(Debug, Clone)]
struct CachedAssignment {
    assignment: CoordinatorAssignment,
    cached_at: std::time::Instant,
}

/// Consistent hash ring for coordinator assignment
struct ConsistentHashRing {
    ring: BTreeMap<u32, i32>, // hash -> broker_id
    brokers: HashMap<i32, BrokerMetadata>,
    virtual_nodes: u32,
}

impl ConsistentHashRing {
    fn new(virtual_nodes: u32) -> Self {
        Self {
            ring: BTreeMap::new(),
            brokers: HashMap::new(),
            virtual_nodes,
        }
    }
    
    /// Add a broker to the ring
    fn add_broker(&mut self, broker: BrokerMetadata) {
        self.brokers.insert(broker.broker_id, broker.clone());
        
        // Add virtual nodes for this broker
        for i in 0..self.virtual_nodes {
            let key = format!("{}:{}", broker.broker_id, i);
            let hash = murmur2(key.as_bytes(), 0);
            self.ring.insert(hash, broker.broker_id);
        }
    }
    
    /// Remove a broker from the ring
    fn remove_broker(&mut self, broker_id: i32) {
        self.brokers.remove(&broker_id);
        
        // Remove virtual nodes for this broker
        self.ring.retain(|_, &mut v| v != broker_id);
    }
    
    /// Get the broker responsible for a key
    fn get_broker_for_key(&self, key: &str) -> Option<&BrokerMetadata> {
        if self.ring.is_empty() {
            return None;
        }
        
        let hash = murmur2(key.as_bytes(), 0);
        
        // Find the first node with hash >= key hash
        let broker_id = self.ring.range(hash..).next()
            .or_else(|| self.ring.iter().next()) // Wrap around
            .map(|(_, &broker_id)| broker_id)?;
        
        self.brokers.get(&broker_id)
    }
    
    /// Update the ring with current broker list
    fn update_brokers(&mut self, brokers: Vec<BrokerMetadata>) {
        // Remove brokers that are no longer in the list
        let new_broker_ids: std::collections::HashSet<_> = brokers.iter()
            .map(|b| b.broker_id)
            .collect();
        
        let to_remove: Vec<_> = self.brokers.keys()
            .filter(|id| !new_broker_ids.contains(id))
            .cloned()
            .collect();
        
        for broker_id in to_remove {
            self.remove_broker(broker_id);
        }
        
        // Add new brokers
        for broker in brokers {
            if !self.brokers.contains_key(&broker.broker_id) {
                self.add_broker(broker);
            }
        }
    }
}

impl CoordinatorManager {
    /// Create a new coordinator manager
    pub fn new(
        metadata_store: Arc<dyn MetadataStore>,
        config: CoordinatorConfig,
        current_broker_id: i32,
    ) -> Self {
        let hash_ring = Arc::new(RwLock::new(ConsistentHashRing::new(config.virtual_nodes)));
        let manager = Self {
            metadata_store,
            config,
            assignment_cache: Arc::new(RwLock::new(HashMap::new())),
            hash_ring,
            current_broker_id,
        };
        
        // Start background task for monitoring broker changes
        manager.start_broker_monitor();
        
        manager
    }
    
    /// Initialize the coordinator manager
    pub async fn init(&self) -> Result<()> {
        // Load broker list and build hash ring
        let brokers = self.metadata_store.list_brokers().await?;
        
        let mut ring = self.hash_ring.write().await;
        ring.update_brokers(brokers);
        
        info!("Coordinator manager initialized with {} brokers", ring.brokers.len());
        Ok(())
    }
    
    /// Find coordinator for a key
    pub async fn find_coordinator(
        &self,
        key: &str,
        coordinator_type: CoordinatorType,
    ) -> Result<CoordinatorAssignment> {
        // Check cache first if enabled
        if self.config.enable_cache {
            let cache_key = (key.to_string(), coordinator_type);
            let cache = self.assignment_cache.read().await;
            
            if let Some(cached) = cache.get(&cache_key) {
                if cached.cached_at.elapsed().as_secs() < self.config.cache_ttl_seconds {
                    debug!("Returning cached coordinator assignment for key '{}'", key);
                    return Ok(cached.assignment.clone());
                }
            }
        }
        
        // Get coordinator from hash ring
        let ring = self.hash_ring.read().await;
        let broker = ring.get_broker_for_key(key)
            .ok_or_else(|| Error::Internal("No brokers available for coordinator assignment".to_string()))?;
        
        let assignment = CoordinatorAssignment {
            key: key.to_string(),
            coordinator_type,
            broker_id: broker.broker_id,
            broker_host: broker.host.clone(),
            broker_port: broker.port,
            assigned_at: Utc::now(),
        };
        
        // Update cache
        if self.config.enable_cache {
            let mut cache = self.assignment_cache.write().await;
            cache.insert(
                (key.to_string(), coordinator_type),
                CachedAssignment {
                    assignment: assignment.clone(),
                    cached_at: std::time::Instant::now(),
                }
            );
        }
        
        info!(
            "Assigned coordinator for key '{}' (type {:?}) to broker {}",
            key, coordinator_type, broker.broker_id
        );
        
        Ok(assignment)
    }
    
    /// Check if this broker is the coordinator for a key
    pub async fn is_coordinator_for(&self, key: &str, coordinator_type: CoordinatorType) -> Result<bool> {
        let assignment = self.find_coordinator(key, coordinator_type).await?;
        Ok(assignment.broker_id == self.current_broker_id)
    }
    
    /// Handle broker membership changes
    pub async fn handle_broker_change(&self) -> Result<()> {
        info!("Handling broker membership change");
        
        // Reload broker list
        let brokers = self.metadata_store.list_brokers().await?;
        
        // Update hash ring
        let mut ring = self.hash_ring.write().await;
        let old_broker_count = ring.brokers.len();
        ring.update_brokers(brokers);
        let new_broker_count = ring.brokers.len();
        
        if old_broker_count != new_broker_count {
            // Clear cache on membership change
            let mut cache = self.assignment_cache.write().await;
            cache.clear();
            
            info!(
                "Broker membership changed from {} to {} brokers, cleared coordinator cache",
                old_broker_count, new_broker_count
            );
        }
        
        Ok(())
    }
    
    /// Get coordinator statistics
    pub async fn get_stats(&self) -> CoordinatorStats {
        let ring = self.hash_ring.read().await;
        let cache = self.assignment_cache.read().await;
        
        CoordinatorStats {
            total_brokers: ring.brokers.len(),
            cached_assignments: cache.len(),
            virtual_nodes_per_broker: self.config.virtual_nodes,
        }
    }
    
    /// Start background task for monitoring broker changes
    fn start_broker_monitor(&self) {
        let metadata_store = self.metadata_store.clone();
        let hash_ring = self.hash_ring.clone();
        let assignment_cache = self.assignment_cache.clone();
        let rebalance_delay_ms = self.config.rebalance_delay_ms;
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            let mut last_broker_state = HashMap::new();
            
            loop {
                interval.tick().await;
                
                // Get current broker list
                match metadata_store.list_brokers().await {
                    Ok(brokers) => {
                        let current_state: HashMap<i32, chronik_common::metadata::BrokerStatus> = 
                            brokers.iter()
                                .map(|b| (b.broker_id, b.status.clone()))
                                .collect();
                        
                        // Check for changes
                        let changed = current_state.len() != last_broker_state.len() ||
                            current_state.iter().any(|(id, status)| {
                                last_broker_state.get(id) != Some(status)
                            });
                        
                        if changed {
                            info!("Broker membership changed, triggering rebalance");
                            
                            // Wait for rebalance delay to allow transient issues to resolve
                            tokio::time::sleep(std::time::Duration::from_millis(rebalance_delay_ms)).await;
                            
                            // Re-check after delay
                            match metadata_store.list_brokers().await {
                                Ok(brokers_after_delay) => {
                                    // Update hash ring
                                    let mut ring = hash_ring.write().await;
                                    ring.update_brokers(brokers_after_delay);
                                    
                                    // Clear cache to force reassignment
                                    let mut cache = assignment_cache.write().await;
                                    cache.clear();
                                    
                                    info!("Coordinator rebalance completed");
                                }
                                Err(e) => {
                                    error!("Failed to get broker list after delay: {}", e);
                                }
                            }
                        }
                        
                        last_broker_state = current_state;
                    }
                    Err(e) => {
                        error!("Failed to monitor broker changes: {}", e);
                    }
                }
            }
        });
    }
    
    /// Get all consumer groups for which this broker is the coordinator
    pub async fn get_coordinated_groups(&self) -> Result<Vec<String>> {
        // This would need to be implemented by scanning all consumer groups
        // For now, return empty as this is mainly for monitoring
        Ok(vec![])
    }
    
    /// Handle graceful coordinator shutdown
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down coordinator manager");
        
        // Clear cache
        let mut cache = self.assignment_cache.write().await;
        cache.clear();
        
        // In a real implementation, we would:
        // 1. Stop accepting new coordinator requests
        // 2. Notify consumer groups to find new coordinator
        // 3. Wait for ongoing operations to complete
        
        Ok(())
    }
}

/// Coordinator statistics
#[derive(Debug, Clone)]
pub struct CoordinatorStats {
    pub total_brokers: usize,
    pub cached_assignments: usize,
    pub virtual_nodes_per_broker: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_common::metadata::traits::MetadataStore as MetadataStoreTrait;
    
    // Mock metadata store for testing
    struct MockMetadataStore {
        brokers: Arc<RwLock<Vec<BrokerMetadata>>>,
    }
    
    impl MockMetadataStore {
        fn new() -> Self {
            Self {
                brokers: Arc::new(RwLock::new(vec![])),
            }
        }
        
        fn add_broker(&self, broker: BrokerMetadata) {
            let mut brokers = self.brokers.blocking_write();
            brokers.push(broker);
        }
    }
    
    #[async_trait::async_trait]
    impl MetadataStoreTrait for MockMetadataStore {
        async fn list_brokers(&self) -> chronik_common::metadata::traits::Result<Vec<BrokerMetadata>> {
            Ok(self.brokers.read().await.clone())
        }
        
        // Implement other required methods as no-ops
        async fn create_topic(&self, _name: &str, _config: chronik_common::metadata::TopicConfig) -> chronik_common::metadata::traits::Result<chronik_common::metadata::TopicMetadata> {
            unimplemented!()
        }
        
        async fn create_topic_with_assignments(
            &self,
            _name: &str,
            _config: chronik_common::metadata::TopicConfig,
            _assignments: Vec<chronik_common::metadata::PartitionAssignment>,
            _segment_boundaries: Vec<(u32, i64, i64)>,
        ) -> chronik_common::metadata::traits::Result<chronik_common::metadata::TopicMetadata> {
            unimplemented!()
        }
        async fn get_topic(&self, _name: &str) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::TopicMetadata>> {
            unimplemented!()
        }
        async fn list_topics(&self) -> chronik_common::metadata::traits::Result<Vec<chronik_common::metadata::TopicMetadata>> {
            unimplemented!()
        }
        async fn update_topic(&self, _name: &str, _config: chronik_common::metadata::TopicConfig) -> chronik_common::metadata::traits::Result<chronik_common::metadata::TopicMetadata> {
            unimplemented!()
        }
        async fn delete_topic(&self, _name: &str) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn persist_segment_metadata(&self, _metadata: chronik_common::metadata::SegmentMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn get_segment_metadata(&self, _topic: &str, _segment_id: &str) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::SegmentMetadata>> {
            unimplemented!()
        }
        async fn list_segments(&self, _topic: &str, _partition: Option<u32>) -> chronik_common::metadata::traits::Result<Vec<chronik_common::metadata::SegmentMetadata>> {
            unimplemented!()
        }
        async fn delete_segment(&self, _topic: &str, _segment_id: &str) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn register_broker(&self, _metadata: BrokerMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn get_broker(&self, _broker_id: i32) -> chronik_common::metadata::traits::Result<Option<BrokerMetadata>> {
            unimplemented!()
        }
        async fn update_broker_status(&self, _broker_id: i32, _status: chronik_common::metadata::BrokerStatus) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn assign_partition(&self, _assignment: chronik_common::metadata::PartitionAssignment) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn get_partition_assignments(&self, _topic: &str) -> chronik_common::metadata::traits::Result<Vec<chronik_common::metadata::PartitionAssignment>> {
            unimplemented!()
        }
        async fn create_consumer_group(&self, _metadata: chronik_common::metadata::ConsumerGroupMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn get_consumer_group(&self, _group_id: &str) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::ConsumerGroupMetadata>> {
            unimplemented!()
        }
        async fn update_consumer_group(&self, _metadata: chronik_common::metadata::ConsumerGroupMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn commit_offset(&self, _offset: chronik_common::metadata::ConsumerOffset) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn get_consumer_offset(&self, _group_id: &str, _topic: &str, _partition: u32) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::ConsumerOffset>> {
            unimplemented!()
        }
        async fn update_partition_offset(&self, _topic: &str, _partition: u32, _high_watermark: i64, _log_start_offset: i64) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        async fn get_partition_offset(&self, _topic: &str, _partition: u32) -> chronik_common::metadata::traits::Result<Option<(i64, i64)>> {
            unimplemented!()
        }
        async fn init_system_state(&self) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        // New methods for partition leadership and transactional support
        async fn get_partition_leader(&self, _topic: &str, _partition: u32) -> chronik_common::metadata::traits::Result<Option<i32>> {
            unimplemented!()
        }

        async fn get_partition_replicas(&self, _topic: &str, _partition: u32) -> chronik_common::metadata::traits::Result<Option<Vec<i32>>> {
            unimplemented!()
        }

        async fn commit_transactional_offsets(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _group_id: String,
            _offsets: Vec<(String, u32, i64, Option<String>)>,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        async fn begin_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _timeout_ms: i32,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        async fn add_partitions_to_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _partitions: Vec<(String, u32)>,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        async fn add_offsets_to_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _group_id: String,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        async fn prepare_commit_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        async fn commit_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        async fn abort_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }

        async fn fence_producer(
            &self,
            _transactional_id: String,
            _old_producer_id: i64,
            _old_producer_epoch: i16,
            _new_producer_id: i64,
            _new_producer_epoch: i16,
        ) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
    }
    
    #[test]
    fn test_murmur2_hash() {
        // Test that murmur2 hash works correctly
        let key = "test-group";
        let hash = murmur2(key.as_bytes(), 0);
        assert_ne!(hash, 0);
        
        // Hash should be consistent
        let hash2 = murmur2(key.as_bytes(), 0);
        assert_eq!(hash, hash2);
    }
    
    #[test]
    fn test_coordinator_type_conversion() {
        assert_eq!(CoordinatorType::try_from(0).unwrap(), CoordinatorType::Group);
        assert_eq!(CoordinatorType::try_from(1).unwrap(), CoordinatorType::Transaction);
        assert!(CoordinatorType::try_from(2).is_err());
    }
    
    #[test]
    fn test_consistent_hash_ring() {
        let mut ring = ConsistentHashRing::new(3);
        
        // Add brokers
        ring.add_broker(BrokerMetadata {
            broker_id: 1,
            host: "broker1".to_string(),
            port: 9092,
            rack: None,
            status: chronik_common::metadata::BrokerStatus::Online,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        
        ring.add_broker(BrokerMetadata {
            broker_id: 2,
            host: "broker2".to_string(),
            port: 9092,
            rack: None,
            status: chronik_common::metadata::BrokerStatus::Online,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        
        // Test key assignment
        let broker1 = ring.get_broker_for_key("test-key-1");
        assert!(broker1.is_some());
        
        // Same key should always map to same broker
        let broker2 = ring.get_broker_for_key("test-key-1");
        assert_eq!(broker1.unwrap().broker_id, broker2.unwrap().broker_id);
        
        // Different keys should distribute
        let mut assignments = HashMap::new();
        for i in 0..100 {
            let key = format!("test-key-{}", i);
            if let Some(broker) = ring.get_broker_for_key(&key) {
                *assignments.entry(broker.broker_id).or_insert(0) += 1;
            }
        }
        
        // Both brokers should get some assignments
        assert!(assignments.len() > 1);
    }
}