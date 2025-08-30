//! Integration tests for consumer group coordinator with KIP-848 support

use chronik_common::metadata::FileMetadataStore;
use chronik_common::metadata::traits::{MetadataStore, TopicConfig};
use chronik_ingest::consumer_group::{
    AssignmentStrategy, GroupManager, GroupState, TopicPartitionOffset,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

#[tokio::test]
async fn test_consumer_group_lifecycle() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
    metadata_store.init_system_state().await.unwrap();
    
    let manager = Arc::new(GroupManager::new(metadata_store));
    
    // Join group
    let response = manager.join_group(
        "test-group".to_string(),
        None,
        "test-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None, // No static member ID
    ).await.unwrap();
    
    assert_eq!(response.error_code, 0);
    assert_eq!(response.generation_id, 1);
    assert!(!response.member_id.is_empty());
    
    let member_id = response.member_id.clone();
    
    // Sync group (as leader)
    let sync_response = manager.sync_group(
        "test-group".to_string(),
        1,
        member_id.clone(),
        response.member_epoch,
        Some(vec![(member_id.clone(), vec![])]),
    ).await.unwrap();
    
    assert_eq!(sync_response.error_code, 0);
    
    // Heartbeat
    let heartbeat_response = manager.heartbeat(
        "test-group".to_string(),
        member_id.clone(),
        1,
        Some(response.member_epoch),
    ).await.unwrap();
    
    assert_eq!(heartbeat_response.error_code, 0);
    
    // Leave group
    manager.leave_group(
        "test-group".to_string(),
        member_id,
    ).await.unwrap();
}

#[tokio::test]
async fn test_consumer_group_rebalance() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
    metadata_store.init_system_state().await.unwrap();
    
    let manager = Arc::new(GroupManager::new(metadata_store));
    
    // First member joins
    let response1 = manager.join_group(
        "rebalance-group".to_string(),
        None,
        "client-1".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    let member1 = response1.member_id.clone();
    let leader = response1.leader_id.clone();
    
    assert_eq!(member1, leader);
    assert_eq!(response1.members.len(), 1);
    
    // Second member joins
    let response2 = manager.join_group(
        "rebalance-group".to_string(),
        None,
        "client-2".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    let member2 = response2.member_id.clone();
    
    // Leader should still be the first member
    assert_eq!(response2.leader_id, member1);
    assert!(response2.members.is_empty()); // Non-leaders don't get member list
    
    // Clean up
    manager.leave_group("rebalance-group".to_string(), member1).await.unwrap();
    manager.leave_group("rebalance-group".to_string(), member2).await.unwrap();
}

#[tokio::test]
async fn test_consumer_group_expiration() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
    metadata_store.init_system_state().await.unwrap();
    
    let manager = Arc::new(GroupManager::new(metadata_store));
    
    // Start expiration checker
    manager.clone().start_expiration_checker();
    
    // Join with short session timeout
    let response = manager.join_group(
        "expire-group".to_string(),
        None,
        "expire-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_millis(100), // Very short timeout
        Duration::from_secs(30),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    let member_id = response.member_id.clone();
    
    // Wait for expiration
    sleep(Duration::from_millis(2000)).await;
    
    // Heartbeat should fail after expiration
    let heartbeat_result = manager.heartbeat(
        "expire-group".to_string(),
        member_id,
        1,
        Some(response.member_epoch),
    ).await;
    
    // Should get an error since member was expired
    assert!(heartbeat_result.is_err());
}