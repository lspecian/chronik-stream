use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{
    Condition, EnvVar, ImagePullSecret, ListenerSpec, ListenerStatus, ObjectStoreSpec, PdbSpec,
    ResourceRequirements, SchemaRegistrySpec, StorageSpec, Toleration, UpdateStrategy,
    VectorSearchSpec,
};

/// Spec for a multi-node Chronik Raft cluster.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "chronik.io",
    version = "v1alpha1",
    kind = "ChronikCluster",
    namespaced,
    status = "ChronikClusterStatus",
    shortname = "ccl",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.readyNodes"}"#,
    printcolumn = r#"{"name":"Desired","type":"string","jsonPath":".status.desiredNodes"}"#,
    printcolumn = r#"{"name":"Leader","type":"integer","jsonPath":".status.leaderNodeId"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ChronikClusterSpec {
    /// Number of cluster nodes (minimum 3 for Raft quorum).
    #[serde(default = "super::defaults::replicas")]
    pub replicas: i32,

    /// Container image for Chronik server.
    #[serde(default = "super::defaults::image")]
    pub image: String,

    /// Image pull policy.
    #[serde(default = "super::defaults::image_pull_policy")]
    pub image_pull_policy: String,

    /// Image pull secrets.
    #[serde(default)]
    pub image_pull_secrets: Vec<ImagePullSecret>,

    /// Resource requests and limits per node.
    #[serde(default)]
    pub resources: Option<ResourceRequirements>,

    /// Persistent storage configuration per node.
    #[serde(default)]
    pub storage: Option<StorageSpec>,

    /// Replication factor for partitions.
    #[serde(default = "super::defaults::replication_factor")]
    pub replication_factor: i32,

    /// Minimum in-sync replicas for writes.
    #[serde(default = "super::defaults::min_insync_replicas")]
    pub min_insync_replicas: i32,

    /// WAL replication port (internal, same on each Pod).
    #[serde(default = "super::defaults::wal_port")]
    pub wal_port: i32,

    /// Raft consensus port (internal, same on each Pod).
    #[serde(default = "super::defaults::raft_port")]
    pub raft_port: i32,

    /// Unified API port.
    #[serde(default = "super::defaults::unified_api_port")]
    pub unified_api_port: i32,

    /// Metrics endpoint port.
    #[serde(default)]
    pub metrics_port: Option<i32>,

    /// Named listeners (Strimzi-inspired). Each defines a Kafka access point.
    #[serde(default)]
    pub listeners: Vec<ListenerSpec>,

    /// WAL commit profile.
    #[serde(default = "super::defaults::wal_profile")]
    pub wal_profile: String,

    /// Produce handler flush profile.
    #[serde(default = "super::defaults::produce_profile")]
    pub produce_profile: String,

    /// Log level.
    #[serde(default = "super::defaults::log_level")]
    pub log_level: String,

    /// Secret containing the admin API key.
    #[serde(default)]
    pub admin_api_key_secret: Option<super::common::SecretKeyRef>,

    /// Object store for tiered storage.
    #[serde(default)]
    pub object_store: Option<ObjectStoreSpec>,

    /// Anti-affinity mode: "preferred" or "required".
    #[serde(default = "super::defaults::anti_affinity")]
    pub anti_affinity: String,

    /// Topology key for anti-affinity.
    #[serde(default = "super::defaults::anti_affinity_topology_key")]
    pub anti_affinity_topology_key: String,

    /// Node selector for Pod scheduling.
    #[serde(default)]
    pub node_selector: Option<std::collections::BTreeMap<String, String>>,

    /// Pod tolerations.
    #[serde(default)]
    pub tolerations: Vec<Toleration>,

    /// Pod Disruption Budget configuration.
    #[serde(default)]
    pub pod_disruption_budget: Option<PdbSpec>,

    /// Rolling update strategy.
    #[serde(default)]
    pub update_strategy: Option<UpdateStrategy>,

    /// Enable automatic quorum recovery.
    #[serde(default = "super::defaults::auto_recover")]
    pub auto_recover: bool,

    /// Enable columnar storage.
    #[serde(default)]
    pub columnar_enabled: bool,

    /// Vector search configuration.
    #[serde(default)]
    pub vector_search: Option<VectorSearchSpec>,

    /// Schema Registry configuration.
    #[serde(default)]
    pub schema_registry: Option<SchemaRegistrySpec>,

    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<EnvVar>,
}

/// Status for ChronikCluster.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChronikClusterStatus {
    /// Current phase: Pending, Bootstrapping, Running, Scaling, Updating, Degraded, Failed.
    #[serde(default)]
    pub phase: Option<String>,

    /// Number of ready nodes.
    #[serde(default)]
    pub ready_nodes: i32,

    /// Desired number of nodes.
    #[serde(default)]
    pub desired_nodes: i32,

    /// Current Raft leader node ID.
    #[serde(default)]
    pub leader_node_id: Option<i64>,

    /// Last observed generation.
    #[serde(default)]
    pub observed_generation: Option<i64>,

    /// Per-listener connection info.
    #[serde(default)]
    pub listeners: Vec<ListenerStatus>,

    /// Headless Service DNS name.
    #[serde(default)]
    pub headless_service: Option<String>,

    /// Per-node status.
    #[serde(default)]
    pub nodes: Vec<ClusterNodeStatus>,

    /// Partition summary.
    #[serde(default)]
    pub partition_summary: Option<PartitionSummary>,

    /// Status conditions.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// Status of a single cluster node.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterNodeStatus {
    /// Node ID.
    pub node_id: i64,

    /// Pod name.
    pub pod_name: String,

    /// Node phase: Pending, Running, Failed.
    #[serde(default)]
    pub phase: Option<String>,

    /// Whether the node is ready.
    #[serde(default)]
    pub ready: bool,

    /// Whether this node is the Raft leader.
    #[serde(default)]
    pub is_leader: bool,

    /// Pod IP address.
    #[serde(default)]
    pub pod_ip: Option<String>,
}

/// Summary of partition state across the cluster.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PartitionSummary {
    pub total_partitions: i32,
    pub in_sync_partitions: i32,
    pub under_replicated_partitions: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn test_crd_generates_valid_schema() {
        let crd = ChronikCluster::crd();
        let yaml = serde_yaml::to_string(&crd).expect("CRD should serialize to YAML");
        assert!(yaml.contains("ChronikCluster"));
        assert!(yaml.contains("chronik.io"));
        assert!(yaml.contains("v1alpha1"));
    }

    #[test]
    fn test_spec_defaults() {
        let spec: ChronikClusterSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.replicas, 3);
        assert_eq!(spec.replication_factor, 3);
        assert_eq!(spec.min_insync_replicas, 2);
        assert_eq!(spec.wal_port, 9291);
        assert_eq!(spec.raft_port, 5001);
        assert_eq!(spec.unified_api_port, 6092);
        assert_eq!(spec.wal_profile, "medium"); // shared default; users typically set "high" for clusters
        assert!(spec.auto_recover);
        assert!(spec.listeners.is_empty());
    }

    #[test]
    fn test_minimum_replicas_enforced_by_convention() {
        // The operator controller will enforce minimum 3 replicas at reconcile time,
        // not at the CRD schema level, so users get a clear error message.
        let spec: ChronikClusterSpec = serde_json::from_str(r#"{"replicas": 1}"#).unwrap();
        assert_eq!(spec.replicas, 1); // Schema allows it; controller rejects it
    }
}
