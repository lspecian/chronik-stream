//! Custom Resource Definitions for Chronik Stream.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    api::{Api, Patch, PatchParams, PostParams},
    core::CustomResourceExt,
    Client, CustomResource,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;


/// Install all CRDs
pub async fn install_crds(client: Client) -> Result<(), kube::Error> {
    install_chronik_cluster_crd(client.clone()).await?;
    Ok(())
}

/// ChronikCluster custom resource
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "chronik.stream",
    version = "v1alpha1",
    kind = "ChronikCluster",
    plural = "chronikclusters",
    shortname = "ck",
    namespaced,
    status = "ChronikClusterStatus",
    derive = "Default"
)]
#[serde(rename_all = "camelCase")]
pub struct ChronikClusterSpec {
    /// Number of controller nodes
    #[serde(default = "default_controllers")]
    pub controllers: i32,
    
    /// Number of ingest nodes
    #[serde(default = "default_ingest_nodes")]
    pub ingest_nodes: i32,
    
    /// Number of search nodes
    pub search_nodes: Option<i32>,
    
    /// Storage configuration
    pub storage: StorageSpec,
    
    /// Metastore configuration
    pub metastore: MetastoreSpec,
    
    /// Resource requirements
    pub resources: Option<ResourceRequirements>,
    
    /// Image configuration
    pub image: Option<ImageSpec>,
    
    /// Monitoring configuration
    pub monitoring: Option<MonitoringSpec>,
    
    /// Network configuration
    pub network: Option<NetworkSpec>,
    
    /// Security configuration
    pub security: Option<SecuritySpec>,
    
    /// Autoscaling configuration
    pub autoscaling: Option<AutoscalingSpec>,
    
    /// Pod disruption budget configuration
    pub pod_disruption_budget: Option<PodDisruptionBudgetSpec>,
    
    /// Additional pod annotations
    pub pod_annotations: Option<BTreeMap<String, String>>,
    
    /// Additional pod labels
    pub pod_labels: Option<BTreeMap<String, String>>,
    
    /// Node selector for pod scheduling
    pub node_selector: Option<BTreeMap<String, String>>,
    
    /// Tolerations for pod scheduling
    pub tolerations: Option<Vec<Toleration>>,
    
    /// Affinity rules for pod scheduling
    pub affinity: Option<AffinitySpec>,
}

/// Storage specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// Storage backend type
    pub backend: StorageBackend,
    
    /// Storage size per node
    pub size: String,
    
    /// Storage class name
    pub storage_class: Option<String>,
}

/// Storage backend types
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum StorageBackend {
    S3 {
        bucket: String,
        region: String,
        endpoint: Option<String>,
    },
    Gcs {
        bucket: String,
    },
    Azure {
        container: String,
        account: String,
    },
    Local,
}

/// Metastore specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MetastoreSpec {
    /// Database type
    pub database: DatabaseType,
    
    /// Connection configuration
    pub connection: ConnectionSpec,
}

/// Database types
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DatabaseType {
    Postgres,
}

/// Connection specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSpec {
    /// Host or service name
    pub host: String,
    
    /// Port number
    pub port: i32,
    
    /// Database name
    pub database: String,
    
    /// Secret name for credentials
    pub credentials_secret: String,
}

/// Resource requirements
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequirements {
    /// Controller resources
    pub controller: ResourceSpec,
    
    /// Ingest node resources
    pub ingest: ResourceSpec,
    
    /// Search node resources
    pub search: Option<ResourceSpec>,
}

/// Resource specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSpec {
    /// CPU request
    pub cpu: String,
    
    /// Memory request
    pub memory: String,
    
    /// CPU limit
    pub cpu_limit: Option<String>,
    
    /// Memory limit
    pub memory_limit: Option<String>,
}

/// Image specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageSpec {
    /// Controller image
    pub controller: String,
    
    /// Ingest image
    pub ingest: String,
    
    /// Search image
    pub search: Option<String>,
    
    /// Image pull policy
    pub pull_policy: Option<String>,
    
    /// Image pull secrets
    pub pull_secrets: Option<Vec<String>>,
}

/// Monitoring specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MonitoringSpec {
    /// Enable Prometheus metrics
    pub prometheus: bool,
    
    /// Enable OpenTelemetry tracing
    pub tracing: bool,
    
    /// OTLP endpoint for traces
    pub otlp_endpoint: Option<String>,
}

/// Network specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSpec {
    /// Service type for ingress (ClusterIP, NodePort, LoadBalancer)
    pub service_type: Option<String>,
    
    /// Load balancer source ranges
    pub load_balancer_source_ranges: Option<Vec<String>>,
    
    /// Enable host network
    pub host_network: Option<bool>,
    
    /// DNS policy
    pub dns_policy: Option<String>,
}

/// Security specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecuritySpec {
    /// Enable TLS
    pub tls_enabled: bool,
    
    /// TLS secret name
    pub tls_secret: Option<String>,
    
    /// Enable mTLS
    pub mtls_enabled: Option<bool>,
    
    /// Pod security context
    pub pod_security_context: Option<PodSecurityContext>,
    
    /// Container security context
    pub container_security_context: Option<ContainerSecurityContext>,
}

/// Pod security context
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodSecurityContext {
    /// Run as user
    pub run_as_user: Option<i64>,
    
    /// Run as group
    pub run_as_group: Option<i64>,
    
    /// FS group
    pub fs_group: Option<i64>,
    
    /// Run as non root
    pub run_as_non_root: Option<bool>,
}

/// Container security context
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSecurityContext {
    /// Allow privilege escalation
    pub allow_privilege_escalation: Option<bool>,
    
    /// Privileged
    pub privileged: Option<bool>,
    
    /// Read only root filesystem
    pub read_only_root_filesystem: Option<bool>,
    
    /// Run as non root
    pub run_as_non_root: Option<bool>,
    
    /// Run as user
    pub run_as_user: Option<i64>,
}

/// Autoscaling specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutoscalingSpec {
    /// Enable autoscaling for controllers
    pub controllers: Option<AutoscalingConfig>,
    
    /// Enable autoscaling for ingest nodes
    pub ingest_nodes: Option<AutoscalingConfig>,
    
    /// Enable autoscaling for search nodes
    pub search_nodes: Option<AutoscalingConfig>,
}

/// Autoscaling configuration
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutoscalingConfig {
    /// Minimum replicas
    pub min_replicas: i32,
    
    /// Maximum replicas
    pub max_replicas: i32,
    
    /// Target CPU utilization percentage
    pub target_cpu_utilization_percentage: Option<i32>,
    
    /// Target memory utilization percentage
    pub target_memory_utilization_percentage: Option<i32>,
    
    /// Custom metrics
    pub custom_metrics: Option<Vec<CustomMetric>>,
}

/// Custom metric for autoscaling
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CustomMetric {
    /// Metric name
    pub name: String,
    
    /// Target type (Utilization, Value, AverageValue)
    pub target_type: String,
    
    /// Target value
    pub target_value: String,
}

/// Pod disruption budget specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodDisruptionBudgetSpec {
    /// Minimum available pods
    pub min_available: Option<IntOrString>,
    
    /// Maximum unavailable pods
    pub max_unavailable: Option<IntOrString>,
}

/// IntOrString type
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IntOrString {
    Int(i32),
    String(String),
}

/// Toleration for pod scheduling
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Toleration {
    /// Key
    pub key: Option<String>,
    
    /// Operator
    pub operator: Option<String>,
    
    /// Value
    pub value: Option<String>,
    
    /// Effect
    pub effect: Option<String>,
    
    /// Toleration seconds
    pub toleration_seconds: Option<i64>,
}

/// Affinity specification
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AffinitySpec {
    /// Node affinity
    pub node_affinity: Option<serde_json::Value>,
    
    /// Pod affinity
    pub pod_affinity: Option<serde_json::Value>,
    
    /// Pod anti-affinity
    pub pod_anti_affinity: Option<serde_json::Value>,
}

/// ChronikCluster status
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChronikClusterStatus {
    /// Current phase
    pub phase: ClusterPhase,
    
    /// Controller endpoints
    pub controllers: Vec<String>,
    
    /// Ingest endpoints
    pub ingest_nodes: Vec<String>,
    
    /// Search endpoints
    pub search_nodes: Vec<String>,
    
    /// Last updated timestamp
    pub last_updated: String,
    
    /// Conditions
    pub conditions: Vec<Condition>,
    
    /// Observed generation
    pub observed_generation: Option<i64>,
    
    /// Ready replicas per component
    pub ready_replicas: Option<ReadyReplicas>,
}

/// Ready replicas per component
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadyReplicas {
    /// Controller ready replicas
    pub controllers: i32,
    
    /// Ingest ready replicas
    pub ingest_nodes: i32,
    
    /// Search ready replicas
    pub search_nodes: i32,
}

/// Cluster phases
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub enum ClusterPhase {
    #[default]
    Pending,
    Creating,
    Running,
    Updating,
    Failed,
    Deleting,
}

/// Condition
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// Condition type
    pub condition_type: String,
    
    /// Status
    pub status: String,
    
    /// Last transition time
    pub last_transition_time: String,
    
    /// Reason
    pub reason: String,
    
    /// Message
    pub message: String,
}

/// Install ChronikCluster CRD
async fn install_chronik_cluster_crd(client: Client) -> Result<(), kube::Error> {
    let crds: Api<CustomResourceDefinition> = Api::all(client);
    let crd = ChronikCluster::crd();
    
    let params = PostParams::default();
    match crds.create(&params, &crd).await {
        Ok(_) => {
            tracing::info!("Created ChronikCluster CRD");
            Ok(())
        }
        Err(kube::Error::Api(api_err)) if api_err.code == 409 => {
            tracing::info!("ChronikCluster CRD already exists");
            // Update the existing CRD
            let name = crd.metadata.name.as_ref().unwrap();
            let patch_params = PatchParams::default();
            crds.patch(name, &patch_params, &Patch::Apply(crd.clone())).await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// Manual Default implementation for ChronikClusterSpec
impl Default for ChronikClusterSpec {
    fn default() -> Self {
        Self {
            controllers: 3,
            ingest_nodes: 3,
            search_nodes: None,
            storage: StorageSpec {
                backend: StorageBackend::Local,
                size: "10Gi".to_string(),
                storage_class: None,
            },
            metastore: MetastoreSpec {
                database: DatabaseType::Postgres,
                connection: ConnectionSpec {
                    host: "postgres".to_string(),
                    port: 5432,
                    database: "chronik".to_string(),
                    credentials_secret: "chronik-postgres-secret".to_string(),
                },
            },
            resources: None,
            image: None,
            monitoring: None,
            network: None,
            security: None,
            autoscaling: None,
            pod_disruption_budget: None,
            pod_annotations: None,
            pod_labels: None,
            node_selector: None,
            tolerations: None,
            affinity: None,
        }
    }
}

// Default functions for serde
fn default_controllers() -> i32 {
    3
}

fn default_ingest_nodes() -> i32 {
    3
}

fn default_storage_size() -> String {
    "10Gi".to_string()
}

fn default_host() -> String {
    "postgres".to_string()
}

fn default_port() -> i32 {
    5432
}

fn default_database() -> String {
    "chronik".to_string()
}

fn default_credentials_secret() -> String {
    "chronik-postgres-secret".to_string()
}