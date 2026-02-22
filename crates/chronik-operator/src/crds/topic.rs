use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::Condition;

/// Spec for declarative topic management.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "chronik.io",
    version = "v1alpha1",
    kind = "ChronikTopic",
    namespaced,
    status = "ChronikTopicStatus",
    shortname = "ctopic",
    printcolumn = r#"{"name":"Partitions","type":"integer","jsonPath":".spec.partitions"}"#,
    printcolumn = r#"{"name":"RF","type":"integer","jsonPath":".spec.replicationFactor"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ChronikTopicSpec {
    /// Reference to the target Chronik cluster or standalone.
    pub cluster_ref: ClusterRef,

    /// Topic name. Defaults to metadata.name if omitted.
    #[serde(default)]
    pub topic_name: Option<String>,

    /// Number of partitions.
    #[serde(default = "super::defaults::topic_partitions")]
    pub partitions: i32,

    /// Replication factor (capped at cluster replicas).
    #[serde(default = "super::defaults::topic_replication_factor")]
    pub replication_factor: i32,

    /// Kafka-compatible topic configuration entries.
    #[serde(default)]
    pub config: Option<std::collections::BTreeMap<String, String>>,

    /// Columnar storage configuration.
    #[serde(default)]
    pub columnar: Option<ColumnarConfig>,

    /// Vector search configuration for this topic.
    #[serde(default)]
    pub vector_search: Option<TopicVectorSearchConfig>,

    /// Enable Tantivy full-text indexing.
    #[serde(default)]
    pub searchable: bool,

    /// What happens when the CR is deleted: "delete" (default) or "retain".
    #[serde(default)]
    pub deletion_policy: Option<String>,
}

/// Reference to a ChronikCluster or ChronikStandalone.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClusterRef {
    /// Name of the ChronikCluster or ChronikStandalone resource.
    pub name: String,

    /// Kind of the referenced resource. Defaults to "ChronikCluster".
    #[serde(default)]
    pub kind: Option<String>,
}

/// Columnar storage (Arrow/Parquet) configuration for a topic.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ColumnarConfig {
    /// Enable columnar storage.
    #[serde(default)]
    pub enabled: bool,

    /// Storage format: parquet or arrow.
    #[serde(default = "super::defaults::columnar_format")]
    pub format: String,

    /// Compression codec: zstd, snappy, lz4, none.
    #[serde(default = "super::defaults::columnar_compression")]
    pub compression: String,

    /// Time partitioning: none, hourly, daily.
    #[serde(default = "super::defaults::columnar_partitioning")]
    pub partitioning: String,
}

/// Vector search configuration specific to a topic.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopicVectorSearchConfig {
    /// Enable vector search on this topic.
    #[serde(default)]
    pub enabled: bool,

    /// Embedding provider: openai, external.
    #[serde(default = "super::defaults::embedding_provider")]
    pub embedding_provider: String,

    /// Embedding model.
    #[serde(default = "super::defaults::embedding_model")]
    pub embedding_model: String,

    /// Field to embed: value, key, or JSON path (e.g., "$.content").
    #[serde(default = "super::defaults::vector_field")]
    pub field: String,

    /// Distance metric: cosine, euclidean, dot.
    #[serde(default = "super::defaults::index_metric")]
    pub index_metric: String,
}

/// Status for ChronikTopic.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChronikTopicStatus {
    /// Current phase: Pending, Creating, Ready, Updating, Error.
    #[serde(default)]
    pub phase: Option<String>,

    /// Last observed generation.
    #[serde(default)]
    pub observed_generation: Option<i64>,

    /// Current partition count (from cluster).
    #[serde(default)]
    pub current_partitions: Option<i32>,

    /// Current replication factor.
    #[serde(default)]
    pub current_replication_factor: Option<i32>,

    /// Internal topic ID from Chronik.
    #[serde(default)]
    pub topic_id: Option<String>,

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
        let crd = ChronikTopic::crd();
        let yaml = serde_yaml::to_string(&crd).expect("CRD should serialize to YAML");
        assert!(yaml.contains("ChronikTopic"));
        assert!(yaml.contains("chronik.io"));
        assert!(yaml.contains("v1alpha1"));
    }

    #[test]
    fn test_spec_defaults() {
        let json = r#"{"clusterRef": {"name": "my-cluster"}}"#;
        let spec: ChronikTopicSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.partitions, 1);
        assert_eq!(spec.replication_factor, 3);
        assert!(!spec.searchable);
        assert!(spec.columnar.is_none());
        assert!(spec.vector_search.is_none());
    }

    #[test]
    fn test_topic_name_defaults_to_none() {
        let json = r#"{"clusterRef": {"name": "test"}}"#;
        let spec: ChronikTopicSpec = serde_json::from_str(json).unwrap();
        assert!(spec.topic_name.is_none());
    }
}
