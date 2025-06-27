//! Integration tests for backend service connections
//!
//! Tests the admin API integration with ingest and search backend services.

use chronik_controller::{
    AdminService, MetadataSyncManager, ControllerMetadataStore,
    ControllerState, TopicConfig, BrokerInfo,
    backend_client::{BackendServiceManager, ClusterStats},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tempfile::TempDir;

/// Test backend service manager initialization
#[tokio::test]
async fn test_backend_manager_creation() {
    let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
    let backend_manager = BackendServiceManager::new(cluster_state);
    
    // Should start with no connections
    let stats = backend_manager.get_cluster_stats().await.unwrap();
    assert_eq!(stats.active_ingest_nodes, 0);
    assert_eq!(stats.active_search_nodes, 0);
}

/// Test admin service with backend integration
#[tokio::test]
async fn test_admin_service_with_backend() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(ControllerMetadataStore::new(temp_dir.path()).unwrap());
    
    let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
    
    // Create backend manager
    let backend_manager = Arc::new(BackendServiceManager::new(cluster_state.clone()));
    
    // Create sync manager with backend
    let sync_manager = Arc::new(MetadataSyncManager::new(
        metadata_store.clone(),
        cluster_state.clone(),
        Duration::from_secs(60),
    ).with_backend_manager(backend_manager.clone()));
    
    // Create admin service with backend
    let admin_service = Arc::new(AdminService::new(
        metadata_store,
        cluster_state,
        sync_manager,
    ).with_backend_manager(backend_manager));
    
    // Create router - should include backend endpoints
    let _router = admin_service.router();
}

/// Test metadata update propagation through backend
#[tokio::test]
async fn test_metadata_propagation() {
    let temp_dir = TempDir::new().unwrap();
    let metadata_store = Arc::new(ControllerMetadataStore::new(temp_dir.path()).unwrap());
    
    // Create cluster state with a broker
    let mut state = ControllerState::default();
    state.brokers.insert(1, BrokerInfo {
        id: 1,
        address: "127.0.0.1:9092".parse().unwrap(),
        rack: None,
        status: Some("online".to_string()),
        version: Some("1.0.0".to_string()),
        metadata_version: Some(1),
    });
    
    let cluster_state = Arc::new(RwLock::new(state));
    
    // Create backend manager
    let backend_manager = Arc::new(BackendServiceManager::new(cluster_state.clone()));
    
    // Create sync manager with backend
    let sync_manager = Arc::new(MetadataSyncManager::new(
        metadata_store.clone(),
        cluster_state.clone(),
        Duration::from_secs(60),
    ).with_backend_manager(backend_manager.clone()));
    
    // Test triggering a sync
    let result = sync_manager.trigger_topic_sync().await;
    assert!(result.is_ok());
}

/// Test search index lifecycle
#[tokio::test]
async fn test_search_index_operations() {
    let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
    let backend_manager = BackendServiceManager::new(cluster_state);
    
    // Create a test topic
    let topic = TopicConfig {
        name: "test-topic".to_string(),
        partition_count: 3,
        replication_factor: 1,
        config: HashMap::new(),
        created_at: SystemTime::now(),
        version: 1,
    };
    
    // Try to create search index (will fail without actual search nodes)
    let create_result = backend_manager.create_search_index(&topic).await;
    // We expect this to fail in test environment
    assert!(create_result.is_err());
    
    // Try to delete search index
    let delete_result = backend_manager.delete_search_index(&topic.name).await;
    // This should succeed even if index doesn't exist
    assert!(delete_result.is_ok());
}

/// Test cluster stats aggregation
#[tokio::test]
async fn test_cluster_stats() {
    // Create cluster state with multiple brokers
    let mut state = ControllerState::default();
    
    state.brokers.insert(1, BrokerInfo {
        id: 1,
        address: "127.0.0.1:9092".parse().unwrap(),
        rack: Some("rack1".to_string()),
        status: Some("online".to_string()),
        version: Some("1.0.0".to_string()),
        metadata_version: Some(1),
    });
    
    state.brokers.insert(2, BrokerInfo {
        id: 2,
        address: "127.0.0.1:9093".parse().unwrap(),
        rack: Some("rack2".to_string()),
        status: Some("offline".to_string()),
        version: Some("1.0.0".to_string()),
        metadata_version: Some(1),
    });
    
    let cluster_state = Arc::new(RwLock::new(state));
    let backend_manager = BackendServiceManager::new(cluster_state);
    
    let stats = backend_manager.get_cluster_stats().await.unwrap();
    
    // Should have 2 total ingest nodes (brokers)
    assert_eq!(stats.total_ingest_nodes, 2);
    // But 0 active (not connected in test)
    assert_eq!(stats.active_ingest_nodes, 0);
    
    // Search nodes depend on discovery
    assert_eq!(stats.total_search_nodes, 0);
    assert_eq!(stats.active_search_nodes, 0);
}

/// Test backend manager health checks
#[tokio::test]
async fn test_backend_health_checks() {
    let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
    let backend_manager = BackendServiceManager::new(cluster_state);
    
    // Initialize should start health check loop
    let init_result = backend_manager.initialize().await;
    assert!(init_result.is_ok());
    
    // Give health checks time to run
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Get stats to verify health checks are working
    let stats = backend_manager.get_cluster_stats().await.unwrap();
    
    // In test environment, we expect no active nodes
    assert_eq!(stats.active_ingest_nodes, 0);
    assert_eq!(stats.active_search_nodes, 0);
}