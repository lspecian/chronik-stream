use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{
    Condition, EnvVar, ImagePullSecret, ListenerSpec, ListenerStatus, ObjectStoreSpec, PdbSpec,
    PodSecurityContextSpec, ResourceRequirements, SchemaRegistrySpec, SecretKeyRef, StorageSpec,
    Toleration, UpdateStrategy, VectorSearchSpec,
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

    /// Pod-level security context applied to every broker Pod.
    ///
    /// When omitted, the controller defaults to `fsGroup: 1001` so the
    /// official `ghcr.io/lspecian/chronik-stream` image (which runs as
    /// `chronik` UID/GID 1001) can write to RWO PVCs that ship as
    /// `root:root`. Set explicitly to override (e.g. when running a
    /// custom image with a different UID, or to harden the broker).
    #[serde(default)]
    pub pod_security_context: Option<PodSecurityContextSpec>,

    /// Repair ownership of the data volume before the broker starts.
    ///
    /// Defaults to `true`. An init container chowns anything under the data dir
    /// not already owned by the broker's UID (1001). `fsGroup` alone does not
    /// cover this: the kubelet does not apply ownership management to
    /// `hostPath` volumes, which is what microk8s' `standard` class provisions,
    /// so an upgrade can leave data owned by `root` and every write fails with
    /// `Permission denied` while reads keep working (issue #51).
    ///
    /// Set `false` where a root init container is not permitted; the data must
    /// then be made writable by UID 1001 some other way.
    #[serde(default)]
    pub fix_data_ownership: Option<bool>,

    /// Secret holding the broker-to-broker SASL secret
    /// (`CHRONIK_INTER_BROKER_SECRET`).
    ///
    /// With SASL required (`CHRONIK_SASL_ENABLED` = `true`/`1`/`yes`/`required`
    /// in `env`) a cluster broker refuses to start without this secret, and
    /// replica fetchers use it to authenticate to the partition leader. It
    /// must be at least 16 characters and identical on every broker.
    ///
    /// Leave it unset and the operator generates one in a Secret named
    /// `<cluster>-inter-broker` (key `secret`), once, and never rotates it.
    /// Set it to supply your own. Either way it is only injected while SASL is
    /// enabled: a broker with SASL disabled cannot accept the broker login, so
    /// handing it the secret would break replication. If `CHRONIK_SASL_ENABLED`
    /// itself comes from a Secret the operator cannot tell whether SASL is on,
    /// so it injects only a secret set here, and generates none.
    #[serde(default)]
    pub inter_broker_secret: Option<SecretKeyRef>,

    /// TLS on the Kafka port (and the Unified API). Unset: plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<super::common::KafkaTlsSpec>,
}

/// Env var that turns on SASL in the broker.
pub const SASL_ENABLED_ENV: &str = "CHRONIK_SASL_ENABLED";

/// Env var carrying the broker-to-broker SASL secret.
pub const INTER_BROKER_SECRET_ENV: &str = "CHRONIK_INTER_BROKER_SECRET";

/// Key inside the generated `<cluster>-inter-broker` Secret.
pub const GENERATED_INTER_BROKER_SECRET_KEY: &str = "secret";

/// Name of the Secret the operator generates for a cluster's inter-broker secret.
pub fn generated_inter_broker_secret_name(cluster_name: &str) -> String {
    format!("{cluster_name}-inter-broker")
}

/// Whether a cluster spec turns SASL on, as the broker will read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaslSetting {
    /// `CHRONIK_SASL_ENABLED` unset or a value the broker treats as off.
    Disabled,
    /// `required`/`optional` (or their aliases): brokers need the secret.
    Enabled,
    /// `CHRONIK_SASL_ENABLED` comes from a Secret the operator does not read.
    Unknown,
}

/// Where the broker Pods get `CHRONIK_INTER_BROKER_SECRET` from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterBrokerSecretSource {
    /// `spec.interBrokerSecret`, supplied by the user.
    User(SecretKeyRef),
    /// The operator-generated `<cluster>-inter-broker` Secret.
    Generated,
}

impl InterBrokerSecretSource {
    /// The Secret key the Pods reference, for a cluster named `cluster_name`.
    pub fn secret_key_ref(&self, cluster_name: &str) -> SecretKeyRef {
        match self {
            Self::User(r) => r.clone(),
            Self::Generated => SecretKeyRef {
                name: generated_inter_broker_secret_name(cluster_name),
                key: GENERATED_INTER_BROKER_SECRET_KEY.into(),
            },
        }
    }
}

/// SASL mode of a container `env`, as the broker parses `CHRONIK_SASL_ENABLED`
/// (the last entry wins, trimmed, case-insensitive).
pub fn sasl_setting_of(env: &[EnvVar]) -> SaslSetting {
    let Some(var) = env.iter().rev().find(|e| e.name == SASL_ENABLED_ENV) else {
        return SaslSetting::Disabled;
    };
    match var.value.as_deref() {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "required" | "optional" | "warn" => SaslSetting::Enabled,
            _ => SaslSetting::Disabled,
        },
        None if var.value_from_secret.is_some() => SaslSetting::Unknown,
        None => SaslSetting::Disabled,
    }
}

impl ChronikClusterSpec {
    /// The last `env` entry with this name — the one Kubernetes applies when a
    /// container lists a name twice.
    fn user_env(&self, name: &str) -> Option<&EnvVar> {
        self.env.iter().rev().find(|e| e.name == name)
    }

    /// Read SASL mode from `env` exactly as the broker parses
    /// `CHRONIK_SASL_ENABLED` (trimmed, case-insensitive).
    pub fn sasl_setting(&self) -> SaslSetting {
        sasl_setting_of(&self.env)
    }

    /// Where the operator should source `CHRONIK_INTER_BROKER_SECRET` from, or
    /// `None` when it must not inject it at all: SASL is off, or the user
    /// already set the variable in `env`.
    pub fn inter_broker_secret_source(&self) -> Option<InterBrokerSecretSource> {
        if self.user_env(INTER_BROKER_SECRET_ENV).is_some() {
            return None;
        }
        match (self.sasl_setting(), &self.inter_broker_secret) {
            (SaslSetting::Disabled, _) => None,
            (_, Some(r)) => Some(InterBrokerSecretSource::User(r.clone())),
            (SaslSetting::Enabled, None) => Some(InterBrokerSecretSource::Generated),
            (SaslSetting::Unknown, None) => None,
        }
    }
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

    fn with_sasl(value_json: &str) -> ChronikClusterSpec {
        serde_json::from_str(&format!(
            r#"{{"env":[{{"name":"CHRONIK_SASL_ENABLED",{value_json}}}]}}"#
        ))
        .unwrap()
    }

    /// Mirrors the broker's `SaslMode::from_env`: trimmed, case-insensitive.
    #[test]
    fn sasl_setting_parses_like_the_broker() {
        for v in [
            "true", "1", "yes", "required", "Required", "  YES ", "optional", "warn",
        ] {
            assert_eq!(
                with_sasl(&format!(r#""value":"{v}""#)).sasl_setting(),
                SaslSetting::Enabled,
                "{v:?}"
            );
        }
        for v in ["false", "0", "no", "", "nonsense"] {
            assert_eq!(
                with_sasl(&format!(r#""value":"{v}""#)).sasl_setting(),
                SaslSetting::Disabled,
                "{v:?}"
            );
        }
        let none: ChronikClusterSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(none.sasl_setting(), SaslSetting::Disabled);
        assert_eq!(
            with_sasl(r#""valueFromSecret":{"name":"s","key":"k"}"#).sasl_setting(),
            SaslSetting::Unknown
        );
    }

    #[test]
    fn the_last_duplicate_env_entry_wins_as_in_kubernetes() {
        let spec: ChronikClusterSpec = serde_json::from_str(
            r#"{"env":[{"name":"CHRONIK_SASL_ENABLED","value":"required"},
                       {"name":"CHRONIK_SASL_ENABLED","value":"false"}]}"#,
        )
        .unwrap();
        assert_eq!(spec.sasl_setting(), SaslSetting::Disabled);
    }

    #[test]
    fn inter_broker_secret_source_resolution() {
        let user_ref = SecretKeyRef {
            name: "mine".into(),
            key: "s".into(),
        };

        let enabled = with_sasl(r#""value":"required""#);
        assert_eq!(
            enabled.inter_broker_secret_source(),
            Some(InterBrokerSecretSource::Generated)
        );
        assert_eq!(
            InterBrokerSecretSource::Generated.secret_key_ref("prod"),
            SecretKeyRef {
                name: "prod-inter-broker".into(),
                key: "secret".into()
            }
        );

        let mut enabled_user = enabled.clone();
        enabled_user.inter_broker_secret = Some(user_ref.clone());
        assert_eq!(
            enabled_user.inter_broker_secret_source(),
            Some(InterBrokerSecretSource::User(user_ref.clone()))
        );

        // SASL from a Secret: honour an explicit ref, but never generate one.
        let unknown = with_sasl(r#""valueFromSecret":{"name":"s","key":"k"}"#);
        assert_eq!(unknown.inter_broker_secret_source(), None);
        let mut unknown_user = unknown.clone();
        unknown_user.inter_broker_secret = Some(user_ref.clone());
        assert_eq!(
            unknown_user.inter_broker_secret_source(),
            Some(InterBrokerSecretSource::User(user_ref))
        );

        // The user set the variable themselves: leave it alone.
        let own: ChronikClusterSpec = serde_json::from_str(
            r#"{"env":[{"name":"CHRONIK_SASL_ENABLED","value":"required"},
                       {"name":"CHRONIK_INTER_BROKER_SECRET","value":"0123456789abcdef"}]}"#,
        )
        .unwrap();
        assert_eq!(own.inter_broker_secret_source(), None);
    }

    #[test]
    fn the_crd_schema_carries_inter_broker_secret() {
        let yaml = serde_yaml::to_string(&ChronikCluster::crd()).unwrap();
        assert!(yaml.contains("interBrokerSecret"));
    }
}
