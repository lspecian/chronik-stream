use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{
    Condition, EnvVar, ImagePullSecret, ObjectStoreSpec, ResourceRequirements, StorageSpec,
    Toleration, VectorSearchSpec,
};

/// Spec for a single-node Chronik deployment.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "chronik.io",
    version = "v1alpha1",
    kind = "ChronikStandalone",
    namespaced,
    status = "ChronikStandaloneStatus",
    shortname = "csa",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Healthy","type":"boolean","jsonPath":".status.healthy"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ChronikStandaloneSpec {
    /// Container image for Chronik server.
    #[serde(default = "super::defaults::image")]
    pub image: String,

    /// Image pull policy.
    #[serde(default = "super::defaults::image_pull_policy")]
    pub image_pull_policy: String,

    /// Image pull secrets.
    #[serde(default)]
    pub image_pull_secrets: Vec<ImagePullSecret>,

    /// Resource requests and limits.
    #[serde(default)]
    pub resources: Option<ResourceRequirements>,

    /// Persistent storage configuration.
    #[serde(default)]
    pub storage: Option<StorageSpec>,

    /// Kafka protocol port.
    #[serde(default = "super::defaults::kafka_port")]
    pub kafka_port: i32,

    /// Unified API port (SQL, vector, search).
    #[serde(default = "super::defaults::unified_api_port")]
    pub unified_api_port: i32,

    /// Metrics endpoint port.
    #[serde(default)]
    pub metrics_port: Option<i32>,

    /// WAL commit profile: low, medium, high, ultra.
    #[serde(default = "super::defaults::wal_profile")]
    pub wal_profile: String,

    /// Produce handler flush profile: low-latency, balanced, high-throughput.
    #[serde(default = "super::defaults::produce_profile")]
    pub produce_profile: String,

    /// Log level: trace, debug, info, warn, error.
    #[serde(default = "super::defaults::log_level")]
    pub log_level: String,

    /// Object store configuration for tiered storage.
    #[serde(default)]
    pub object_store: Option<ObjectStoreSpec>,

    /// Enable columnar storage (Arrow/Parquet).
    #[serde(default)]
    pub columnar_enabled: bool,

    /// Vector search configuration.
    #[serde(default)]
    pub vector_search: Option<VectorSearchSpec>,

    /// Node selector for Pod scheduling.
    #[serde(default)]
    pub node_selector: Option<std::collections::BTreeMap<String, String>>,

    /// Pod tolerations.
    #[serde(default)]
    pub tolerations: Vec<Toleration>,

    /// Kubernetes Service type: ClusterIP, NodePort, LoadBalancer.
    #[serde(default = "super::defaults::service_type")]
    pub service_type: String,

    /// Annotations for the Service.
    #[serde(default)]
    pub service_annotations: Option<std::collections::BTreeMap<String, String>>,

    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<EnvVar>,
}

/// Status for ChronikStandalone.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChronikStandaloneStatus {
    /// Current phase: Pending, Running, Failed.
    #[serde(default)]
    pub phase: Option<String>,

    /// Whether the instance is healthy.
    #[serde(default)]
    pub healthy: bool,

    /// Kafka bootstrap address.
    #[serde(default)]
    pub kafka_address: Option<String>,

    /// Unified API address.
    #[serde(default)]
    pub unified_api_address: Option<String>,

    /// Last observed generation of the spec.
    #[serde(default)]
    pub observed_generation: Option<i64>,

    /// Status conditions.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn test_crd_generates_valid_schema() {
        let crd = ChronikStandalone::crd();
        let yaml = serde_yaml::to_string(&crd).expect("CRD should serialize to YAML");
        assert!(yaml.contains("ChronikStandalone"));
        assert!(yaml.contains("chronik.io"));
        assert!(yaml.contains("v1alpha1"));
    }

    #[test]
    fn test_spec_defaults() {
        let spec: ChronikStandaloneSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.kafka_port, 9092);
        assert_eq!(spec.unified_api_port, 6092);
        assert_eq!(spec.wal_profile, "medium");
        assert_eq!(spec.produce_profile, "balanced");
        assert_eq!(spec.log_level, "info");
        assert_eq!(spec.service_type, "ClusterIP");
        assert!(!spec.columnar_enabled);
        assert!(spec.vector_search.is_none());
        assert!(spec.object_store.is_none());
    }
}
