use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::Condition;

/// Spec for operator-managed broker autoscaling.
///
/// Targets **broker infrastructure** metrics (CPU, memory, disk, produce throughput).
/// Does NOT scale based on consumer lag — consumer lag means the consumer application
/// needs more instances, not the broker cluster.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "chronik.io",
    version = "v1alpha1",
    kind = "ChronikAutoScaler",
    namespaced,
    status = "ChronikAutoScalerStatus",
    shortname = "cas",
    printcolumn = r#"{"name":"Cluster","type":"string","jsonPath":".spec.clusterRef.name"}"#,
    printcolumn = r#"{"name":"Current","type":"integer","jsonPath":".status.currentReplicas"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Active","type":"boolean","jsonPath":".status.active"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ChronikAutoScalerSpec {
    /// Reference to the target ChronikCluster.
    pub cluster_ref: ClusterRef,

    /// Minimum replicas (never below 3 for Raft quorum).
    #[serde(default = "super::defaults::min_replicas")]
    pub min_replicas: i32,

    /// Maximum replicas.
    #[serde(default = "super::defaults::max_replicas")]
    pub max_replicas: i32,

    /// Scaling metrics and thresholds.
    #[serde(default)]
    pub metrics: Vec<ScalingMetric>,

    /// Cooldown after scale-up in seconds.
    #[serde(default = "super::defaults::scale_up_cooldown_secs")]
    pub scale_up_cooldown_secs: u64,

    /// Cooldown after scale-down in seconds.
    #[serde(default = "super::defaults::scale_down_cooldown_secs")]
    pub scale_down_cooldown_secs: u64,

    /// Number of consecutive reconcile cycles a metric must exceed threshold before scale-up.
    #[serde(default = "super::defaults::scale_up_stabilization_count")]
    pub scale_up_stabilization_count: u32,

    /// Number of consecutive reconcile cycles a metric must be under threshold before scale-down.
    #[serde(default = "super::defaults::scale_down_stabilization_count")]
    pub scale_down_stabilization_count: u32,
}

/// A cluster reference for the autoscaler.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClusterRef {
    /// Name of the ChronikCluster resource.
    pub name: String,
}

/// A scaling metric definition.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScalingMetric {
    /// Metric type: cpu, memory, disk, produce_throughput.
    pub metric_type: ScalingMetricType,

    /// Target value (percentage for cpu/memory/disk, msgs/sec for throughput).
    pub target_value: u64,

    /// Tolerance percentage — don't scale if within this range of the target.
    #[serde(default = "super::defaults::tolerance_percent")]
    pub tolerance_percent: u32,
}

/// Supported scaling metric types.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScalingMetricType {
    Cpu,
    Memory,
    Disk,
    ProduceThroughput,
}

/// Status for ChronikAutoScaler.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChronikAutoScalerStatus {
    /// Whether autoscaling is active.
    #[serde(default)]
    pub active: bool,

    /// Current replica count.
    #[serde(default)]
    pub current_replicas: Option<i32>,

    /// Desired replica count.
    #[serde(default)]
    pub desired_replicas: Option<i32>,

    /// Last scale event time (ISO 8601 string).
    #[serde(default)]
    pub last_scale_time: Option<String>,

    /// Direction of last scale event: "up", "down", or "none".
    #[serde(default)]
    pub last_scale_direction: Option<String>,

    /// Current metric values.
    #[serde(default)]
    pub current_metrics: Vec<MetricStatus>,

    /// Status conditions.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// Current value of a scaling metric.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MetricStatus {
    /// Metric type.
    pub metric_type: ScalingMetricType,

    /// Current average value across all brokers.
    pub current_value: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn test_crd_generates_valid_schema() {
        let crd = ChronikAutoScaler::crd();
        let yaml = serde_yaml::to_string(&crd).expect("CRD should serialize to YAML");
        assert!(yaml.contains("ChronikAutoScaler"));
        assert!(yaml.contains("chronik.io"));
        assert!(yaml.contains("v1alpha1"));
    }

    #[test]
    fn test_spec_defaults() {
        let json = r#"{"clusterRef": {"name": "my-cluster"}}"#;
        let spec: ChronikAutoScalerSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.min_replicas, 3);
        assert_eq!(spec.max_replicas, 9);
        assert_eq!(spec.scale_up_cooldown_secs, 600);
        assert_eq!(spec.scale_down_cooldown_secs, 1800);
        assert_eq!(spec.scale_up_stabilization_count, 3);
        assert_eq!(spec.scale_down_stabilization_count, 6);
        assert!(spec.metrics.is_empty());
    }

    #[test]
    fn test_metric_type_serialization() {
        let metric = ScalingMetric {
            metric_type: ScalingMetricType::ProduceThroughput,
            target_value: 50000,
            tolerance_percent: 15,
        };
        let json = serde_json::to_string(&metric).unwrap();
        assert!(json.contains("produce_throughput"));
    }
}
