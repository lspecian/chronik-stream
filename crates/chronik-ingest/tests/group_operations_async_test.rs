//! Comprehensive async test suite for consumer group operations
//!
//! This test suite covers:
//! - Async/non-blocking operations
//! - Concurrent group operations
//! - State transitions
//! - Edge cases and error scenarios
//! - Performance and stress testing

use chronik_ingest::{
    consumer_group::{GroupManager, GroupState, AssignmentStrategy, TopicPartitionOffset},
    distributed_lock::DistributedLockManager,
    heartbeat_monitor::{HeartbeatMonitor, HeartbeatMonitorConfig, create_heartbeat_monitor},
};
use chronik_common::metadata::InMemoryMetadataStore;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use tokio::sync::{RwLock, Barrier};
use tikv_client::RawClient;
use futures::future::join_all;

/// Helper to create a test group manager
async fn create_test_manager() -> GroupManager {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    GroupManager::new(metadata_store)
}

/// Helper to create test protocol metadata
fn create_test_protocol_metadata() -> Vec<u8> {
    // Create a simple protocol metadata with version and topics
    let mut data = Vec::new();
    // Version (2 bytes): 0
    data.extend_from_slice(&[0u8, 0u8]);
    // Number of topics (4 bytes): 0 (empty for now)
    data.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]);
    // User data length (4 bytes): -1 (no user data)
    data.extend_from_slice(&[0xffu8, 0xffu8, 0xffu8, 0xffu8]);
    data
}

/// Helper to create a test manager with distributed locking
async fn create_test_manager_with_lock() -> Option<GroupManager> {
    // Try to connect to TiKV
    let pd_endpoints = vec!["localhost:2379".to_string()];
    match RawClient::new(pd_endpoints).await {
        Ok(client) => {
            let tikv_client = Arc::new(client);
            let lock_manager = Arc::new(DistributedLockManager::new(tikv_client));
            let metadata_store = Arc::new(InMemoryMetadataStore::new());
            Some(GroupManager::with_lock_manager(metadata_store, lock_manager))
        }
        Err(_) => None,
    }
}

#[tokio::test]
async fn test_concurrent_join_operations() {
    let manager = Arc::new(create_test_manager().await);
    let barrier = Arc::new(Barrier::new(10));
    
    // Spawn 10 concurrent join operations
    let join_handles: Vec<_> = (0..10).map(|i| {
        let manager = manager.clone();
        let barrier = barrier.clone();
        
        tokio::spawn(async move {
            // Wait for all tasks to be ready
            barrier.wait().await;
            
            let response = manager.join_group(
                "concurrent-group".to_string(),
                None,
                format!("client{}", i),
                format!("127.0.0.{}", i + 1),
                Duration::from_secs(30),
                Duration::from_secs(60),
                "consumer".to_string(),
                vec![("range".to_string(), create_test_protocol_metadata())],
                None,
            ).await.unwrap();
            
            (i, response)
        })
    }).collect();
    
    // Wait for all joins to complete
    let results = join_all(join_handles).await;
    
    // Verify results
    let mut member_ids = Vec::new();
    let mut generation_ids = Vec::new();
    let mut leader_id = None;
    
    for result in results {
        let (idx, response) = result.unwrap();
        assert_eq!(response.error_code, 0);
        assert!(!response.member_id.is_empty());
        member_ids.push(response.member_id.clone());
        generation_ids.push(response.generation_id);
        
        if leader_id.is_none() {
            leader_id = Some(response.leader_id.clone());
        } else {
            // All members should see the same leader
            assert_eq!(response.leader_id, leader_id.as_ref().unwrap().clone());
        }
    }
    
    // All members should have unique IDs
    member_ids.sort();
    member_ids.dedup();
    assert_eq!(member_ids.len(), 10);
    
    // Check final group state
    let group = manager.describe_group("concurrent-group".to_string()).await.unwrap().unwrap();
    assert_eq!(group.members.len(), 10);
}

#[tokio::test]
async fn test_concurrent_heartbeat_operations() {
    let manager = Arc::new(create_test_manager().await);
    
    // First, create a group with members
    let mut member_ids = Vec::new();
    for i in 0..5 {
        let response = manager.join_group(
            "heartbeat-group".to_string(),
            None,
            format!("client{}", i),
            format!("127.0.0.{}", i + 1),
            Duration::from_secs(10), // Short timeout for testing
            Duration::from_secs(30),
            "consumer".to_string(),
            vec![("range".to_string(), create_test_protocol_metadata())],
            None,
        ).await.unwrap();
        member_ids.push(response.member_id);
    }
    
    // Spawn concurrent heartbeat operations
    let barrier = Arc::new(Barrier::new(5));
    let heartbeat_handles: Vec<_> = member_ids.iter().enumerate().map(|(i, member_id)| {
        let manager = manager.clone();
        let barrier = barrier.clone();
        let member_id = member_id.clone();
        
        tokio::spawn(async move {
            // Send heartbeats for 5 seconds
            let start = Instant::now();
            let mut count = 0;
            
            while start.elapsed() < Duration::from_secs(5) {
                barrier.wait().await;
                
                let result = manager.heartbeat(
                    "heartbeat-group".to_string(),
                    member_id.clone(),
                    1, // generation_id
                    None, // member_epoch
                ).await;
                
                assert!(result.is_ok());
                count += 1;
                
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            
            (i, count)
        })
    }).collect();
    
    // Wait for all heartbeat tasks
    let results = join_all(heartbeat_handles).await;
    
    for (idx, result) in results.into_iter().enumerate() {
        let (i, count) = result.unwrap();
        assert_eq!(i, idx);
        assert!(count >= 8); // Should have sent at least 8 heartbeats in 5 seconds
    }
}

#[tokio::test]
async fn test_async_rebalance_operations() {
    let manager = Arc::new(create_test_manager().await);
    
    // Set cooperative sticky strategy
    manager.set_assignment_strategy(AssignmentStrategy::CooperativeSticky).await;
    
    // Phase 1: Initial members join
    let member1 = manager.join_group(
        "rebalance-group".to_string(),
        None,
        "client1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("cooperative-sticky".to_string(), create_test_protocol_metadata())],
        None,
    ).await.unwrap();
    
    let member2 = manager.join_group(
        "rebalance-group".to_string(),
        None,
        "client2".to_string(),
        "127.0.0.2".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("cooperative-sticky".to_string(), create_test_protocol_metadata())],
        None,
    ).await.unwrap();
    
    // Sync group for assignment
    let sync1 = manager.sync_group(
        "rebalance-group".to_string(),
        member1.generation_id,
        member1.member_id.clone(),
        member1.member_epoch,
        Some(vec![]), // Leader provides assignments
    ).await.unwrap();
    
    let sync2 = manager.sync_group(
        "rebalance-group".to_string(),
        member2.generation_id,
        member2.member_id.clone(),
        member2.member_epoch,
        None,
    ).await.unwrap();
    
    // Phase 2: Add new member concurrently with heartbeats
    let manager_clone = manager.clone();
    let member1_id = member1.member_id.clone();
    let member2_id = member2.member_id.clone();
    
    // Heartbeat task
    let heartbeat_task = tokio::spawn(async move {
        for _ in 0..10 {
            manager_clone.heartbeat("rebalance-group".to_string(), member1_id.clone(), 1, None).await.ok();
            manager_clone.heartbeat("rebalance-group".to_string(), member2_id.clone(), 1, None).await.ok();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    
    // Join new member
    let member3 = manager.join_group(
        "rebalance-group".to_string(),
        None,
        "client3".to_string(),
        "127.0.0.3".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("cooperative-sticky".to_string(), create_test_protocol_metadata())],
        None,
    ).await.unwrap();
    
    // Wait for heartbeat task
    heartbeat_task.await.unwrap();
    
    // Verify rebalance occurred
    assert!(member3.generation_id > member1.generation_id);
}

#[tokio::test]
async fn test_distributed_lock_integration() {
    // Skip if TiKV is not available
    let Some(manager) = create_test_manager_with_lock().await else {
        println!("Skipping test - TiKV not available");
        return;
    };
    
    let manager = Arc::new(manager);
    
    // Create multiple concurrent operations that require locking
    let barrier = Arc::new(Barrier::new(5));
    
    let handles: Vec<_> = (0..5).map(|i| {
        let manager = manager.clone();
        let barrier = barrier.clone();
        
        tokio::spawn(async move {
            barrier.wait().await;
            
            // Try to join and immediately update group metadata
            let response = manager.join_group(
                "lock-test-group".to_string(),
                None,
                format!("client{}", i),
                "127.0.0.1".to_string(),
                Duration::from_secs(30),
                Duration::from_secs(60),
                "consumer".to_string(),
                vec![("range".to_string(), create_test_protocol_metadata())],
                None,
            ).await;
            
            (i, response)
        })
    }).collect();
    
    let results = join_all(handles).await;
    
    // All operations should succeed without conflicts
    for (idx, result) in results.into_iter().enumerate() {
        let (i, response) = result.unwrap();
        assert_eq!(i, idx);
        assert!(response.is_ok());
    }
}

#[tokio::test]
async fn test_heartbeat_monitor_integration() {
    let manager = Arc::new(create_test_manager().await);
    let expired_members = Arc::new(RwLock::new(Vec::new()));
    let expired_clone = expired_members.clone();
    
    // Create heartbeat monitor
    let monitor = create_heartbeat_monitor(
        HeartbeatMonitorConfig {
            check_interval: Duration::from_millis(100),
            grace_period: Duration::from_millis(50),
            ..Default::default()
        },
        move |group_id, member_id| {
            let expired = expired_clone.clone();
            tokio::spawn(async move {
                expired.write().await.push((group_id, member_id));
            });
        },
    );
    
    // Start monitor
    monitor.clone().start();
    
    // Create a group with short timeout
    let response = manager.join_group(
        "monitor-test".to_string(),
        None,
        "client1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_millis(200), // Very short timeout
        Duration::from_secs(30),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    let member_id = response.member_id.clone();
    
    // Record initial heartbeat through monitor
    monitor.record_heartbeat(
        "monitor-test",
        &member_id,
        response.generation_id,
        Duration::from_millis(200),
    ).await.unwrap();
    
    // Send a few heartbeats
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        monitor.record_heartbeat(
            "monitor-test",
            &member_id,
            response.generation_id,
            Duration::from_millis(200),
        ).await.unwrap();
    }
    
    // Stop sending heartbeats and wait for expiration
    tokio::time::sleep(Duration::from_millis(400)).await;
    
    // Check if member was marked as expired
    let expired = expired_members.read().await;
    assert!(!expired.is_empty());
    assert_eq!(expired[0], ("monitor-test".to_string(), member_id));
}

#[tokio::test]
async fn test_async_offset_operations() {
    let manager = Arc::new(create_test_manager().await);
    
    // Create a consumer group
    let response = manager.join_group(
        "offset-group".to_string(),
        None,
        "client1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    let member_id = response.member_id;
    let generation_id = response.generation_id;
    
    // Commit offsets concurrently
    let manager_clone = manager.clone();
    let commit_tasks: Vec<_> = (0..5).map(|partition| {
        let manager = manager_clone.clone();
        let member_id = member_id.clone();
        
        tokio::spawn(async move {
            let result = manager.commit_offsets(
                "offset-group".to_string(),
                generation_id,
                member_id,
                None, // member_epoch
                vec![TopicPartitionOffset {
                    topic: "test-topic".to_string(),
                    partition: partition as i32,
                    offset: (partition as i64 + 1) * 100,
                    metadata: Some(format!("metadata-{}", partition)),
                }],
            ).await;
            
            (partition, result)
        })
    }).collect();
    
    let commit_results = join_all(commit_tasks).await;
    
    // Verify all commits succeeded
    for (idx, result) in commit_results.into_iter().enumerate() {
        let (partition, commit_result) = result.unwrap();
        assert_eq!(partition, idx as u32);
        assert!(commit_result.is_ok());
    }
    
    // Fetch offsets concurrently
    let fetch_tasks: Vec<_> = (0..5).map(|partition| {
        let manager = manager.clone();
        
        tokio::spawn(async move {
            let offsets = manager.fetch_offsets(
                "offset-group".to_string(),
                vec!["test-topic".to_string()],
            ).await;
            
            (partition, offsets)
        })
    }).collect();
    
    let fetch_results = join_all(fetch_tasks).await;
    
    // Verify fetched offsets
    for result in fetch_results {
        let (_partition, offsets) = result.unwrap();
        assert!(offsets.is_ok());
        let offsets = offsets.unwrap();
        
        // Should have offsets for all partitions
        let topic_offsets: Vec<_> = offsets.iter()
            .filter(|o| o.topic == "test-topic")
            .collect();
        
        // Verify we got offsets for partitions we committed
        for partition in 0..5 {
            let offset = topic_offsets.iter()
                .find(|o| o.partition == partition)
                .expect(&format!("Missing offset for partition {}", partition));
            
            assert_eq!(offset.offset, (partition as i64 + 1) * 100);
            assert_eq!(offset.metadata, Some(format!("metadata-{}", partition)));
        }
    }
}

#[tokio::test]
async fn test_stress_concurrent_operations() {
    let manager = Arc::new(create_test_manager().await);
    let num_groups = 10;
    let members_per_group = 20;
    let operations_per_member = 50;
    
    // Create multiple groups concurrently
    let group_handles: Vec<_> = (0..num_groups).map(|group_idx| {
        let manager = manager.clone();
        
        tokio::spawn(async move {
            let group_id = format!("stress-group-{}", group_idx);
            
            // Create members for this group
            let member_handles: Vec<_> = (0..members_per_group).map(|member_idx| {
                let manager = manager.clone();
                let group_id = group_id.clone();
                
                tokio::spawn(async move {
                    // Join group
                    let response = manager.join_group(
                        group_id.clone(),
                        None,
                        format!("client-{}-{}", group_idx, member_idx),
                        "127.0.0.1".to_string(),
                        Duration::from_secs(30),
                        Duration::from_secs(60),
                        "consumer".to_string(),
                        vec![("range".to_string(), create_test_protocol_metadata())],
                        None,
                    ).await.unwrap();
                    
                    let member_id = response.member_id;
                    let generation_id = response.generation_id;
                    
                    // Perform multiple operations
                    for op in 0..operations_per_member {
                        match op % 4 {
                            0 => {
                                // Heartbeat
                                manager.heartbeat(
                                    group_id.clone(),
                                    member_id.clone(),
                                    generation_id,
                                    None,
                                ).await.ok();
                            }
                            1 => {
                                // Commit offset
                                manager.commit_offsets(
                                    group_id.clone(),
                                    generation_id,
                                    member_id.clone(),
                                    None,
                                    vec![TopicPartitionOffset {
                                        topic: "topic".to_string(),
                                        partition: (op % 10) as i32,
                                        offset: op as i64,
                                        metadata: None,
                                    }],
                                ).await.ok();
                            }
                            2 => {
                                // Fetch offsets
                                manager.fetch_offsets(
                                    group_id.clone(),
                                    vec!["topic".to_string()],
                                ).await.ok();
                            }
                            3 => {
                                // Describe group
                                manager.describe_group(group_id.clone()).await.ok();
                            }
                            _ => unreachable!(),
                        }
                        
                        // Small delay to spread operations
                        tokio::time::sleep(Duration::from_micros(100)).await;
                    }
                    
                    (group_idx, member_idx)
                })
            }).collect();
            
            let member_results = join_all(member_handles).await;
            
            // Verify all members completed
            assert_eq!(member_results.len(), members_per_group);
            
            group_idx
        })
    }).collect();
    
    let start = Instant::now();
    let group_results = join_all(group_handles).await;
    let elapsed = start.elapsed();
    
    // Verify all groups completed
    assert_eq!(group_results.len(), num_groups);
    
    println!(
        "Stress test completed: {} groups, {} members/group, {} ops/member in {:?}",
        num_groups, members_per_group, operations_per_member, elapsed
    );
    
    // Verify final state
    let groups = manager.list_groups().await.unwrap();
    assert_eq!(groups.len(), num_groups);
}

#[tokio::test]
async fn test_edge_case_empty_group_transitions() {
    let manager = Arc::new(create_test_manager().await);
    
    // Create and immediately check empty group
    manager.get_or_create_group("empty-group".to_string(), "consumer".to_string()).await.unwrap();
    
    let group = manager.describe_group("empty-group".to_string()).await.unwrap().unwrap();
    assert_eq!(group.state, GroupState::Empty);
    assert_eq!(group.members.len(), 0);
    
    // Join and immediately leave
    let response = manager.join_group(
        "empty-group".to_string(),
        None,
        "client1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    // Leave by not sending heartbeats and waiting for timeout
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Manually trigger member removal
    let result = manager.leave_group(
        "empty-group".to_string(),
        response.member_id.clone(),
    ).await;
    
    assert!(result.is_ok());
    
    // Group should be empty again
    let group = manager.describe_group("empty-group".to_string()).await.unwrap().unwrap();
    assert_eq!(group.state, GroupState::Empty);
}

#[tokio::test]
async fn test_edge_case_rapid_rebalances() {
    let manager = Arc::new(create_test_manager().await);
    
    // Create initial member
    let member1 = manager.join_group(
        "rapid-rebalance".to_string(),
        None,
        "client1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    // Rapidly add and remove members
    for i in 0..5 {
        // Add member
        let member = manager.join_group(
            "rapid-rebalance".to_string(),
            None,
            format!("client{}", i + 2),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), create_test_protocol_metadata())],
            None,
        ).await.unwrap();
        
        // Small delay
        tokio::time::sleep(Duration::from_millis(50)).await;
        
        // Remove member
        manager.leave_group(
            "rapid-rebalance".to_string(),
            member.member_id,
        ).await.ok();
    }
    
    // Final state should be stable with just the first member
    let group = manager.describe_group("rapid-rebalance".to_string()).await.unwrap().unwrap();
    assert_eq!(group.members.len(), 1);
    assert!(group.generation_id > 1); // Multiple rebalances should have occurred
}