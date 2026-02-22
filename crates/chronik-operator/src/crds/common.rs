use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Persistent storage configuration.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// Kubernetes StorageClass name.
    #[serde(default = "super::defaults::storage_class")]
    pub storage_class: String,

    /// Storage size (e.g., "50Gi", "100Gi").
    #[serde(default = "super::defaults::storage_size")]
    pub size: String,

    /// Access modes. Defaults to `["ReadWriteOnce"]`.
    #[serde(default = "super::defaults::access_modes")]
    pub access_modes: Vec<String>,
}

/// Object store backend configuration for tiered storage.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectStoreSpec {
    /// Backend type: s3, gcs, azure, local.
    pub backend: String,

    /// Bucket or container name.
    #[serde(default)]
    pub bucket: Option<String>,

    /// Region (S3/GCS).
    #[serde(default)]
    pub region: Option<String>,

    /// Endpoint override (e.g., for MinIO).
    #[serde(default)]
    pub endpoint: Option<String>,

    /// Secret name containing access credentials.
    #[serde(default)]
    pub credentials_secret: Option<String>,

    /// Prefix for object keys.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Vector search configuration for a topic or standalone instance.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VectorSearchSpec {
    /// Enable vector search.
    #[serde(default)]
    pub enabled: bool,

    /// Secret reference for embedding API key.
    #[serde(default)]
    pub api_key_secret: Option<SecretKeyRef>,

    /// Embedding model name (e.g., "text-embedding-3-small").
    #[serde(default = "super::defaults::embedding_model")]
    pub model: String,

    /// Embedding provider.
    #[serde(default = "super::defaults::embedding_provider")]
    pub provider: String,
}

/// Reference to a key within a Kubernetes Secret.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SecretKeyRef {
    /// Secret name.
    pub name: String,

    /// Key within the Secret.
    pub key: String,
}

/// Kubernetes-style condition for status reporting.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// Condition type (e.g., "Ready", "ClusterReady", "LeaderElected").
    #[serde(rename = "type")]
    pub type_: String,

    /// Status: "True", "False", or "Unknown".
    pub status: String,

    /// Machine-readable reason (e.g., "PodRunning").
    #[serde(default)]
    pub reason: Option<String>,

    /// Human-readable message.
    #[serde(default)]
    pub message: Option<String>,

    /// Last transition time (ISO 8601 string).
    #[serde(default)]
    pub last_transition_time: Option<String>,
}

/// Listener configuration for cluster nodes (inspired by Strimzi).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListenerSpec {
    /// Listener name (e.g., "internal", "external").
    pub name: String,

    /// Port number.
    pub port: i32,

    /// Listener type: internal, nodeport, loadbalancer, ingress.
    #[serde(rename = "type")]
    pub type_: ListenerType,

    /// Enable TLS for this listener.
    #[serde(default)]
    pub tls: bool,

    /// Base NodePort (for nodeport type). Node N gets nodePortBase + N - 1.
    #[serde(default)]
    pub node_port_base: Option<i32>,

    /// TLS certificate secret name.
    #[serde(default)]
    pub tls_secret: Option<String>,
}

/// Listener type enum.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ListenerType {
    Internal,
    #[serde(rename = "nodeport")]
    NodePort,
    #[serde(rename = "loadbalancer")]
    LoadBalancer,
    Ingress,
}

/// Listener status — reported in CR status so users know how to connect.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListenerStatus {
    /// Listener name.
    pub name: String,

    /// Bootstrap servers string (e.g., "cluster.ns.svc:9092").
    pub bootstrap_servers: String,

    /// Listener type.
    #[serde(rename = "type")]
    pub type_: ListenerType,
}

/// Rolling update strategy for cluster nodes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStrategy {
    /// Maximum number of nodes unavailable during rolling update.
    #[serde(default = "super::defaults::max_unavailable")]
    pub max_unavailable: i32,

    /// Delay in seconds between node restarts.
    #[serde(default = "super::defaults::restart_delay_secs")]
    pub restart_delay_secs: u64,
}

/// Pod Disruption Budget spec.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PdbSpec {
    /// Maximum number of pods that can be unavailable during voluntary disruptions.
    #[serde(default = "super::defaults::max_unavailable")]
    pub max_unavailable: i32,
}

/// Schema Registry configuration.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaRegistrySpec {
    /// Enable Schema Registry.
    #[serde(default)]
    pub enabled: bool,

    /// Enable HTTP Basic Auth for Schema Registry.
    #[serde(default)]
    pub auth_enabled: bool,

    /// Secret containing user credentials for Schema Registry.
    #[serde(default)]
    pub credentials_secret: Option<String>,
}

/// Environment variable definition.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    /// Variable name.
    pub name: String,

    /// Variable value (plain text).
    #[serde(default)]
    pub value: Option<String>,

    /// Value from a Secret key reference.
    #[serde(default)]
    pub value_from_secret: Option<SecretKeyRef>,
}

/// Resource requirements (requests and limits).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResourceRequirements {
    /// Resource requests.
    #[serde(default)]
    pub requests: Option<ResourceValues>,

    /// Resource limits.
    #[serde(default)]
    pub limits: Option<ResourceValues>,
}

/// CPU and memory values.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResourceValues {
    /// CPU (e.g., "500m", "2").
    #[serde(default)]
    pub cpu: Option<String>,

    /// Memory (e.g., "1Gi", "4Gi").
    #[serde(default)]
    pub memory: Option<String>,
}

/// Toleration for Pod scheduling.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Toleration {
    pub key: Option<String>,
    pub operator: Option<String>,
    pub value: Option<String>,
    pub effect: Option<String>,
    #[serde(default)]
    pub toleration_seconds: Option<i64>,
}

/// Image pull secret reference.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ImagePullSecret {
    pub name: String,
}
