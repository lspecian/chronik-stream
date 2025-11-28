//! Raft cluster setup and initialization
//!
//! Handles Raft cluster bootstrapping, gRPC server startup, and connection pre-warming.
//! Extracted from `run_cluster_mode()` to reduce complexity.

use anyhow::Result;
use std::sync::Arc;
use tracing::info;

use crate::raft_cluster::RaftCluster;
use super::config::ClusterInitConfig;

/// Bootstrap Raft cluster from configuration
///
/// Creates a new RaftCluster instance with the specified peers and data directory.
/// Complexity: < 15 (cluster creation and initialization)
pub async fn bootstrap_raft_cluster(init_config: &ClusterInitConfig) -> Result<Arc<RaftCluster>> {
    info!("Bootstrapping Raft cluster (node_id={}, peers={})...",
        init_config.node_id, init_config.raft_peers.len());

    let raft_cluster = Arc::new(
        RaftCluster::bootstrap(
            init_config.node_id,
            init_config.raft_peers.clone(),
            init_config.data_dir.clone(),
        ).await?
    );

    info!("✓ Raft cluster bootstrapped successfully");
    Ok(raft_cluster)
}

/// Start Raft gRPC server for peer communication
///
/// Starts the gRPC server that handles Raft consensus messages between nodes.
/// Must be called before leader election to enable inter-node communication.
/// Complexity: < 10 (server startup and connection pre-warming)
pub async fn start_raft_grpc_server(
    raft_cluster: Arc<RaftCluster>,
    raft_bind_addr: &str,
) -> Result<()> {
    info!("Starting Raft gRPC server on {}...", raft_bind_addr);

    // Start gRPC server to listen for peer messages
    raft_cluster.clone().start_grpc_server(raft_bind_addr.to_string()).await?;

    info!("✓ Raft gRPC server started successfully");

    // Pre-warm connections AFTER gRPC server starts
    // This prevents chicken-and-egg problem during cluster startup
    // Spawn in background so it doesn't block message loop startup
    info!("Pre-warming Raft connections in background...");
    let cluster_for_prewarm = raft_cluster.clone();
    tokio::spawn(async move {
        cluster_for_prewarm.pre_warm_connections().await;
    });

    info!("Raft message loop will be started by IntegratedKafkaServer");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_bootstrap_raft_cluster() {
        let temp_dir = TempDir::new().unwrap();
        let config = ClusterInitConfig {
            node_id: 1,
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
            data_dir: temp_dir.path().to_path_buf(),
            object_store_config: None,
            replication_factor: 3,
            kafka_bind_addr: "0.0.0.0:9092".to_string(),
            wal_bind_addr: "0.0.0.0:9291".to_string(),
            raft_bind_addr: "0.0.0.0:5001".to_string(),
            raft_peers: vec![
                (2, "localhost:5002".to_string()),
                (3, "localhost:5003".to_string()),
            ],
            cluster_config: Default::default(),
        };

        let cluster = bootstrap_raft_cluster(&config).await.unwrap();
        assert_eq!(cluster.node_id(), 1);
    }
}
