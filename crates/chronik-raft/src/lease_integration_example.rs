//! Integration example: Using LeaseManager with FetchHandler for fast reads
//!
//! This example shows how to integrate LeaseManager with Chronik's FetchHandler
//! to enable sub-5ms reads without ReadIndex RPC overhead.

#![allow(dead_code)]

use crate::{LeaseConfig, LeaseManager, RaftGroupManager, ReadIndexManager};
use std::sync::Arc;
use std::time::Instant;

/// Example FetchHandler with lease-based read fallback chain
///
/// Read path (in order of preference):
/// 1. Lease-based read (0-5ms) - if lease valid
/// 2. ReadIndex read (10-50ms) - if lease invalid
/// 3. Forward to leader (50-200ms) - if not leader
pub struct FetchHandlerWithLease {
    /// Lease manager (for fast reads)
    lease_manager: Option<Arc<LeaseManager>>,

    /// ReadIndex manager (for fallback)
    read_index_manager: Option<Arc<ReadIndexManager>>,

    /// Raft group manager
    raft_group_manager: Arc<RaftGroupManager>,
}

impl FetchHandlerWithLease {
    /// Create a new FetchHandler with lease-based reads
    pub fn new(
        raft_group_manager: Arc<RaftGroupManager>,
        lease_config: LeaseConfig,
    ) -> anyhow::Result<Self> {
        // Create lease manager
        let lease_manager = Arc::new(LeaseManager::new(
            raft_group_manager.node_id(),
            raft_group_manager.clone(),
            lease_config,
        )?);

        // Spawn background lease renewal loop
        lease_manager.clone().spawn_renewal_loop();

        Ok(Self {
            lease_manager: Some(lease_manager),
            read_index_manager: None,
            raft_group_manager,
        })
    }

    /// Add ReadIndex manager for fallback
    pub fn with_read_index(mut self, read_index_manager: Arc<ReadIndexManager>) -> Self {
        self.read_index_manager = Some(read_index_manager);
        self
    }

    /// Handle fetch request with lease-based read optimization
    pub async fn handle_fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> anyhow::Result<Vec<u8>> {
        let start = Instant::now();

        // Phase 1: Try lease-based read (fastest path)
        if let Some(lease_manager) = &self.lease_manager {
            if let Some(safe_index) = lease_manager.get_read_index(topic, partition) {
                println!(
                    "Lease-based read: topic={} partition={} offset={} safe_index={} (latency: {:?})",
                    topic,
                    partition,
                    offset,
                    safe_index,
                    start.elapsed()
                );

                // Serve read from local state (no RPC needed)
                return self
                    .serve_read_from_local(topic, partition, offset, safe_index)
                    .await;
            }
        }

        // Phase 2: Try ReadIndex protocol (fallback if lease invalid)
        if let Some(read_index_manager) = &self.read_index_manager {
            println!(
                "ReadIndex fallback: topic={} partition={} offset={} (lease invalid)",
                topic, partition, offset
            );

            let read_req = crate::ReadIndexRequest {
                topic: topic.to_string(),
                partition,
            };

            let response = read_index_manager.request_read_index(read_req).await?;

            println!(
                "ReadIndex response: commit_index={} is_leader={} (latency: {:?})",
                response.commit_index,
                response.is_leader,
                start.elapsed()
            );

            return self
                .serve_read_from_local(topic, partition, offset, response.commit_index)
                .await;
        }

        // Phase 3: Forward to leader (slowest path)
        println!(
            "Forward to leader: topic={} partition={} offset={} (no ReadIndex available)",
            topic, partition, offset
        );

        self.forward_to_leader(topic, partition, offset).await
    }

    /// Serve read from local state (using commit_index as safety bound)
    async fn serve_read_from_local(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        safe_commit_index: u64,
    ) -> anyhow::Result<Vec<u8>> {
        // In real implementation, this would:
        // 1. Check if offset <= safe_commit_index
        // 2. Read from local segment/WAL
        // 3. Return data
        //
        // For demo, we just return placeholder

        println!(
            "Serving read from local: topic={} partition={} offset={} safe_index={}",
            topic, partition, offset, safe_commit_index
        );

        Ok(format!(
            "data-{}-{}-{}",
            topic, partition, offset
        )
        .into_bytes())
    }

    /// Forward read to Raft leader
    async fn forward_to_leader(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> anyhow::Result<Vec<u8>> {
        // In real implementation, this would:
        // 1. Get leader ID from RaftGroupManager
        // 2. Send gRPC fetch request to leader
        // 3. Return response
        //
        // For demo, we just return placeholder

        let leader_id = self
            .raft_group_manager
            .get_leader_for_partition(topic, partition)
            .unwrap_or(0);

        println!(
            "Forwarding to leader {}: topic={} partition={} offset={}",
            leader_id, topic, partition, offset
        );

        Ok(format!(
            "forwarded-data-{}-{}-{}",
            topic, partition, offset
        )
        .into_bytes())
    }
}

/// Example: Performance comparison between lease-based and ReadIndex reads
#[cfg(test)]
mod example_tests {
    use super::*;
    use crate::{MemoryLogStorage, RaftConfig};
    use std::time::Duration;

    async fn create_test_group_manager(node_id: u64) -> Arc<RaftGroupManager> {
        let config = RaftConfig {
            node_id,
            listen_addr: format!("127.0.0.1:{}", 5000 + node_id),
            election_timeout_ms: 10_000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        };

        Arc::new(RaftGroupManager::new(
            node_id,
            config,
            || Arc::new(MemoryLogStorage::new()),
        ))
    }

    #[tokio::test]
    async fn example_lease_based_read() {
        let group_manager = create_test_group_manager(1).await;

        // Create partition replica and make it leader
        let replica = group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Create FetchHandler with lease-based reads
        let lease_config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(9),
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_millis(500),
        };

        let handler = FetchHandlerWithLease::new(group_manager.clone(), lease_config).unwrap();

        // Grant lease
        if let Some(lease_manager) = &handler.lease_manager {
            lease_manager.grant_lease("test-topic", 0).unwrap();
        }

        // Perform lease-based read
        let result = handler.handle_fetch("test-topic", 0, 100).await.unwrap();

        assert!(!result.is_empty());
        println!("Lease-based read result: {} bytes", result.len());
    }

    #[tokio::test]
    async fn example_readindex_fallback() {
        let group_manager = create_test_group_manager(1).await;

        // Create partition replica and make it leader
        let replica = group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Create FetchHandler with ReadIndex fallback
        let lease_config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(9),
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_millis(500),
        };

        let read_index_manager = Arc::new(ReadIndexManager::new(
            group_manager.node_id(),
            replica.clone(),
        ));

        let handler = FetchHandlerWithLease::new(group_manager.clone(), lease_config)
            .unwrap()
            .with_read_index(read_index_manager);

        // Don't grant lease - should fall back to ReadIndex
        let result = handler.handle_fetch("test-topic", 0, 100).await.unwrap();

        assert!(!result.is_empty());
        println!("ReadIndex fallback result: {} bytes", result.len());
    }

    #[tokio::test]
    async fn example_performance_comparison() {
        let group_manager = create_test_group_manager(1).await;

        // Create partition replica and make it leader
        let replica = group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Create handler with both lease and ReadIndex
        let lease_config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(9),
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_millis(500),
        };

        let read_index_manager = Arc::new(ReadIndexManager::new(
            group_manager.node_id(),
            replica.clone(),
        ));

        let handler = FetchHandlerWithLease::new(group_manager.clone(), lease_config)
            .unwrap()
            .with_read_index(read_index_manager);

        // Grant lease for partition 0
        if let Some(lease_manager) = &handler.lease_manager {
            lease_manager.grant_lease("test-topic", 0).unwrap();
        }

        println!("\n=== Performance Comparison ===\n");

        // Test 1: Lease-based read (partition 0 - has lease)
        let start = Instant::now();
        let _result = handler.handle_fetch("test-topic", 0, 100).await.unwrap();
        let lease_latency = start.elapsed();
        println!("Lease-based read latency: {:?}", lease_latency);

        // Test 2: ReadIndex read (partition 1 - no lease)
        let start = Instant::now();
        let _result = handler.handle_fetch("test-topic", 1, 100).await.unwrap();
        let readindex_latency = start.elapsed();
        println!("ReadIndex fallback latency: {:?}", readindex_latency);

        // Verify lease read is faster
        // (In real deployment: lease < 5ms, ReadIndex 10-50ms)
        println!(
            "\nSpeedup: {:.1}x faster with lease",
            readindex_latency.as_micros() as f64 / lease_latency.as_micros() as f64
        );
    }
}
