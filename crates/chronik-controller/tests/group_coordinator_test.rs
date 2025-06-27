//! Tests for consumer group coordinator functionality

use chronik_controller::{
    controller::Controller,
    node_v2::{RaftNode, RaftNodeConfig},
    group_coordinator::{GroupCoordinator, GroupCoordinatorConfig},
    proto::{
        controller_service_server::ControllerService,
        JoinGroupRequest, SyncGroupRequest, HeartbeatRequest, LeaveGroupRequest,
        CommitOffsetsRequest, FetchOffsetsRequest, OffsetCommitRequestTopic,
        OffsetCommitRequestPartition, Protocol,
    },
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use std::collections::HashMap;

/// Test helper to create a test controller
async fn create_test_controller() -> Arc<Controller> {
    let config = RaftNodeConfig {
        node_id: 1,
        peers: vec![],
        data_dir: format!("/tmp/test-controller-{}", uuid::Uuid::new_v4()),
        election_tick: 10,
        heartbeat_tick: 3,
        snapshot_interval: 100,
        log_compaction_threshold: 1000,
        enable_pre_vote: true,
    };
    
    Arc::new(Controller::new(config))
}

/// Test helper to create a group coordinator
async fn create_group_coordinator(controller: Arc<Controller>) -> Arc<GroupCoordinator> {
    let config = GroupCoordinatorConfig {
        session_timeout_ms: 30000,
        rebalance_timeout_ms: 60000,
        min_session_timeout_ms: 6000,
        max_session_timeout_ms: 300000,
        group_initial_rebalance_delay_ms: 3000,
        group_max_size: 1000,
    };
    
    let (raft_handle, state) = controller.get_raft_handle_and_state();
    Arc::new(GroupCoordinator::new(config, raft_handle, state))
}

#[tokio::test]
async fn test_group_lifecycle() {
    let controller = create_test_controller().await;
    let coordinator = create_group_coordinator(controller.clone()).await;
    
    let group_id = "test-group".to_string();
    let member_id = "".to_string(); // Empty for first join
    let client_id = "test-client".to_string();
    let client_host = "127.0.0.1".to_string();
    let protocol_type = "consumer".to_string();
    
    // Test 1: Join group as first member
    let join_result = coordinator.join_group(
        group_id.clone(),
        member_id,
        client_id.clone(),
        client_host.clone(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        protocol_type.clone(),
        vec![("range".to_string(), vec![1, 2, 3])],
    ).await.unwrap();
    
    assert_eq!(join_result.error_code, 0);
    assert!(!join_result.member_id.is_empty());
    assert_eq!(join_result.leader_id, join_result.member_id);
    assert_eq!(join_result.generation_id, 1);
    
    let leader_id = join_result.member_id.clone();
    
    // Test 2: Sync group as leader
    let mut assignments = HashMap::new();
    assignments.insert(leader_id.clone(), vec![4, 5, 6]);
    
    let sync_result = coordinator.sync_group(
        group_id.clone(),
        join_result.generation_id,
        leader_id.clone(),
        Some(assignments),
    ).await.unwrap();
    
    assert_eq!(sync_result.error_code, 0);
    assert_eq!(sync_result.assignment, vec![4, 5, 6]);
    
    // Test 3: Heartbeat
    let heartbeat_result = coordinator.heartbeat(
        group_id.clone(),
        leader_id.clone(),
        join_result.generation_id,
    ).await.unwrap();
    
    assert_eq!(heartbeat_result.error_code, 0);
    
    // Test 4: Leave group
    coordinator.leave_group(
        group_id.clone(),
        leader_id.clone(),
    ).await.unwrap();
    
    // Test 5: Heartbeat after leaving should fail
    let heartbeat_result = coordinator.heartbeat(
        group_id.clone(),
        leader_id.clone(),
        join_result.generation_id,
    ).await.unwrap();
    
    assert_ne!(heartbeat_result.error_code, 0);
}

#[tokio::test]
async fn test_multiple_members() {
    let controller = create_test_controller().await;
    let coordinator = create_group_coordinator(controller.clone()).await;
    
    let group_id = "multi-member-group".to_string();
    let protocol_type = "consumer".to_string();
    
    // Member 1 joins
    let member1_result = coordinator.join_group(
        group_id.clone(),
        "".to_string(),
        "client1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        protocol_type.clone(),
        vec![("range".to_string(), vec![1, 2, 3])],
    ).await.unwrap();
    
    assert_eq!(member1_result.error_code, 0);
    let member1_id = member1_result.member_id.clone();
    
    // Member 2 joins - should trigger rebalance
    let member2_result = coordinator.join_group(
        group_id.clone(),
        "".to_string(),
        "client2".to_string(),
        "127.0.0.2".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        protocol_type.clone(),
        vec![("range".to_string(), vec![1, 2, 3])],
    ).await.unwrap();
    
    assert_eq!(member2_result.error_code, 0);
    let member2_id = member2_result.member_id.clone();
    
    // Both should have same generation but member1 is leader
    assert_eq!(member2_result.generation_id, member1_result.generation_id + 1);
    assert_eq!(member2_result.leader_id, member1_id);
    
    // Leader assigns partitions
    let mut assignments = HashMap::new();
    assignments.insert(member1_id.clone(), vec![0, 1, 2]);
    assignments.insert(member2_id.clone(), vec![3, 4, 5]);
    
    let sync_result = coordinator.sync_group(
        group_id.clone(),
        member2_result.generation_id,
        member1_id.clone(),
        Some(assignments),
    ).await.unwrap();
    
    assert_eq!(sync_result.error_code, 0);
}

#[tokio::test]
async fn test_offset_management() {
    let controller = create_test_controller().await;
    let coordinator = create_group_coordinator(controller.clone()).await;
    
    let group_id = "offset-test-group".to_string();
    
    // Join group first
    let join_result = coordinator.join_group(
        group_id.clone(),
        "".to_string(),
        "offset-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
    ).await.unwrap();
    
    let member_id = join_result.member_id.clone();
    
    // Sync group
    let mut assignments = HashMap::new();
    assignments.insert(member_id.clone(), vec![]);
    
    coordinator.sync_group(
        group_id.clone(),
        join_result.generation_id,
        member_id.clone(),
        Some(assignments),
    ).await.unwrap();
    
    // Commit offsets
    let offsets = vec![
        chronik_controller::proto::TopicPartitionOffset {
            topic: "test-topic".to_string(),
            partition: 0,
            offset: 100,
            metadata: "test-metadata".to_string(),
        },
        chronik_controller::proto::TopicPartitionOffset {
            topic: "test-topic".to_string(),
            partition: 1,
            offset: 200,
            metadata: "".to_string(),
        },
    ];
    
    let commit_results = coordinator.commit_offsets(
        group_id.clone(),
        join_result.generation_id,
        member_id.clone(),
        offsets,
    ).await.unwrap();
    
    assert_eq!(commit_results.len(), 2);
    for result in &commit_results {
        assert_eq!(result.error_code, 0);
    }
    
    // Fetch offsets
    let fetched_offsets = coordinator.fetch_offsets(
        group_id.clone(),
        vec!["test-topic".to_string()],
    ).await.unwrap();
    
    assert_eq!(fetched_offsets.len(), 2);
    
    let offset_map: HashMap<i32, i64> = fetched_offsets.iter()
        .map(|o| (o.partition, o.offset))
        .collect();
    
    assert_eq!(offset_map.get(&0), Some(&100));
    assert_eq!(offset_map.get(&1), Some(&200));
}

#[tokio::test]
async fn test_rebalance_timeout() {
    let controller = create_test_controller().await;
    let coordinator = create_group_coordinator(controller.clone()).await;
    
    let group_id = "rebalance-timeout-group".to_string();
    
    // Member 1 joins with short rebalance timeout
    let member1_result = coordinator.join_group(
        group_id.clone(),
        "".to_string(),
        "client1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_millis(100), // Very short rebalance timeout
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
    ).await.unwrap();
    
    // Don't sync within timeout
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Member 2 tries to join - should succeed as member 1 timed out
    let member2_result = coordinator.join_group(
        group_id.clone(),
        "".to_string(),
        "client2".to_string(),
        "127.0.0.2".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
    ).await.unwrap();
    
    // Member 2 should become leader of new generation
    assert_eq!(member2_result.error_code, 0);
    assert_eq!(member2_result.leader_id, member2_result.member_id);
    assert!(member2_result.generation_id > member1_result.generation_id);
}

#[tokio::test]
async fn test_concurrent_operations() {
    let controller = create_test_controller().await;
    let coordinator = create_group_coordinator(controller.clone()).await;
    
    let group_id = "concurrent-group".to_string();
    let num_members = 5;
    
    // Spawn multiple members joining concurrently
    let mut join_handles = vec![];
    
    for i in 0..num_members {
        let coordinator = coordinator.clone();
        let group_id = group_id.clone();
        
        let handle = tokio::spawn(async move {
            coordinator.join_group(
                group_id,
                "".to_string(),
                format!("client{}", i),
                format!("127.0.0.{}", i + 1),
                Duration::from_secs(30),
                Duration::from_secs(60),
                "consumer".to_string(),
                vec![("range".to_string(), vec![])],
            ).await
        });
        
        join_handles.push(handle);
    }
    
    // Wait for all joins to complete
    let mut results = vec![];
    for handle in join_handles {
        let result = handle.await.unwrap().unwrap();
        results.push(result);
    }
    
    // All should succeed
    for result in &results {
        assert_eq!(result.error_code, 0);
    }
    
    // All should have same generation
    let generation = results[0].generation_id;
    for result in &results {
        assert_eq!(result.generation_id, generation);
    }
    
    // One should be leader
    let leader_id = results[0].leader_id.clone();
    for result in &results {
        assert_eq!(result.leader_id, leader_id);
    }
}

#[tokio::test]
async fn test_group_state_persistence() {
    let data_dir = format!("/tmp/test-persistence-{}", uuid::Uuid::new_v4());
    
    // Create controller with specific data dir
    let config = RaftNodeConfig {
        node_id: 1,
        peers: vec![],
        data_dir: data_dir.clone(),
        election_tick: 10,
        heartbeat_tick: 3,
        snapshot_interval: 100,
        log_compaction_threshold: 1000,
        enable_pre_vote: true,
    };
    
    let controller = Arc::new(Controller::new(config.clone()));
    let coordinator = create_group_coordinator(controller.clone()).await;
    
    let group_id = "persistent-group".to_string();
    
    // Join and commit offsets
    let join_result = coordinator.join_group(
        group_id.clone(),
        "".to_string(),
        "persistent-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
    ).await.unwrap();
    
    let offsets = vec![
        chronik_controller::proto::TopicPartitionOffset {
            topic: "persistent-topic".to_string(),
            partition: 0,
            offset: 42,
            metadata: "checkpoint".to_string(),
        },
    ];
    
    coordinator.commit_offsets(
        group_id.clone(),
        join_result.generation_id,
        join_result.member_id.clone(),
        offsets,
    ).await.unwrap();
    
    // Simulate restart by creating new controller with same data dir
    drop(coordinator);
    drop(controller);
    
    let new_controller = Arc::new(Controller::new(config));
    let new_coordinator = create_group_coordinator(new_controller.clone()).await;
    
    // Fetch offsets - should still be there
    let fetched_offsets = new_coordinator.fetch_offsets(
        group_id.clone(),
        vec!["persistent-topic".to_string()],
    ).await.unwrap();
    
    assert_eq!(fetched_offsets.len(), 1);
    assert_eq!(fetched_offsets[0].offset, 42);
    assert_eq!(fetched_offsets[0].metadata, "checkpoint");
    
    // Cleanup
    let _ = std::fs::remove_dir_all(&data_dir);
}