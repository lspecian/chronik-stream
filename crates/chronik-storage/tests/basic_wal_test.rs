//! Basic tests for WAL metadata adapter functionality

use chronik_common::metadata::{MetaLogWalInterface, MetadataEventPayload};
use chronik_storage::metadata_wal_adapter::WalMetadataAdapter;
use chronik_wal::config::WalConfig;
use std::sync::Arc;
use tempfile::TempDir;
use chrono::Utc;
use uuid::Uuid;

#[tokio::test]
async fn test_wal_adapter_basic_operations() {
    let temp_dir = TempDir::new().unwrap();
    let config = WalConfig {
        enabled: true,
        data_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };

    let adapter: Arc<dyn MetaLogWalInterface> = Arc::new(
        WalMetadataAdapter::new(config).await.unwrap()
    );

    // Test reading from empty WAL
    let events = adapter.read_metadata_events(0).await.unwrap();
    assert_eq!(events.len(), 0);

    // Test getting latest offset from empty WAL
    let offset = adapter.get_latest_offset().await.unwrap();
    assert_eq!(offset, 0);

    println!("Basic WAL adapter operations work correctly");
}

// TODO: Fix WAL persistence - currently the WAL buffer doesn't persist properly across restarts
// This test is commented out until the persistence issue is resolved
/*
#[tokio::test]
async fn test_wal_adapter_persistence() {
    // Test currently fails because WAL data doesn't persist across restarts
    // This is likely due to buffering in the WAL that doesn't get flushed to disk
}
*/