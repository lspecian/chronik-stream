//! Listener setup for Kafka, Search API, and Admin API
//!
//! Extracted from `run_cluster_mode()` to reduce complexity.
//! Handles TCP listener startup for all cluster services.

use anyhow::Result;
use std::sync::Arc;
use tracing::info;
use tokio::task::JoinHandle;

use crate::integrated_server::IntegratedKafkaServer;
use super::config::ClusterInitConfig;

/// Start Kafka protocol listener
///
/// Starts the main Kafka protocol handler on the configured bind address.
/// This runs BEFORE leader election to avoid cold-start deadlock.
/// Complexity: < 15 (spawn task with delay)
pub async fn start_kafka_listener(
    server: Arc<IntegratedKafkaServer>,
    kafka_bind_addr: String,
) -> Result<JoinHandle<Result<()>>> {
    info!("Starting Kafka protocol listener before leader election...");
    info!("  (WalReceiver was started in Stage 15 of IntegratedKafkaServerBuilder::build())");

    // Log address before moving into async block
    info!("✓ Kafka protocol listener starting on {}", kafka_bind_addr);

    // Start Kafka protocol listener on ALL nodes (not just leader)
    let server_task = tokio::spawn(async move {
        server.run(&kafka_bind_addr).await
    });

    // Small delay to ensure TCP listener is bound before proceeding
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    Ok(server_task)
}

/// Start Admin API HTTP server
///
/// Starts the admin REST API for cluster management operations.
/// Complexity: < 10 (simple API server startup)
///
/// v2.2.22: Added isr_tracker parameter for accurate ISR queries
/// Phase 5: Added schema_registry parameter for Schema Registry API
pub async fn start_admin_api(
    catalog: Arc<crate::cluster_catalog::ClusterCatalog>,
    metadata_store: Arc<dyn chronik_common::metadata::traits::MetadataStore>,
    node_id: u64,
    bind: &str,
    isr_tracker: Option<Arc<crate::isr_tracker::IsrTracker>>,
    schema_registry: Arc<crate::schema_registry::SchemaRegistry>,
) -> Result<()> {
    use crate::admin_api;

    let admin_port = 10000 + (node_id as u16);
    info!("Starting Admin API + Schema Registry on port {}", admin_port);

    let admin_api_key = std::env::var("CHRONIK_ADMIN_API_KEY").ok();
    admin_api::start_admin_api(Some(catalog), metadata_store, admin_port, admin_api_key, isr_tracker, schema_registry).await?;

    info!("✓ Admin API available at http://{}:{}/admin", bind, admin_port);
    info!("✓ Schema Registry available at http://{}:{}/subjects", bind, admin_port);
    Ok(())
}

/// Log all running TCP listeners
///
/// Outputs status summary of all active listeners.
/// Complexity: < 5 (logging only)
pub fn log_listener_status(
    init_config: &ClusterInitConfig,
) {
    info!("✓ All TCP listeners now running:");
    info!("  - Raft gRPC: {}", init_config.raft_bind_addr);
    info!("  - Kafka protocol: {}", init_config.kafka_bind_addr);
    info!("  - WAL replication: {} (background task)", init_config.wal_bind_addr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_listener_status() {
        use chronik_config::ClusterConfig;
        use std::path::PathBuf;

        let cluster_config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "./data".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![],
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        let init_config = ClusterInitConfig {
            node_id: 1,
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
            data_dir: PathBuf::from("./data"),
            object_store_config: None,
            replication_factor: 3,
            kafka_bind_addr: "0.0.0.0:9092".to_string(),
            wal_bind_addr: "0.0.0.0:9291".to_string(),
            raft_bind_addr: "0.0.0.0:5001".to_string(),
            raft_peers: vec![],
            self_raft_addr: "localhost:5001".to_string(),
            cluster_config,
        };

        // Just test that this doesn't panic
        log_listener_status(&init_config);
    }
}
