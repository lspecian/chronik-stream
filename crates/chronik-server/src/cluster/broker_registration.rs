//! Broker registration and partition assignment
//!
//! Extracted from `run_cluster_mode()` to reduce complexity.
//! Handles leader election wait, broker registration, and partition assignment.

use anyhow::Result;
use std::sync::Arc;
use tracing::{info, error, warn};

use crate::integrated_server::IntegratedKafkaServer;
use crate::raft_cluster::RaftCluster;
use super::config::ClusterInitConfig;

/// Wait for Raft leader election
///
/// Polls Raft cluster for leader election with timeout.
/// Complexity: < 15 (polling loop with timeout)
pub async fn wait_for_leader_election(
    raft_cluster: Arc<RaftCluster>,
    node_id: u64,
) -> Result<bool> {
    info!("Waiting for Raft leader election (max 10s)...");

    let mut election_attempts = 0;
    let max_election_wait = 100; // 100 * 100ms = 10 seconds
    let mut is_leader = false;

    while election_attempts < max_election_wait {
        let (has_leader, leader_id, state) = raft_cluster.is_leader_ready().await;
        if has_leader {
            is_leader = leader_id == node_id;
            info!(
                "✓ Raft leader elected: leader_id={}, this_node={}, is_leader={}, state={}",
                leader_id, node_id, is_leader, state
            );
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        election_attempts += 1;
    }

    Ok(is_leader)
}

/// Register all brokers from cluster configuration
///
/// Leader node registers all peers as brokers in metadata store.
/// Runs in background task to avoid blocking message loop.
/// Complexity: < 20 (broker registration loop)
pub async fn register_brokers_and_assign_partitions(
    server: Arc<IntegratedKafkaServer>,
    raft_cluster: Arc<RaftCluster>,
    init_config: &ClusterInitConfig,
) -> Result<()> {
    // Wait for leader election
    let is_leader = wait_for_leader_election(raft_cluster, init_config.node_id).await?;

    if !is_leader {
        info!("This node is a follower - skipping broker registration (will receive via Raft replication)");
        return Ok(());
    }

    // Leader registers ALL brokers from config
    info!("This node is the Raft leader - spawning broker registration task");
    let metadata_store = server.metadata_store();
    let peers_clone = init_config.cluster_config.peers.clone();

    // Signal when broker registration completes
    let (broker_reg_tx, broker_reg_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        info!("Background task: Registering {} brokers from config", peers_clone.len());

        for peer in &peers_clone {
            // Parse actual Kafka address from config
            let (host, port) = parse_kafka_address(&peer.kafka);

            let broker_metadata = create_broker_metadata(peer.id, host, port);

            match metadata_store.register_broker(broker_metadata.clone()).await {
                Ok(_) => {
                    info!("✅ Successfully registered broker {} ({}) via Raft", peer.id, peer.kafka);
                }
                Err(e) => {
                    error!("❌ Failed to register broker {} ({}): {:?}", peer.id, peer.kafka, e);
                }
            }
        }

        info!("✓ Broker registration task completed");
        let _ = broker_reg_tx.send(()); // Signal completion
    });

    info!("Broker registration task spawned - will complete in background");

    // Wait for broker registration, then assign partitions
    let server_clone = server.clone();
    let config_clone = init_config.cluster_config.clone();
    tokio::spawn(async move {
        if broker_reg_rx.await.is_ok() {
            info!("Broker registration completed - starting partition assignment");

            if let Err(e) = server_clone.assign_existing_partitions(&config_clone).await {
                error!("❌ Failed to assign existing partitions: {:?}", e);
            } else {
                info!("✅ Partition assignment completed successfully");
            }
        } else {
            warn!("⚠️ Broker registration task dropped before completing - skipping partition assignment");
        }
    });

    Ok(())
}

/// Parse Kafka address into host and port
///
/// Complexity: < 10 (address parsing)
fn parse_kafka_address(kafka_addr: &str) -> (String, i32) {
    if let Some(colon_pos) = kafka_addr.rfind(':') {
        let host = kafka_addr[..colon_pos].to_string();
        let port = kafka_addr[colon_pos + 1..]
            .parse::<i32>()
            .unwrap_or_else(|_| {
                warn!("Failed to parse Kafka port from '{}', using default 9092", kafka_addr);
                9092
            });
        (host, port)
    } else {
        warn!("Invalid Kafka address format '{}', using defaults", kafka_addr);
        (kafka_addr.to_string(), 9092)
    }
}

/// Create broker metadata from parsed values
///
/// Complexity: < 5 (struct creation)
fn create_broker_metadata(
    broker_id: u64,
    host: String,
    port: i32,
) -> chronik_common::metadata::traits::BrokerMetadata {
    use chronik_common::metadata::traits::{BrokerMetadata, BrokerStatus};

    BrokerMetadata {
        broker_id: broker_id as i32,
        host,
        port,
        rack: None,
        status: BrokerStatus::Online,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kafka_address_with_port() {
        let (host, port) = parse_kafka_address("localhost:9092");
        assert_eq!(host, "localhost");
        assert_eq!(port, 9092);
    }

    #[test]
    fn test_parse_kafka_address_without_port() {
        let (host, port) = parse_kafka_address("example.com");
        assert_eq!(host, "example.com");
        assert_eq!(port, 9092);
    }

    #[test]
    fn test_create_broker_metadata() {
        let metadata = create_broker_metadata(1, "localhost".to_string(), 9092);
        assert_eq!(metadata.broker_id, 1);
        assert_eq!(metadata.host, "localhost");
        assert_eq!(metadata.port, 9092);
    }
}
