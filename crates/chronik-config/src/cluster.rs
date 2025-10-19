//! Cluster configuration for static peer discovery.
//!
//! This module defines the configuration structure for Chronik Raft clustering,
//! including node identity, peer information, and replication settings.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};
use validator::Validate;

use crate::{ConfigError, Result};

/// Cluster configuration for static peer discovery
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ClusterConfig {
    /// Whether clustering is enabled
    pub enabled: bool,

    /// This node's unique identifier
    #[validate(custom = "validate_node_id")]
    pub node_id: u64,

    /// Number of replicas for each partition
    #[validate(range(min = 1, max = 100))]
    pub replication_factor: usize,

    /// Minimum number of in-sync replicas required for writes
    #[validate(range(min = 1, max = 100))]
    pub min_insync_replicas: usize,

    /// List of all peers in the cluster (including this node)
    #[validate(length(min = 1))]
    pub peers: Vec<NodeConfig>,

    /// Gossip configuration (optional, for automatic bootstrap)
    pub gossip: Option<GossipConfig>,
}

/// Gossip configuration for automatic cluster discovery and bootstrap
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct GossipConfig {
    /// Gossip bind address (e.g., "0.0.0.0:7946")
    #[validate(custom = "validate_addr")]
    pub bind_addr: String,

    /// Seed nodes for initial gossip (comma-separated addresses)
    /// Example: "10.0.1.10:7946,10.0.1.11:7946"
    pub seed_nodes: Vec<String>,

    /// Expected number of server nodes for bootstrap quorum
    #[validate(range(min = 1, max = 100))]
    pub bootstrap_expect: usize,
}

/// Configuration for a single node in the cluster
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NodeConfig {
    /// Unique node identifier
    #[validate(custom = "validate_node_id")]
    pub id: u64,

    /// Kafka API address (hostname:port)
    #[validate(custom = "validate_addr")]
    pub addr: String,

    /// Raft gRPC port for consensus communication
    #[validate(range(min = 1024, max = 65535))]
    pub raft_port: u16,
}

impl ClusterConfig {
    /// Parse cluster configuration from environment variables
    ///
    /// Supported variables:
    /// - CHRONIK_CLUSTER_ENABLED: "true" or "false"
    /// - CHRONIK_NODE_ID: Node ID (e.g., "1")
    /// - CHRONIK_REPLICATION_FACTOR: Replication factor (e.g., "3")
    /// - CHRONIK_MIN_INSYNC_REPLICAS: Min in-sync replicas (e.g., "2")
    /// - CHRONIK_CLUSTER_PEERS: Comma-separated list of "host:kafka_port:raft_port"
    ///   Example: "10.0.1.10:9092:9093,10.0.1.11:9092:9093,10.0.1.12:9092:9093"
    pub fn from_env() -> Result<Option<Self>> {
        // Check if CHRONIK_CLUSTER_PEERS is set (auto-enable clustering)
        let has_peers = std::env::var("CHRONIK_CLUSTER_PEERS").is_ok();

        let enabled = std::env::var("CHRONIK_CLUSTER_ENABLED")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(has_peers);  // Auto-enable if peers are configured

        if !enabled {
            return Ok(None);
        }

        let node_id = std::env::var("CHRONIK_NODE_ID")
            .map_err(|_| ConfigError::Parse("CHRONIK_NODE_ID not set".to_string()))?
            .parse::<u64>()
            .map_err(|e| ConfigError::Parse(format!("Invalid CHRONIK_NODE_ID: {}", e)))?;

        let replication_factor = std::env::var("CHRONIK_REPLICATION_FACTOR")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3);

        let min_insync_replicas = std::env::var("CHRONIK_MIN_INSYNC_REPLICAS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2);

        let peers = Self::parse_peers_from_env()?;

        let config = ClusterConfig {
            enabled,
            node_id,
            replication_factor,
            min_insync_replicas,
            peers,
            gossip: None, // TODO: Parse from env if needed
        };

        config.validate_config()?;
        Ok(Some(config))
    }

    /// Parse peers from CHRONIK_CLUSTER_PEERS environment variable
    fn parse_peers_from_env() -> Result<Vec<NodeConfig>> {
        let peers_str = std::env::var("CHRONIK_CLUSTER_PEERS")
            .map_err(|_| ConfigError::Parse("CHRONIK_CLUSTER_PEERS not set".to_string()))?;

        let mut peers = Vec::new();
        for (idx, peer_str) in peers_str.split(',').enumerate() {
            let parts: Vec<&str> = peer_str.split(':').collect();
            if parts.len() != 3 {
                return Err(ConfigError::Parse(format!(
                    "Invalid peer format '{}': expected 'host:kafka_port:raft_port'",
                    peer_str
                )));
            }

            let host = parts[0].to_string();
            let kafka_port = parts[1].parse::<u16>().map_err(|e| {
                ConfigError::Parse(format!("Invalid Kafka port '{}': {}", parts[1], e))
            })?;
            let raft_port = parts[2].parse::<u16>().map_err(|e| {
                ConfigError::Parse(format!("Invalid Raft port '{}': {}", parts[2], e))
            })?;

            peers.push(NodeConfig {
                id: (idx + 1) as u64, // Auto-assign IDs based on order
                addr: format!("{}:{}", host, kafka_port),
                raft_port,
            });
        }

        Ok(peers)
    }

    /// Validate the entire cluster configuration
    pub fn validate_config(&self) -> Result<()> {
        // Run validator validation first
        self.validate().map_err(|e| {
            ConfigError::Validation(crate::validation::ValidationError::Custom(
                e.to_string(),
            ))
        })?;

        // Additional custom validations
        self.validate_node_exists()?;
        self.validate_unique_node_ids()?;
        self.validate_unique_addresses()?;
        self.validate_replication_settings()?;

        Ok(())
    }

    /// Validate that node_id exists in peers list
    fn validate_node_exists(&self) -> Result<()> {
        if !self.peers.iter().any(|p| p.id == self.node_id) {
            return Err(ConfigError::Validation(
                crate::validation::ValidationError::Custom(format!(
                    "node_id {} not found in peers list",
                    self.node_id
                )),
            ));
        }
        Ok(())
    }

    /// Validate no duplicate node IDs
    fn validate_unique_node_ids(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for peer in &self.peers {
            if !seen.insert(peer.id) {
                return Err(ConfigError::Validation(
                    crate::validation::ValidationError::Custom(format!(
                        "Duplicate node ID: {}",
                        peer.id
                    )),
                ));
            }
        }
        Ok(())
    }

    /// Validate no duplicate addresses
    fn validate_unique_addresses(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for peer in &self.peers {
            let key = format!("{}:{}", peer.addr, peer.raft_port);
            if !seen.insert(key.clone()) {
                return Err(ConfigError::Validation(
                    crate::validation::ValidationError::Custom(format!(
                        "Duplicate address: {}",
                        key
                    )),
                ));
            }
        }
        Ok(())
    }

    /// Validate replication settings
    fn validate_replication_settings(&self) -> Result<()> {
        if self.replication_factor > self.peers.len() {
            return Err(ConfigError::Validation(
                crate::validation::ValidationError::Custom(format!(
                    "replication_factor ({}) cannot exceed peer count ({})",
                    self.replication_factor,
                    self.peers.len()
                )),
            ));
        }

        if self.min_insync_replicas > self.replication_factor {
            return Err(ConfigError::Validation(
                crate::validation::ValidationError::Custom(format!(
                    "min_insync_replicas ({}) cannot exceed replication_factor ({})",
                    self.min_insync_replicas,
                    self.replication_factor
                )),
            ));
        }

        Ok(())
    }

    /// Get this node's configuration
    pub fn this_node(&self) -> Option<&NodeConfig> {
        self.peers.iter().find(|p| p.id == self.node_id)
    }

    /// Get peer nodes (excluding this node)
    pub fn peer_nodes(&self) -> Vec<&NodeConfig> {
        self.peers.iter().filter(|p| p.id != self.node_id).collect()
    }

    /// Get all peer Raft addresses (for initial cluster setup)
    pub fn raft_peer_addrs(&self) -> Vec<String> {
        self.peers
            .iter()
            .map(|p| p.raft_addr())
            .collect()
    }
}

impl NodeConfig {
    /// Get the full Raft gRPC address
    pub fn raft_addr(&self) -> String {
        // Extract hostname from kafka addr
        let hostname = self.addr.split(':').next().unwrap_or("localhost");
        format!("{}:{}", hostname, self.raft_port)
    }

    /// Parse the Kafka address as a SocketAddr
    pub fn kafka_socket_addr(&self) -> Result<SocketAddr> {
        self.addr
            .to_socket_addrs()
            .map_err(|e| ConfigError::Parse(format!("Invalid address '{}': {}", self.addr, e)))?
            .next()
            .ok_or_else(|| ConfigError::Parse(format!("No socket address for '{}'", self.addr)))
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_id: 1,
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![],
            gossip: None,
        }
    }
}

/// Custom validator for node_id (must be non-zero)
fn validate_node_id(node_id: u64) -> std::result::Result<(), validator::ValidationError> {
    if node_id == 0 {
        return Err(validator::ValidationError::new("node_id must be non-zero"));
    }
    Ok(())
}

/// Custom validator for address format
fn validate_addr(addr: &str) -> std::result::Result<(), validator::ValidationError> {
    // Basic validation: must contain a colon
    if !addr.contains(':') {
        return Err(validator::ValidationError::new(
            "address must be in format 'hostname:port'",
        ));
    }

    // Try to parse as socket address (this validates format)
    if addr.to_socket_addrs().is_err() {
        return Err(validator::ValidationError::new("invalid socket address"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_peers() -> Vec<NodeConfig> {
        vec![
            NodeConfig {
                id: 1,
                addr: "10.0.1.10:9092".to_string(),
                raft_port: 9093,
            },
            NodeConfig {
                id: 2,
                addr: "10.0.1.11:9092".to_string(),
                raft_port: 9093,
            },
            NodeConfig {
                id: 3,
                addr: "10.0.1.12:9092".to_string(),
                raft_port: 9093,
            },
        ]
    }

    #[test]
    fn test_valid_cluster_config() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            gossip: None,
        };

        assert!(config.validate_config().is_ok());
    }

    #[test]
    fn test_node_id_not_in_peers() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 99, // Not in peers
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            gossip: None,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_duplicate_node_ids() {
        let peers = vec![
            NodeConfig {
                id: 1,
                addr: "10.0.1.10:9092".to_string(),
                raft_port: 9093,
            },
            NodeConfig {
                id: 1, // Duplicate!
                addr: "10.0.1.11:9092".to_string(),
                raft_port: 9093,
            },
        ];

        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            replication_factor: 2,
            min_insync_replicas: 1,
            peers,
            gossip: None,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_duplicate_addresses() {
        let peers = vec![
            NodeConfig {
                id: 1,
                addr: "10.0.1.10:9092".to_string(),
                raft_port: 9093,
            },
            NodeConfig {
                id: 2,
                addr: "10.0.1.10:9092".to_string(), // Duplicate address!
                raft_port: 9093,
            },
        ];

        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            replication_factor: 2,
            min_insync_replicas: 1,
            peers,
            gossip: None,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_replication_factor_exceeds_peers() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            replication_factor: 5, // More than peer count
            min_insync_replicas: 2,
            peers: create_test_peers(),
            gossip: None,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_min_insync_exceeds_replication_factor() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            replication_factor: 2,
            min_insync_replicas: 3, // More than replication factor
            peers: create_test_peers(),
            gossip: None,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_this_node() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            gossip: None,
        };

        let this_node = config.this_node().unwrap();
        assert_eq!(this_node.id, 2);
        assert_eq!(this_node.addr, "10.0.1.11:9092");
    }

    #[test]
    fn test_peer_nodes() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            gossip: None,
        };

        let peers = config.peer_nodes();
        assert_eq!(peers.len(), 2);
        assert!(peers.iter().all(|p| p.id != 2));
    }

    #[test]
    fn test_raft_addr() {
        let node = NodeConfig {
            id: 1,
            addr: "node1.example.com:9092".to_string(),
            raft_port: 9093,
        };

        assert_eq!(node.raft_addr(), "node1.example.com:9093");
    }

    #[test]
    fn test_raft_peer_addrs() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            gossip: None,
        };

        let addrs = config.raft_peer_addrs();
        assert_eq!(addrs.len(), 3);
        assert!(addrs.contains(&"10.0.1.10:9093".to_string()));
        assert!(addrs.contains(&"10.0.1.11:9093".to_string()));
        assert!(addrs.contains(&"10.0.1.12:9093".to_string()));
    }

    #[test]
    fn test_invalid_address_format() {
        let node = NodeConfig {
            id: 1,
            addr: "invalid-no-port".to_string(), // Missing port
            raft_port: 9093,
        };

        // Validation should fail
        let result = validate_addr(&node.addr);
        assert!(result.is_err());
    }

    #[test]
    fn test_zero_node_id() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 0, // Invalid!
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            gossip: None,
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_default_config() {
        let config = ClusterConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.node_id, 1);
        assert_eq!(config.replication_factor, 3);
        assert_eq!(config.min_insync_replicas, 2);
        assert_eq!(config.peers.len(), 0);
    }
}
