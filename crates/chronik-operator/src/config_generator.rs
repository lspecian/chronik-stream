use serde::Serialize;

/// Generate TOML cluster configuration for a specific node.
///
/// Produces a config file matching the `chronik-config::ClusterConfig` schema.
pub fn generate_node_config(params: &NodeConfigParams) -> String {
    let config = ClusterToml {
        enabled: true,
        node_id: params.node_id,
        data_dir: "/data".to_string(),
        replication_factor: params.replication_factor as usize,
        min_insync_replicas: params.min_insync_replicas as usize,
        auto_recover: params.auto_recover,
        bind: BindAddresses {
            kafka: format!("0.0.0.0:{}", params.kafka_port),
            wal: format!("0.0.0.0:{}", params.wal_port),
            raft: format!("0.0.0.0:{}", params.raft_port),
        },
        advertise: AdvertiseAddresses {
            kafka: format!("{}:{}", params.advertise_host, params.kafka_port),
            wal: format!("{}:{}", params.advertise_host, params.wal_port),
            raft: format!("{}:{}", params.advertise_host, params.raft_port),
        },
        peers: params.peers.clone(),
    };

    toml::to_string_pretty(&config).expect("TOML serialization should not fail")
}

/// Parameters for generating a node's TOML config.
pub struct NodeConfigParams {
    pub node_id: u64,
    pub kafka_port: i32,
    pub wal_port: i32,
    pub raft_port: i32,
    /// DNS name for this node's advertised address (e.g., "cluster-1.ns.svc.cluster.local").
    pub advertise_host: String,
    pub replication_factor: i32,
    pub min_insync_replicas: i32,
    pub auto_recover: bool,
    pub peers: Vec<PeerEntry>,
}

/// A peer entry in the [[peers]] array.
#[derive(Clone, Debug, Serialize)]
pub struct PeerEntry {
    pub id: u64,
    pub kafka: String,
    pub wal: String,
    pub raft: String,
}

/// Internal TOML structure matching `chronik-config::ClusterConfig`.
#[derive(Serialize)]
struct ClusterToml {
    enabled: bool,
    node_id: u64,
    data_dir: String,
    replication_factor: usize,
    min_insync_replicas: usize,
    auto_recover: bool,
    bind: BindAddresses,
    advertise: AdvertiseAddresses,
    peers: Vec<PeerEntry>,
}

#[derive(Serialize)]
struct BindAddresses {
    kafka: String,
    wal: String,
    raft: String,
}

#[derive(Serialize)]
struct AdvertiseAddresses {
    kafka: String,
    wal: String,
    raft: String,
}

/// Build the peer list for all nodes in the cluster.
///
/// Each peer's addresses use the per-node Service DNS name.
pub fn build_peer_list(
    cluster_name: &str,
    namespace: &str,
    replicas: i32,
    kafka_port: i32,
    wal_port: i32,
    raft_port: i32,
) -> Vec<PeerEntry> {
    (1..=replicas as u64)
        .map(|id| {
            let dns = node_dns_name(cluster_name, id, namespace);
            PeerEntry {
                id,
                kafka: format!("{dns}:{kafka_port}"),
                wal: format!("{dns}:{wal_port}"),
                raft: format!("{dns}:{raft_port}"),
            }
        })
        .collect()
}

/// DNS name for a specific cluster node's Service.
///
/// Format: `{cluster_name}-{node_id}.{namespace}.svc.cluster.local`
pub fn node_dns_name(cluster_name: &str, node_id: u64, namespace: &str) -> String {
    format!("{cluster_name}-{node_id}.{namespace}.svc.cluster.local")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_node_config_basic() {
        let peers = build_peer_list("prod", "default", 3, 9092, 9291, 5001);
        let params = NodeConfigParams {
            node_id: 1,
            kafka_port: 9092,
            wal_port: 9291,
            raft_port: 5001,
            advertise_host: "prod-1.default.svc.cluster.local".into(),
            replication_factor: 3,
            min_insync_replicas: 2,
            auto_recover: true,
            peers,
        };

        let toml_str = generate_node_config(&params);

        assert!(toml_str.contains("enabled = true"));
        assert!(toml_str.contains("node_id = 1"));
        assert!(toml_str.contains("replication_factor = 3"));
        assert!(toml_str.contains("min_insync_replicas = 2"));
        assert!(toml_str.contains("auto_recover = true"));
        assert!(toml_str.contains("[bind]"));
        assert!(toml_str.contains("kafka = \"0.0.0.0:9092\""));
        assert!(toml_str.contains("[advertise]"));
        assert!(toml_str.contains("kafka = \"prod-1.default.svc.cluster.local:9092\""));
        assert!(toml_str.contains("[[peers]]"));
        // Should have 3 peer entries
        assert_eq!(toml_str.matches("[[peers]]").count(), 3);
    }

    #[test]
    fn test_build_peer_list() {
        let peers = build_peer_list("my-cluster", "prod", 3, 9092, 9291, 5001);

        assert_eq!(peers.len(), 3);
        assert_eq!(peers[0].id, 1);
        assert_eq!(peers[0].kafka, "my-cluster-1.prod.svc.cluster.local:9092");
        assert_eq!(peers[0].wal, "my-cluster-1.prod.svc.cluster.local:9291");
        assert_eq!(peers[0].raft, "my-cluster-1.prod.svc.cluster.local:5001");
        assert_eq!(peers[2].id, 3);
    }

    #[test]
    fn test_node_dns_name() {
        assert_eq!(
            node_dns_name("prod", 2, "default"),
            "prod-2.default.svc.cluster.local"
        );
    }

    #[test]
    fn test_generated_toml_is_valid() {
        let peers = build_peer_list("test", "ns", 3, 9092, 9291, 5001);
        let params = NodeConfigParams {
            node_id: 2,
            kafka_port: 9092,
            wal_port: 9291,
            raft_port: 5001,
            advertise_host: "test-2.ns.svc.cluster.local".into(),
            replication_factor: 3,
            min_insync_replicas: 2,
            auto_recover: true,
            peers,
        };

        let toml_str = generate_node_config(&params);
        // Verify it parses as valid TOML
        let parsed: toml::Value =
            toml::from_str(&toml_str).expect("Generated TOML should be valid");
        assert_eq!(parsed["node_id"].as_integer(), Some(2));
        assert_eq!(parsed["peers"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_data_dir_is_slash_data() {
        let peers = build_peer_list("c", "ns", 3, 9092, 9291, 5001);
        let params = NodeConfigParams {
            node_id: 1,
            kafka_port: 9092,
            wal_port: 9291,
            raft_port: 5001,
            advertise_host: "c-1.ns.svc.cluster.local".into(),
            replication_factor: 3,
            min_insync_replicas: 2,
            auto_recover: true,
            peers,
        };

        let toml_str = generate_node_config(&params);
        assert!(toml_str.contains("data_dir = \"/data\""));
    }
}
