use std::collections::HashMap;

use tracing::debug;

use crate::error::OperatorError;

/// HTTP-based metrics scraper for Chronik broker nodes.
///
/// Scrapes Prometheus /metrics endpoints and the Kubernetes Metrics API
/// to collect CPU, memory, disk, and produce throughput metrics.
pub struct MetricsScraper {
    http: reqwest::Client,
}

impl Default for MetricsScraper {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsScraper {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("HTTP client should build"),
        }
    }

    /// Scrape Prometheus metrics from a broker's /metrics endpoint.
    pub async fn scrape_prometheus(
        &self,
        url: &str,
    ) -> Result<HashMap<String, f64>, OperatorError> {
        let resp = self.http.get(url).send().await?;
        let body = resp
            .text()
            .await
            .map_err(|e| OperatorError::AdminApi(format!("Failed to read metrics body: {e}")))?;
        Ok(parse_prometheus_metrics(&body))
    }

    /// Scrape pod-level CPU/memory from the Kubernetes Metrics API.
    ///
    /// Uses `kube::Api<DynamicObject>` to query `metrics.k8s.io/v1beta1/pods`.
    /// Returns empty vec if the Metrics API is unavailable (metrics-server not installed).
    pub async fn scrape_k8s_pod_metrics(
        &self,
        client: &kube::Client,
        namespace: &str,
        label_selector: &str,
    ) -> Result<Vec<PodResourceUsage>, OperatorError> {
        use kube::api::{Api, DynamicObject, ListParams};
        use kube::discovery::ApiResource;
        use kube::ResourceExt;

        let ar = ApiResource {
            group: "metrics.k8s.io".into(),
            version: "v1beta1".into(),
            api_version: "metrics.k8s.io/v1beta1".into(),
            kind: "PodMetrics".into(),
            plural: "pods".into(),
        };

        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);
        let lp = ListParams::default().labels(label_selector);
        let list = api.list(&lp).await?;

        let mut results = Vec::new();
        for item in list {
            let pod_name = item.name_any();
            let mut cpu_total: u64 = 0;
            let mut mem_total: u64 = 0;

            if let Some(containers) = item.data.get("containers").and_then(|c| c.as_array()) {
                for c in containers {
                    if let Some(usage) = c.get("usage") {
                        if let Some(cpu) = usage.get("cpu").and_then(|v| v.as_str()) {
                            cpu_total += parse_cpu_millicores(cpu);
                        }
                        if let Some(mem) = usage.get("memory").and_then(|v| v.as_str()) {
                            mem_total += parse_bytes_quantity(mem);
                        }
                    }
                }
            }

            results.push(PodResourceUsage {
                pod_name,
                cpu_millicores: cpu_total,
                memory_bytes: mem_total,
            });
        }

        Ok(results)
    }

    /// Scrape all metrics from a cluster and compute aggregated values.
    pub async fn scrape_cluster_metrics(
        &self,
        client: &kube::Client,
        cluster_name: &str,
        namespace: &str,
        replicas: i32,
        limits: &ResourceLimits,
    ) -> ClusterMetrics {
        let mut metrics = ClusterMetrics::default();

        // 1. CPU/Memory from K8s Metrics API
        let label_selector = format!(
            "{}={},{}={}",
            crate::constants::labels::INSTANCE,
            cluster_name,
            crate::constants::labels::COMPONENT,
            crate::constants::values::COMPONENT_CLUSTER_NODE,
        );

        match self
            .scrape_k8s_pod_metrics(client, namespace, &label_selector)
            .await
        {
            Ok(pod_metrics) if !pod_metrics.is_empty() => {
                let n = pod_metrics.len() as f64;
                let avg_cpu = pod_metrics
                    .iter()
                    .map(|p| p.cpu_millicores as f64)
                    .sum::<f64>()
                    / n;
                let avg_mem = pod_metrics
                    .iter()
                    .map(|p| p.memory_bytes as f64)
                    .sum::<f64>()
                    / n;

                if limits.cpu_millicores > 0 {
                    metrics.avg_cpu_percent = Some(avg_cpu / limits.cpu_millicores as f64 * 100.0);
                }
                if limits.memory_bytes > 0 {
                    metrics.avg_memory_percent = Some(avg_mem / limits.memory_bytes as f64 * 100.0);
                }
            }
            Ok(_) => {
                debug!("No pod metrics found for cluster {cluster_name}");
            }
            Err(e) => {
                debug!("K8s metrics API not available for {cluster_name}: {e}");
            }
        }

        // 2. Disk + produce throughput from Prometheus /metrics endpoint
        let mut total_disk: f64 = 0.0;
        let mut total_messages: f64 = 0.0;
        let mut scraped_count = 0u32;

        for node_id in 1..=(replicas as u64) {
            let dns = crate::config_generator::node_dns_name(cluster_name, node_id, namespace);
            let port = crate::constants::ports::METRICS_BASE + node_id as i32;
            let url = format!("http://{dns}:{port}/metrics");

            match self.scrape_prometheus(&url).await {
                Ok(prom) => {
                    if let Some(&disk) = prom.get("chronik_storage_usage_bytes") {
                        total_disk += disk;
                    }
                    if let Some(&msgs) = prom.get("chronik_messages_received_total") {
                        total_messages += msgs;
                    }
                    scraped_count += 1;
                }
                Err(e) => {
                    debug!(node_id, "Failed to scrape Prometheus metrics: {e}");
                }
            }
        }

        if scraped_count > 0 {
            if limits.disk_bytes > 0 {
                metrics.avg_disk_percent =
                    Some((total_disk / scraped_count as f64) / limits.disk_bytes as f64 * 100.0);
            }
            metrics.total_messages_received = Some(total_messages);
            metrics.node_count = scraped_count;
        }

        metrics
    }
}

// --- Types ---

/// Pod-level resource usage from the K8s Metrics API.
#[derive(Debug)]
pub struct PodResourceUsage {
    pub pod_name: String,
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
}

/// Resource limits from the cluster spec (for computing usage percentages).
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    /// CPU limit in millicores (e.g., 2000 = 2 cores).
    pub cpu_millicores: u64,
    /// Memory limit in bytes.
    pub memory_bytes: u64,
    /// Disk capacity in bytes (from PVC size).
    pub disk_bytes: u64,
}

/// Cluster-wide aggregated metrics.
#[derive(Debug, Default, Clone)]
pub struct ClusterMetrics {
    /// Average CPU usage as percentage of limit (0-100).
    pub avg_cpu_percent: Option<f64>,
    /// Average memory usage as percentage of limit (0-100).
    pub avg_memory_percent: Option<f64>,
    /// Average disk usage as percentage of capacity (0-100).
    pub avg_disk_percent: Option<f64>,
    /// Total messages received counter across all nodes.
    pub total_messages_received: Option<f64>,
    /// Number of nodes successfully scraped.
    pub node_count: u32,
}

/// Result of a scaling evaluation.
#[derive(Debug, PartialEq)]
pub enum ScalingAction {
    /// Scale up from current to desired replicas.
    ScaleUp { current: i32, desired: i32 },
    /// Scale down from current to desired replicas.
    ScaleDown { current: i32, desired: i32 },
    /// No scaling action needed.
    NoChange,
}

/// Tracks stabilization state between reconcile cycles.
///
/// Stored in the controller's `Context` (in-memory). Resets on operator restart,
/// which is safe — stabilization just restarts from zero.
#[derive(Debug, Default, Clone)]
pub struct StabilizationState {
    /// Consecutive reconcile cycles where scale-up was indicated.
    pub scale_up_count: u32,
    /// Consecutive reconcile cycles where scale-down was indicated.
    pub scale_down_count: u32,
    /// Previous total messages received (for rate calculation).
    pub prev_messages: Option<f64>,
    /// Timestamp of previous scrape (epoch seconds).
    pub prev_scrape_time: Option<u64>,
}

// --- Parsing ---

/// Parse Prometheus text-format metrics into a map of metric_name -> value.
///
/// Handles simple metrics (no labels) and labeled metrics. For labeled metrics,
/// the full `name{labels}` string is used as the key.
pub fn parse_prometheus_metrics(body: &str) -> HashMap<String, f64> {
    let mut metrics = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (name, rest) = if let Some(brace_pos) = line.find('{') {
            if let Some(close) = line[brace_pos..].find('}') {
                let end = brace_pos + close + 1;
                (&line[..end], line[end..].trim())
            } else {
                continue;
            }
        } else if let Some(space_pos) = line.find(' ') {
            (&line[..space_pos], line[space_pos..].trim())
        } else {
            continue;
        };

        let value_str = rest.split_whitespace().next().unwrap_or("");
        if let Ok(value) = value_str.parse::<f64>() {
            metrics.insert(name.to_string(), value);
        }
    }
    metrics
}

/// Parse a Kubernetes CPU quantity string into millicores.
///
/// Examples: "100m" → 100, "2" → 2000, "2.5" → 2500, "500n" → 0
pub fn parse_cpu_millicores(s: &str) -> u64 {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix('m') {
        stripped.parse::<u64>().unwrap_or(0)
    } else if let Some(stripped) = s.strip_suffix('n') {
        stripped.parse::<u64>().unwrap_or(0) / 1_000_000
    } else {
        (s.parse::<f64>().unwrap_or(0.0) * 1000.0) as u64
    }
}

/// Parse a Kubernetes quantity string (memory/storage) into bytes.
///
/// Supports binary suffixes (Ki, Mi, Gi, Ti) and SI suffixes (K, M, G, T).
/// Examples: "512Mi" → 536870912, "1Gi" → 1073741824, "50Gi" → 53687091200
pub fn parse_bytes_quantity(s: &str) -> u64 {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix("Ti") {
        stripped
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_mul(1024 * 1024 * 1024 * 1024)
    } else if let Some(stripped) = s.strip_suffix("Gi") {
        stripped
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_mul(1024 * 1024 * 1024)
    } else if let Some(stripped) = s.strip_suffix("Mi") {
        stripped
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_mul(1024 * 1024)
    } else if let Some(stripped) = s.strip_suffix("Ki") {
        stripped.parse::<u64>().unwrap_or(0).saturating_mul(1024)
    } else if let Some(stripped) = s.strip_suffix('T') {
        stripped
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_mul(1_000_000_000_000)
    } else if let Some(stripped) = s.strip_suffix('G') {
        stripped
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
    } else if let Some(stripped) = s.strip_suffix('M') {
        stripped
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_mul(1_000_000)
    } else if let Some(stripped) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
        stripped.parse::<u64>().unwrap_or(0).saturating_mul(1_000)
    } else {
        s.parse::<u64>().unwrap_or(0)
    }
}

// --- Scaling decision engine ---

/// Evaluate whether to scale up, down, or hold steady.
///
/// Pure function — all state passed in explicitly for testability.
/// Applies tolerance bands, stabilization counts, and cooldowns.
pub fn evaluate_scaling(
    metrics: &ClusterMetrics,
    spec: &crate::crds::autoscaler::ChronikAutoScalerSpec,
    current_replicas: i32,
    state: &mut StabilizationState,
    last_scale_time: Option<&str>,
    last_scale_direction: Option<&str>,
    produce_rate_per_node: Option<f64>,
) -> ScalingAction {
    use crate::crds::autoscaler::ScalingMetricType;

    if spec.metrics.is_empty() {
        return ScalingAction::NoChange;
    }

    // Check cooldown from last scaling event
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(last_time_str) = last_scale_time {
        if let Ok(last_secs) = last_time_str.parse::<u64>() {
            let elapsed = now.saturating_sub(last_secs);
            let cooldown = match last_scale_direction {
                Some("up") => spec.scale_up_cooldown_secs,
                Some("down") => spec.scale_down_cooldown_secs,
                _ => 0,
            };
            if elapsed < cooldown {
                return ScalingAction::NoChange;
            }
        }
    }

    // Evaluate each configured metric against its threshold
    let mut any_above_upper = false;
    let mut all_below_lower = true;
    let mut any_metric_available = false;

    for metric_spec in &spec.metrics {
        let current_value = match metric_spec.metric_type {
            ScalingMetricType::Cpu => metrics.avg_cpu_percent,
            ScalingMetricType::Memory => metrics.avg_memory_percent,
            ScalingMetricType::Disk => metrics.avg_disk_percent,
            ScalingMetricType::ProduceThroughput => produce_rate_per_node,
        };

        let Some(value) = current_value else {
            // Metric not available — don't use for decisions
            all_below_lower = false;
            continue;
        };

        any_metric_available = true;
        let target = metric_spec.target_value as f64;
        let tolerance = target * metric_spec.tolerance_percent as f64 / 100.0;
        let upper_bound = target + tolerance;
        let lower_bound = (target - tolerance).max(0.0);

        if value > upper_bound {
            any_above_upper = true;
        }
        if value >= lower_bound {
            all_below_lower = false;
        }
    }

    if !any_metric_available {
        return ScalingAction::NoChange;
    }

    // Update stabilization counters
    if any_above_upper {
        state.scale_up_count += 1;
        state.scale_down_count = 0;
    } else if all_below_lower {
        state.scale_down_count += 1;
        state.scale_up_count = 0;
    } else {
        // Within tolerance band — reset both
        state.scale_up_count = 0;
        state.scale_down_count = 0;
    }

    // Check if stabilization thresholds are met
    if state.scale_up_count >= spec.scale_up_stabilization_count {
        let desired = (current_replicas + 1).min(spec.max_replicas);
        if desired > current_replicas {
            state.scale_up_count = 0;
            return ScalingAction::ScaleUp {
                current: current_replicas,
                desired,
            };
        }
    }

    if state.scale_down_count >= spec.scale_down_stabilization_count {
        let desired = (current_replicas - 1).max(spec.min_replicas);
        if desired < current_replicas {
            state.scale_down_count = 0;
            return ScalingAction::ScaleDown {
                current: current_replicas,
                desired,
            };
        }
    }

    ScalingAction::NoChange
}

/// Extract resource limits from a ChronikCluster spec for percentage calculations.
pub fn extract_resource_limits(
    cluster_spec: &crate::crds::cluster::ChronikClusterSpec,
) -> ResourceLimits {
    let mut limits = ResourceLimits::default();

    if let Some(ref resources) = cluster_spec.resources {
        if let Some(ref res_limits) = resources.limits {
            if let Some(ref cpu) = res_limits.cpu {
                limits.cpu_millicores = parse_cpu_millicores(cpu);
            }
            if let Some(ref mem) = res_limits.memory {
                limits.memory_bytes = parse_bytes_quantity(mem);
            }
        }
    }

    if let Some(ref storage) = cluster_spec.storage {
        limits.disk_bytes = parse_bytes_quantity(&storage.size);
    } else {
        limits.disk_bytes = parse_bytes_quantity(crate::constants::defaults::STORAGE_SIZE);
    }

    limits
}

/// Compute produce message rate per node from current and previous counter values.
///
/// Returns messages/sec per node, or None if insufficient data (first scrape cycle).
pub fn compute_produce_rate(
    state: &mut StabilizationState,
    metrics: &ClusterMetrics,
) -> Option<f64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let current_messages = metrics.total_messages_received?;

    let rate =
        if let (Some(prev_msgs), Some(prev_time)) = (state.prev_messages, state.prev_scrape_time) {
            let elapsed = now.saturating_sub(prev_time);
            if elapsed > 0 && current_messages >= prev_msgs && metrics.node_count > 0 {
                let msgs_per_sec = (current_messages - prev_msgs) / elapsed as f64;
                Some(msgs_per_sec / metrics.node_count as f64)
            } else {
                None
            }
        } else {
            None
        };

    // Store current values for next cycle
    state.prev_messages = Some(current_messages);
    state.prev_scrape_time = Some(now);

    rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::autoscaler::{
        ChronikAutoScalerSpec, ClusterRef, ScalingMetric, ScalingMetricType,
    };

    #[test]
    fn test_parse_prometheus_basic() {
        let body = r#"
# HELP chronik_storage_usage_bytes Total storage usage
# TYPE chronik_storage_usage_bytes gauge
chronik_storage_usage_bytes 1073741824
# HELP chronik_messages_received_total Total messages received
# TYPE chronik_messages_received_total counter
chronik_messages_received_total 42000
"#;
        let metrics = parse_prometheus_metrics(body);
        assert_eq!(
            metrics.get("chronik_storage_usage_bytes"),
            Some(&1073741824.0)
        );
        assert_eq!(
            metrics.get("chronik_messages_received_total"),
            Some(&42000.0)
        );
    }

    #[test]
    fn test_parse_prometheus_with_labels() {
        let body = r#"
chronik_produce_requests_total{result="success"} 100
chronik_produce_requests_total{result="error"} 5
chronik_storage_usage_bytes 500000
"#;
        let metrics = parse_prometheus_metrics(body);
        assert_eq!(
            metrics.get(r#"chronik_produce_requests_total{result="success"}"#),
            Some(&100.0)
        );
        assert_eq!(
            metrics.get(r#"chronik_produce_requests_total{result="error"}"#),
            Some(&5.0)
        );
        assert_eq!(metrics.get("chronik_storage_usage_bytes"), Some(&500000.0));
    }

    #[test]
    fn test_parse_prometheus_ignores_comments_and_empty() {
        let body = "# HELP foo\n# TYPE foo gauge\n\n\nfoo 42.5\n";
        let metrics = parse_prometheus_metrics(body);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics.get("foo"), Some(&42.5));
    }

    #[test]
    fn test_parse_cpu_millicores() {
        assert_eq!(parse_cpu_millicores("100m"), 100);
        assert_eq!(parse_cpu_millicores("2"), 2000);
        assert_eq!(parse_cpu_millicores("2.5"), 2500);
        assert_eq!(parse_cpu_millicores("500m"), 500);
        assert_eq!(parse_cpu_millicores("0"), 0);
    }

    #[test]
    fn test_parse_bytes_quantity() {
        assert_eq!(parse_bytes_quantity("512Mi"), 512 * 1024 * 1024);
        assert_eq!(parse_bytes_quantity("1Gi"), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes_quantity("50Gi"), 50 * 1024 * 1024 * 1024);
        assert_eq!(parse_bytes_quantity("100Ki"), 100 * 1024);
        assert_eq!(parse_bytes_quantity("1G"), 1_000_000_000);
        assert_eq!(parse_bytes_quantity("1073741824"), 1073741824);
    }

    fn test_spec(metrics: Vec<ScalingMetric>) -> ChronikAutoScalerSpec {
        ChronikAutoScalerSpec {
            cluster_ref: ClusterRef {
                name: "test".into(),
            },
            min_replicas: 3,
            max_replicas: 9,
            metrics,
            scale_up_cooldown_secs: 600,
            scale_down_cooldown_secs: 1800,
            scale_up_stabilization_count: 3,
            scale_down_stabilization_count: 6,
        }
    }

    #[test]
    fn test_evaluate_scaling_no_metrics() {
        let spec = test_spec(vec![]);
        let metrics = ClusterMetrics::default();
        let mut state = StabilizationState::default();

        let action = evaluate_scaling(&metrics, &spec, 3, &mut state, None, None, None);
        assert_eq!(action, ScalingAction::NoChange);
    }

    #[test]
    fn test_evaluate_scaling_scale_up_after_stabilization() {
        let spec = test_spec(vec![ScalingMetric {
            metric_type: ScalingMetricType::Cpu,
            target_value: 75,
            tolerance_percent: 10,
        }]);

        // CPU at 90% — above upper bound (75 + 7.5 = 82.5)
        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(90.0),
            ..Default::default()
        };
        let mut state = StabilizationState::default();

        // Need 3 consecutive cycles
        let action = evaluate_scaling(&metrics, &spec, 3, &mut state, None, None, None);
        assert_eq!(action, ScalingAction::NoChange);
        assert_eq!(state.scale_up_count, 1);

        let action = evaluate_scaling(&metrics, &spec, 3, &mut state, None, None, None);
        assert_eq!(action, ScalingAction::NoChange);
        assert_eq!(state.scale_up_count, 2);

        // 3rd cycle triggers scale up
        let action = evaluate_scaling(&metrics, &spec, 3, &mut state, None, None, None);
        assert_eq!(
            action,
            ScalingAction::ScaleUp {
                current: 3,
                desired: 4
            }
        );
    }

    #[test]
    fn test_evaluate_scaling_respects_max_replicas() {
        let spec = test_spec(vec![ScalingMetric {
            metric_type: ScalingMetricType::Cpu,
            target_value: 75,
            tolerance_percent: 10,
        }]);

        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(90.0),
            ..Default::default()
        };
        let mut state = StabilizationState {
            scale_up_count: 2,
            ..Default::default()
        };

        // Already at max — should not scale
        let action = evaluate_scaling(&metrics, &spec, 9, &mut state, None, None, None);
        assert_eq!(action, ScalingAction::NoChange);
    }

    #[test]
    fn test_evaluate_scaling_scale_down_after_stabilization() {
        let spec = test_spec(vec![ScalingMetric {
            metric_type: ScalingMetricType::Cpu,
            target_value: 75,
            tolerance_percent: 10,
        }]);

        // CPU at 50% — below lower bound (75 - 7.5 = 67.5)
        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(50.0),
            ..Default::default()
        };
        let mut state = StabilizationState {
            scale_down_count: 5,
            ..Default::default()
        };

        // 6th cycle triggers scale down
        let action = evaluate_scaling(&metrics, &spec, 5, &mut state, None, None, None);
        assert_eq!(
            action,
            ScalingAction::ScaleDown {
                current: 5,
                desired: 4
            }
        );
    }

    #[test]
    fn test_evaluate_scaling_respects_min_replicas() {
        let spec = test_spec(vec![ScalingMetric {
            metric_type: ScalingMetricType::Cpu,
            target_value: 75,
            tolerance_percent: 10,
        }]);

        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(10.0),
            ..Default::default()
        };
        let mut state = StabilizationState {
            scale_down_count: 5,
            ..Default::default()
        };

        // Already at min — should not scale down
        let action = evaluate_scaling(&metrics, &spec, 3, &mut state, None, None, None);
        assert_eq!(action, ScalingAction::NoChange);
    }

    #[test]
    fn test_evaluate_scaling_within_tolerance_resets_counters() {
        let spec = test_spec(vec![ScalingMetric {
            metric_type: ScalingMetricType::Cpu,
            target_value: 75,
            tolerance_percent: 10,
        }]);

        // CPU at 73% — within tolerance band [67.5, 82.5]
        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(73.0),
            ..Default::default()
        };
        let mut state = StabilizationState {
            scale_up_count: 2,
            scale_down_count: 4,
            ..Default::default()
        };

        let action = evaluate_scaling(&metrics, &spec, 5, &mut state, None, None, None);
        assert_eq!(action, ScalingAction::NoChange);
        assert_eq!(state.scale_up_count, 0);
        assert_eq!(state.scale_down_count, 0);
    }

    #[test]
    fn test_evaluate_scaling_cooldown_blocks_action() {
        let spec = test_spec(vec![ScalingMetric {
            metric_type: ScalingMetricType::Cpu,
            target_value: 75,
            tolerance_percent: 10,
        }]);

        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(90.0),
            ..Default::default()
        };
        let mut state = StabilizationState {
            scale_up_count: 10,
            ..Default::default()
        };

        // Recent scale event (1 second ago)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let last_time = (now - 1).to_string();

        let action = evaluate_scaling(
            &metrics,
            &spec,
            3,
            &mut state,
            Some(&last_time),
            Some("up"),
            None,
        );
        assert_eq!(action, ScalingAction::NoChange);
    }

    #[test]
    fn test_extract_resource_limits() {
        use crate::crds::cluster::ChronikClusterSpec;
        use crate::crds::common::{ResourceRequirements, ResourceValues, StorageSpec};

        let spec: ChronikClusterSpec = serde_json::from_str(r#"{"replicas": 3}"#).unwrap();
        let limits = extract_resource_limits(&spec);
        // Default storage: 50Gi
        assert_eq!(limits.disk_bytes, 50 * 1024 * 1024 * 1024);
        assert_eq!(limits.cpu_millicores, 0); // No limits set
        assert_eq!(limits.memory_bytes, 0);

        // With explicit limits
        let spec = ChronikClusterSpec {
            resources: Some(ResourceRequirements {
                requests: None,
                limits: Some(ResourceValues {
                    cpu: Some("4".into()),
                    memory: Some("8Gi".into()),
                }),
            }),
            storage: Some(StorageSpec {
                storage_class: "ssd".into(),
                size: "100Gi".into(),
                access_modes: vec!["ReadWriteOnce".into()],
            }),
            ..serde_json::from_str("{}").unwrap()
        };
        let limits = extract_resource_limits(&spec);
        assert_eq!(limits.cpu_millicores, 4000);
        assert_eq!(limits.memory_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(limits.disk_bytes, 100 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_compute_produce_rate() {
        let mut state = StabilizationState::default();
        let metrics = ClusterMetrics {
            total_messages_received: Some(10000.0),
            node_count: 2,
            ..Default::default()
        };

        // First call — no previous data, returns None
        let rate = compute_produce_rate(&mut state, &metrics);
        assert!(rate.is_none());
        assert!(state.prev_messages.is_some());
        assert!(state.prev_scrape_time.is_some());

        // Simulate second call 30s later with more messages
        // (We can't easily test time-dependent code, but verify state is stored)
        assert_eq!(state.prev_messages, Some(10000.0));
    }
}
