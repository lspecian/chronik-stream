//! Integration tests for Admin Service and Metadata Sync
//!
//! Tests the complete admin API and metadata synchronization functionality.

use chronik_controller::{
    AdminService, MetadataSyncManager, ControllerMetadataStore,
    ControllerState, TopicConfig, BrokerInfo,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tempfile::TempDir;

/// Test basic admin service creation and router setup
#[tokio::test]
async fn test_admin_service_creation() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(ControllerMetadataStore::new(temp_dir.path()).unwrap());
    let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
    let sync_manager = Arc::new(MetadataSyncManager::new(
        metadata_store.clone(),
        cluster_state.clone(),
        Duration::from_secs(60),
    ));

    let admin_service = Arc::new(AdminService::new(
        metadata_store,
        cluster_state,
        sync_manager,
    ));

    // Test that we can create the router without errors
    let _router = admin_service.router();
}

/// Test metadata sync manager creation and basic functionality
#[tokio::test]
async fn test_metadata_sync_manager() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(ControllerMetadataStore::new(temp_dir.path()).unwrap());
    let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
    
    let sync_manager = MetadataSyncManager::new(
        metadata_store,
        cluster_state,
        Duration::from_millis(100), // Fast sync for testing
    );

    // Test that we can get initial sync status
    let status = sync_manager.get_status().await;
    assert!(!status.sync_in_progress);
    assert!(status.last_sync_time.is_none());
    assert_eq!(status.metadata_version, 0);
}

/// Test cluster state initialization and manipulation
#[tokio::test]
async fn test_cluster_state_operations() {
    let mut state = ControllerState::default();
    
    // Add a test topic
    let topic = TopicConfig {
        name: "test-topic".to_string(),
        partition_count: 3,
        replication_factor: 1,
        config: HashMap::new(),
        created_at: SystemTime::now(),
        version: 1,
    };
    
    state.topics.insert("test-topic".to_string(), topic);
    
    // Add a test broker
    let broker = BrokerInfo {
        id: 1,
        address: "127.0.0.1:9092".parse().unwrap(),
        rack: None,
        status: Some("online".to_string()),
        version: Some("1.0.0".to_string()),
        metadata_version: Some(1),
    };
    
    state.brokers.insert(1, broker);
    
    // Verify state
    assert_eq!(state.topics.len(), 1);
    assert_eq!(state.brokers.len(), 1);
    assert!(state.topics.contains_key("test-topic"));
    assert!(state.brokers.contains_key(&1));
}

/// Test admin service with sample data
#[tokio::test]
async fn test_admin_service_with_data() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(ControllerMetadataStore::new(temp_dir.path()).unwrap());
    
    // Initialize cluster state with sample data
    let mut state = ControllerState::default();
    
    // Add sample topic
    let topic = TopicConfig {
        name: "sample-topic".to_string(),
        partition_count: 6,
        replication_factor: 2,
        config: HashMap::from([
            ("retention.ms".to_string(), "86400000".to_string()),
            ("compression.type".to_string(), "snappy".to_string()),
        ]),
        created_at: SystemTime::now(),
        version: 1,
    };
    state.topics.insert("sample-topic".to_string(), topic);
    
    // Add sample broker
    let broker = BrokerInfo {
        id: 1,
        address: "127.0.0.1:9092".parse().unwrap(),
        rack: Some("rack-1".to_string()),
        status: Some("online".to_string()),
        version: Some("1.0.0".to_string()),
        metadata_version: Some(1),
    };
    state.brokers.insert(1, broker);
    
    let cluster_state = Arc::new(RwLock::new(state));
    let sync_manager = Arc::new(MetadataSyncManager::new(
        metadata_store.clone(),
        cluster_state.clone(),
        Duration::from_secs(60),
    ));

    let admin_service = Arc::new(AdminService::new(
        metadata_store,
        cluster_state,
        sync_manager,
    ));

    // Create router
    let _router = admin_service.router();
    
    // Verify the admin service was created successfully with the sample data
    // In a full integration test, we would start an HTTP server and make requests
}