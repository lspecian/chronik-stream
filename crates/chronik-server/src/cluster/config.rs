//! Cluster configuration parsing and validation
//!
//! Extracted from `run_cluster_mode()` to reduce complexity.
//! Handles address parsing, validation, and ClusterInitConfig creation.

use anyhow::{Result, Context};
use std::path::PathBuf;
use chronik_storage::ObjectStoreConfig;
use chronik_config::ClusterConfig;

use crate::Cli;

/// Cluster initialization configuration
///
/// Aggregates all configuration needed for cluster mode startup.
/// Created from CLI arguments and cluster config file.
#[derive(Clone, Debug)]
pub struct ClusterInitConfig {
    pub node_id: u64,
    pub advertised_host: String,
    pub advertised_port: i32,
    pub data_dir: PathBuf,
    pub object_store_config: Option<ObjectStoreConfig>,
    pub replication_factor: u32,
    pub kafka_bind_addr: String,
    pub wal_bind_addr: String,
    pub raft_bind_addr: String,
    pub raft_peers: Vec<(u64, String)>,
    pub cluster_config: ClusterConfig,
}

impl ClusterInitConfig {
    /// Create configuration from CLI and cluster config
    ///
    /// Complexity: < 20 (configuration parsing and validation)
    pub fn from_cli_and_config(
        _cli: &Cli,
        config: ClusterConfig,
        bind: &str,
        advertise: Option<&str>,
    ) -> Result<Self> {
        // Get this node's configuration
        let this_node = config.this_node()
            .ok_or_else(|| anyhow::anyhow!("Node ID {} not found in cluster config", config.node_id))?;

        // Parse advertise address (use cluster config or CLI override)
        let (advertised_host, advertised_port) = if let Some(adv) = advertise {
            parse_advertise_addr(Some(adv), bind, 9092)?
        } else {
            // Extract from cluster config
            parse_kafka_addr(&this_node.kafka)?
        };

        // Parse object store configuration from environment
        let object_store_config = crate::parse_object_store_config_from_env();

        // Extract Raft peers
        let raft_peers: Vec<(u64, String)> = config.peer_nodes()
            .iter()
            .map(|peer| (peer.id, peer.raft.clone()))
            .collect();

        // Parse bind addresses from cluster config
        let kafka_bind_addr = config.bind.as_ref()
            .map(|b| b.kafka.clone())
            .unwrap_or_else(|| this_node.kafka.clone());

        let wal_bind_addr = config.bind.as_ref()
            .map(|b| b.wal.clone())
            .unwrap_or_else(|| this_node.wal.clone());

        let raft_bind_addr = config.bind.as_ref()
            .map(|b| b.raft.clone())
            .or_else(|| config.advertise.as_ref().map(|a| a.raft.clone()))
            .unwrap_or_else(|| "0.0.0.0:5001".to_string());

        Ok(Self {
            node_id: config.node_id,
            advertised_host,
            advertised_port,
            data_dir: PathBuf::from(&config.data_dir),
            object_store_config,
            replication_factor: config.replication_factor as u32,
            kafka_bind_addr,
            wal_bind_addr,
            raft_bind_addr,
            raft_peers,
            cluster_config: config,
        })
    }
}

/// Parse Kafka address into host and port
///
/// Complexity: < 5 (simple address parsing)
fn parse_kafka_addr(addr: &str) -> Result<(String, i32)> {
    if let Some(colon_pos) = addr.rfind(':') {
        let host = addr[..colon_pos].to_string();
        let port_str = &addr[colon_pos + 1..];
        let port = port_str.parse::<u16>()
            .context(format!("Invalid port in address '{}'", addr))?;
        Ok((host, port as i32))
    } else {
        Err(anyhow::anyhow!("Address '{}' must be in format 'host:port'", addr))
    }
}

/// Parse advertise address from CLI or bind address
///
/// Complexity: < 10 (simple address parsing)
fn parse_advertise_addr(
    advertise: Option<&str>,
    bind: &str,
    default_port: i32,
) -> Result<(String, i32)> {
    if let Some(adv) = advertise {
        // Parse host:port format
        if let Some(colon_pos) = adv.rfind(':') {
            let host = adv[..colon_pos].to_string();
            let port = adv[colon_pos+1..].parse::<i32>()
                .context("Invalid port in advertise address")?;
            Ok((host, port))
        } else {
            // Just host, use default port
            Ok((adv.to_string(), default_port))
        }
    } else {
        // No advertise, derive from bind
        let host = if bind.starts_with("0.0.0.0") {
            "localhost".to_string()
        } else {
            bind.split(':').next().unwrap_or("localhost").to_string()
        };
        Ok((host, default_port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_advertise_addr_with_port() {
        let (host, port) = parse_advertise_addr(Some("example.com:9093"), "0.0.0.0:9092", 9092).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 9093);
    }

    #[test]
    fn test_parse_advertise_addr_without_port() {
        let (host, port) = parse_advertise_addr(Some("example.com"), "0.0.0.0:9092", 9092).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 9092);
    }

    #[test]
    fn test_parse_advertise_addr_from_bind() {
        let (host, port) = parse_advertise_addr(None, "0.0.0.0:9092", 9092).unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 9092);
    }
}
