//! Broker registration.
//!
//! Every node, once its catalog has caught up, registers every broker listed
//! in its cluster config. Registration is a catalog command (forwarded to the
//! catalog leader, idempotent), so it does not matter which node gets there
//! first, and a node that was down while the others started still finds the
//! full broker list in the catalog when it comes back.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::cluster_catalog::ClusterCatalog;
use super::config::ClusterInitConfig;
use chronik_common::metadata::MetadataStore;

/// Register the configured brokers in the background, retrying until each
/// succeeds.
pub fn register_configured_brokers(
    metadata_store: Arc<dyn MetadataStore>,
    catalog: Arc<ClusterCatalog>,
    init_config: &ClusterInitConfig,
) -> Result<()> {
    let peers: Vec<(u64, String)> =
        init_config.cluster_config.peers.iter().map(|p| (p.id, p.kafka.clone())).collect();
    tokio::spawn(async move {
        while !catalog.wait_caught_up(Duration::from_secs(30)).await {
            info!("Broker registration waiting for the catalog to catch up");
        }
        for (id, kafka) in peers {
            let (host, port) = parse_kafka_address(&kafka);
            let mut backoff = Duration::from_millis(200);
            loop {
                match metadata_store.register_broker(create_broker_metadata(id, host.clone(), port)).await {
                    Ok(()) => {
                        info!("Broker {} ({}) registered in the catalog", id, kafka);
                        break;
                    }
                    Err(e) => {
                        warn!("Registering broker {} ({}) failed, retrying: {}", id, kafka, e);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                    }
                }
            }
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
