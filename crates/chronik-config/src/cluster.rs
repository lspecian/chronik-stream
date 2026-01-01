//! Cluster configuration for static peer discovery.
//!
//! This module defines the configuration structure for Chronik Raft clustering,
//! including node identity, peer information, and replication settings.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};
use validator::Validate;

use crate::{ConfigError, Result};

/// Node addresses for binding (where server listens)
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NodeBindAddresses {
    /// Kafka API bind address (e.g., "0.0.0.0:9092")
    #[validate(custom = "validate_addr")]
    pub kafka: String,

    /// WAL receiver bind address (e.g., "0.0.0.0:9291")
    #[validate(custom = "validate_addr")]
    pub wal: String,

    /// Raft gRPC bind address (e.g., "0.0.0.0:5001")
    #[validate(custom = "validate_addr")]
    pub raft: String,

    /// Metrics endpoint bind address (optional)
    pub metrics: Option<String>,

    /// Search API bind address (optional, requires search feature)
    pub search: Option<String>,
}

/// Node addresses for advertising (what clients connect to)
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NodeAdvertiseAddresses {
    /// Kafka API advertised address (e.g., "node1.example.com:9092")
    #[validate(custom = "validate_addr")]
    pub kafka: String,

    /// WAL receiver advertised address (e.g., "node1.example.com:9291")
    #[validate(custom = "validate_addr")]
    pub wal: String,

    /// Raft gRPC advertised address (e.g., "node1.example.com:5001")
    #[validate(custom = "validate_addr")]
    pub raft: String,
}

/// Cluster configuration for static peer discovery
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct ClusterConfig {
    /// Whether clustering is enabled
    pub enabled: bool,

    /// This node's unique identifier
    #[validate(custom = "validate_node_id")]
    pub node_id: u64,

    /// Data directory for this node's storage
    /// Example: "/home/ubuntu/Development/chronik-stream/tests/cluster/data/node1"
    pub data_dir: String,

    /// Number of replicas for each partition
    #[validate(range(min = 1, max = 100))]
    pub replication_factor: usize,

    /// Minimum number of in-sync replicas required for writes
    #[validate(range(min = 1, max = 100))]
    pub min_insync_replicas: usize,

    /// List of all peers in the cluster (including this node)
    #[validate(length(min = 1))]
    pub peers: Vec<NodeConfig>,

    /// This node's bind addresses (where to listen) - optional for env-based config
    pub bind: Option<NodeBindAddresses>,

    /// This node's advertise addresses (what to tell clients) - optional for env-based config
    pub advertise: Option<NodeAdvertiseAddresses>,

    /// Gossip configuration (optional, for automatic bootstrap)
    pub gossip: Option<GossipConfig>,

    /// Enable automatic quorum recovery (v2.2.7 Phase 2.5)
    ///
    /// When enabled (default: true), the cluster will automatically remove dead nodes after
    /// quorum is lost for 5 minutes to restore availability.
    ///
    /// **Safety**: Will never remove nodes if it would go below 2 nodes.
    /// **Use case**: Kubernetes/cloud deployments where nodes can be replaced.
    /// **Disable**: Set to false in config if you want manual intervention for node failures.
    #[serde(default = "default_auto_recover")]
    pub auto_recover: bool,
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

    /// Kafka API address (advertised, for clients)
    #[validate(custom = "validate_addr")]
    pub kafka: String,

    /// WAL receiver address (advertised, for followers)
    #[validate(custom = "validate_addr")]
    pub wal: String,

    /// Raft gRPC address (advertised, for peers)
    #[validate(custom = "validate_addr")]
    pub raft: String,

    // DEPRECATED: Kept for backward compatibility only
    /// @deprecated Use `kafka` field instead
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,

    /// @deprecated Use `raft` field instead
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raft_port: Option<u16>,
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
            data_dir: std::env::var("CHRONIK_DATA_DIR").unwrap_or_else(|_| "./data".to_string()),
            replication_factor,
            min_insync_replicas,
            peers,
            bind: None, // Bind addresses set via config file
            advertise: None, // Advertise addresses set via config file
            gossip: None, // TODO: Parse from env if needed
            auto_recover: true, // Default to enabled for resilient cloud deployments
        };

        config.validate_config()?;
        Ok(Some(config))
    }

    /// Parse peers from CHRONIK_CLUSTER_PEERS environment variable
    /// Format: "host:kafka_port:wal_port:raft_port,host:kafka_port:wal_port:raft_port,..."
    fn parse_peers_from_env() -> Result<Vec<NodeConfig>> {
        let peers_str = std::env::var("CHRONIK_CLUSTER_PEERS")
            .map_err(|_| ConfigError::Parse("CHRONIK_CLUSTER_PEERS not set".to_string()))?;

        let mut peers = Vec::new();
        for (idx, peer_str) in peers_str.split(',').enumerate() {
            let parts: Vec<&str> = peer_str.split(':').collect();
            if parts.len() != 4 {
                return Err(ConfigError::Parse(format!(
                    "Invalid peer format '{}': expected 'host:kafka_port:wal_port:raft_port'",
                    peer_str
                )));
            }

            let host = parts[0].to_string();
            let kafka_port = parts[1].parse::<u16>().map_err(|e| {
                ConfigError::Parse(format!("Invalid Kafka port '{}': {}", parts[1], e))
            })?;
            let wal_port = parts[2].parse::<u16>().map_err(|e| {
                ConfigError::Parse(format!("Invalid WAL port '{}': {}", parts[2], e))
            })?;
            let raft_port = parts[3].parse::<u16>().map_err(|e| {
                ConfigError::Parse(format!("Invalid Raft port '{}': {}", parts[3], e))
            })?;

            peers.push(NodeConfig {
                id: (idx + 1) as u64, // Auto-assign IDs based on order
                kafka: format!("{}:{}", host, kafka_port),
                wal: format!("{}:{}", host, wal_port),
                raft: format!("{}:{}", host, raft_port),
                addr: None, // Deprecated
                raft_port: None, // Deprecated
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
        let mut seen_kafka = HashSet::new();
        let mut seen_wal = HashSet::new();
        let mut seen_raft = HashSet::new();

        for peer in &self.peers {
            // Check Kafka address
            if !seen_kafka.insert(peer.kafka.clone()) {
                return Err(ConfigError::Validation(
                    crate::validation::ValidationError::Custom(format!(
                        "Duplicate Kafka address: {}",
                        peer.kafka
                    )),
                ));
            }

            // Check WAL address
            if !seen_wal.insert(peer.wal.clone()) {
                return Err(ConfigError::Validation(
                    crate::validation::ValidationError::Custom(format!(
                        "Duplicate WAL address: {}",
                        peer.wal
                    )),
                ));
            }

            // Check Raft address
            if !seen_raft.insert(peer.raft.clone()) {
                return Err(ConfigError::Validation(
                    crate::validation::ValidationError::Custom(format!(
                        "Duplicate Raft address: {}",
                        peer.raft
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
            .map(|p| p.raft.clone())
            .collect()
    }
}

impl NodeConfig {
    /// Parse the Kafka address as a SocketAddr
    pub fn kafka_socket_addr(&self) -> Result<SocketAddr> {
        self.kafka
            .to_socket_addrs()
            .map_err(|e| ConfigError::Parse(format!("Invalid address '{}': {}", self.kafka, e)))?
            .next()
            .ok_or_else(|| ConfigError::Parse(format!("No socket address for '{}'", self.kafka)))
    }

    /// Parse the WAL address as a SocketAddr
    pub fn wal_socket_addr(&self) -> Result<SocketAddr> {
        self.wal
            .to_socket_addrs()
            .map_err(|e| ConfigError::Parse(format!("Invalid address '{}': {}", self.wal, e)))?
            .next()
            .ok_or_else(|| ConfigError::Parse(format!("No socket address for '{}'", self.wal)))
    }

    /// Parse the Raft address as a SocketAddr
    pub fn raft_socket_addr(&self) -> Result<SocketAddr> {
        self.raft
            .to_socket_addrs()
            .map_err(|e| ConfigError::Parse(format!("Invalid address '{}': {}", self.raft, e)))?
            .next()
            .ok_or_else(|| ConfigError::Parse(format!("No socket address for '{}'", self.raft)))
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_id: 1,
            data_dir: "./data".to_string(),  // Default data directory
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![],
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,  // v2.2.7 Phase 2.5: Enabled by default for resilience
        }
    }
}

/// Default value for auto_recover (v2.2.7 Phase 2.5)
fn default_auto_recover() -> bool {
    true  // Enable by default for resilient cloud/K8s deployments
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
                kafka: "10.0.1.10:9092".to_string(),
                wal: "10.0.1.10:9291".to_string(),
                raft: "10.0.1.10:5001".to_string(),
                addr: None,
                raft_port: None,
            },
            NodeConfig {
                id: 2,
                kafka: "10.0.1.11:9092".to_string(),
                wal: "10.0.1.11:9291".to_string(),
                raft: "10.0.1.11:5001".to_string(),
                addr: None,
                raft_port: None,
            },
            NodeConfig {
                id: 3,
                kafka: "10.0.1.12:9092".to_string(),
                wal: "10.0.1.12:9291".to_string(),
                raft: "10.0.1.12:5001".to_string(),
                addr: None,
                raft_port: None,
            },
        ]
    }

    #[test]
    fn test_valid_cluster_config() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "./data".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        assert!(config.validate_config().is_ok());
    }

    #[test]
    fn test_node_id_not_in_peers() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 99,
            data_dir: "./data".to_string(), // Not in peers
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_duplicate_node_ids() {
        let peers = vec![
            NodeConfig {
                id: 1,
                kafka: "10.0.1.10:9092".to_string(),
                wal: "10.0.1.10:9291".to_string(),
                raft: "10.0.1.10:5001".to_string(),
                addr: None,
                raft_port: None,
            },
            NodeConfig {
                id: 1, // Duplicate!
                kafka: "10.0.1.11:9092".to_string(),
                wal: "10.0.1.11:9291".to_string(),
                raft: "10.0.1.11:5001".to_string(),
                addr: None,
                raft_port: None,
            },
        ];

        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "./data".to_string(),
            replication_factor: 2,
            min_insync_replicas: 1,
            peers,
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_duplicate_addresses() {
        let peers = vec![
            NodeConfig {
                id: 1,
                kafka: "10.0.1.10:9092".to_string(),
                wal: "10.0.1.10:9291".to_string(),
                raft: "10.0.1.10:5001".to_string(),
                addr: None,
                raft_port: None,
            },
            NodeConfig {
                id: 2,
                kafka: "10.0.1.10:9092".to_string(), // Duplicate kafka address!
                wal: "10.0.1.10:9291".to_string(), // Duplicate wal address!
                raft: "10.0.1.10:5001".to_string(), // Duplicate raft address!
                addr: None,
                raft_port: None,
            },
        ];

        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "./data".to_string(),
            replication_factor: 2,
            min_insync_replicas: 1,
            peers,
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_replication_factor_exceeds_peers() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "./data".to_string(),
            replication_factor: 5, // More than peer count
            min_insync_replicas: 2,
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_min_insync_exceeds_replication_factor() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "./data".to_string(),
            replication_factor: 2,
            min_insync_replicas: 3, // More than replication factor
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_this_node() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            data_dir: "./data".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        let this_node = config.this_node().unwrap();
        assert_eq!(this_node.id, 2);
        assert_eq!(this_node.kafka, "10.0.1.11:9092");
    }

    #[test]
    fn test_peer_nodes() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            data_dir: "./data".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        let peers = config.peer_nodes();
        assert_eq!(peers.len(), 2);
        assert!(peers.iter().all(|p| p.id != 2));
    }

    #[test]
    fn test_raft_peer_addrs() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "./data".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        let addrs = config.raft_peer_addrs();
        assert_eq!(addrs.len(), 3);
        assert!(addrs.contains(&"10.0.1.10:5001".to_string()));
        assert!(addrs.contains(&"10.0.1.11:5001".to_string()));
        assert!(addrs.contains(&"10.0.1.12:5001".to_string()));
    }

    #[test]
    fn test_invalid_address_format() {
        let node = NodeConfig {
            id: 1,
            kafka: "invalid-no-port".to_string(), // Missing port
            wal: "localhost:9291".to_string(),
            raft: "localhost:5001".to_string(),
            addr: None,
            raft_port: None,
        };

        // Validation should fail
        let result = validate_addr(&node.kafka);
        assert!(result.is_err());
    }

    #[test]
    fn test_zero_node_id() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 0,
            data_dir: "./data".to_string(), // Invalid!
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: create_test_peers(),
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
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
