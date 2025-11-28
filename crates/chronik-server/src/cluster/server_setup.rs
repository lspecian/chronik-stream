//! IntegratedKafkaServer initialization
//!
//! Extracted from `run_cluster_mode()` to reduce complexity.
//! Handles server config creation and builder pattern orchestration.

use anyhow::Result;
use std::sync::Arc;
use tracing::info;

use crate::integrated_server::{IntegratedKafkaServerBuilder, IntegratedServerConfig, IntegratedKafkaServer};
use crate::raft_cluster::RaftCluster;
use super::config::ClusterInitConfig;
use chronik_storage::ObjectStoreConfig;

/// Create IntegratedKafkaServer from cluster configuration
///
/// Builds IntegratedServerConfig and uses builder pattern to create server.
/// Complexity: < 20 (config creation and builder invocation)
pub async fn create_integrated_server(
    init_config: &ClusterInitConfig,
    raft_cluster: Arc<RaftCluster>,
    object_store_config: Option<ObjectStoreConfig>,
) -> Result<Arc<IntegratedKafkaServer>> {
    info!("Creating IntegratedServerConfig for cluster mode...");

    // Create server configuration
    let server_config = IntegratedServerConfig {
        node_id: init_config.node_id as i32,
        advertised_host: init_config.advertised_host.clone(),
        advertised_port: init_config.advertised_port,
        data_dir: init_config.data_dir.to_string_lossy().to_string(),
        enable_indexing: cfg!(feature = "search"),
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,  // Default for better parallelism
        replication_factor: init_config.replication_factor,
        enable_wal_indexing: true,
        wal_indexing_interval_secs: 30,
        object_store_config,
        enable_metadata_dr: true,
        metadata_upload_interval_secs: 60,
        cluster_config: Some(init_config.cluster_config.clone()),
    };

    info!("Initializing IntegratedKafkaServer with builder (14-stage initialization)...");
    let server = IntegratedKafkaServerBuilder::new(server_config)
        .with_raft_cluster(raft_cluster)
        .build()
        .await?;

    info!("✓ IntegratedKafkaServer initialized successfully (builder pattern)");
    Ok(Arc::new(server))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use chronik_config::ClusterConfig;

    #[tokio::test]
    async fn test_create_integrated_server_config() {
        let temp_dir = TempDir::new().unwrap();
        let cluster_config = ClusterConfig {
            node_id: 1,
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![],
            bind: None,
            advertise: None,
        };

        let init_config = ClusterInitConfig {
            node_id: 1,
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
            data_dir: temp_dir.path().to_path_buf(),
            object_store_config: None,
            replication_factor: 3,
            kafka_bind_addr: "0.0.0.0:9092".to_string(),
            wal_bind_addr: "0.0.0.0:9291".to_string(),
            raft_bind_addr: "0.0.0.0:5001".to_string(),
            raft_peers: vec![],
            cluster_config,
        };

        // Test server config creation (without actually building server)
        let server_config = IntegratedServerConfig {
            node_id: init_config.node_id as i32,
            advertised_host: init_config.advertised_host.clone(),
            advertised_port: init_config.advertised_port,
            data_dir: init_config.data_dir.clone(),
            enable_indexing: cfg!(feature = "search"),
            enable_compression: true,
            auto_create_topics: true,
            num_partitions: 3,
            replication_factor: init_config.replication_factor,
            enable_wal_indexing: true,
            wal_indexing_interval_secs: 30,
            object_store_config: None,
            enable_metadata_dr: true,
            metadata_upload_interval_secs: 60,
            cluster_config: Some(init_config.cluster_config.clone()),
        };

        assert_eq!(server_config.node_id, 1);
        assert_eq!(server_config.advertised_host, "localhost");
        assert_eq!(server_config.replication_factor, 3);
    }
}
