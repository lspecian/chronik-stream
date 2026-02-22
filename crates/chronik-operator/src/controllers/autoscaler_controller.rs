use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use tracing::{error, info, warn};

use crate::constants;
use crate::crds::autoscaler::{
    ChronikAutoScaler, ChronikAutoScalerStatus, MetricStatus, ScalingMetricType,
};
use crate::crds::cluster::ChronikCluster;
use crate::crds::common::Condition;
use crate::error::OperatorError;
use crate::scaling::{
    compute_produce_rate, evaluate_scaling, extract_resource_limits, ClusterMetrics,
    MetricsScraper, ScalingAction, StabilizationState,
};

/// Shared context for the autoscaler reconciler.
pub struct Context {
    pub client: Client,
    pub scraper: MetricsScraper,
    /// Per-autoscaler stabilization state. Key = "namespace/name".
    pub stabilization: Mutex<HashMap<String, StabilizationState>>,
}

/// Start the autoscaler controller.
pub async fn run(client: Client) {
    let api: Api<ChronikAutoScaler> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        client: client.clone(),
        scraper: MetricsScraper::new(),
        stabilization: Mutex::new(HashMap::new()),
    });

    Controller::new(api, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => {
                    error!("AutoScaler reconciliation error: {:?}", e);
                }
            }
        })
        .await;
}

/// Main reconciliation function.
async fn reconcile(
    autoscaler: Arc<ChronikAutoScaler>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let client = &ctx.client;
    let namespace = autoscaler.namespace().unwrap_or_else(|| "default".into());
    let cr_name = autoscaler.name_any();
    let spec = &autoscaler.spec;
    let stab_key = format!("{namespace}/{cr_name}");

    info!(autoscaler = %cr_name, "Reconciling ChronikAutoScaler");

    // 1. Resolve the target ChronikCluster
    let cluster_api: Api<ChronikCluster> = Api::namespaced(client.clone(), &namespace);

    let cluster = match cluster_api.get(&spec.cluster_ref.name).await {
        Ok(c) => c,
        Err(e) => {
            warn!(
                autoscaler = %cr_name,
                "Cannot find ChronikCluster '{}': {e}",
                spec.cluster_ref.name
            );
            update_status(
                client,
                &cr_name,
                &namespace,
                false,
                None,
                None,
                None,
                None,
                vec![],
                vec![make_condition(
                    "ScalingActive",
                    "False",
                    "ClusterNotFound",
                    &format!("ChronikCluster '{}' not found: {e}", spec.cluster_ref.name),
                )],
            )
            .await?;
            return Ok(Action::requeue(Duration::from_secs(
                constants::defaults::REQUEUE_AUTOSCALER_SECS,
            )));
        }
    };

    let current_replicas = cluster.spec.replicas;
    let limits = extract_resource_limits(&cluster.spec);

    // 2. Scrape metrics from all broker nodes
    let metrics = ctx
        .scraper
        .scrape_cluster_metrics(
            client,
            &spec.cluster_ref.name,
            &namespace,
            current_replicas,
            &limits,
        )
        .await;

    // 3. Compute produce rate from counter values
    let mut state = {
        let map = ctx.stabilization.lock().unwrap();
        map.get(&stab_key).cloned().unwrap_or_default()
    };
    let produce_rate = compute_produce_rate(&mut state, &metrics);

    // 4. Get current status for cooldown check
    let current_status = autoscaler.status.as_ref();
    let last_scale_time = current_status.and_then(|s| s.last_scale_time.as_deref());
    let last_scale_direction = current_status.and_then(|s| s.last_scale_direction.as_deref());

    // 5. Evaluate scaling decision
    let action = evaluate_scaling(
        &metrics,
        spec,
        current_replicas,
        &mut state,
        last_scale_time,
        last_scale_direction,
        produce_rate,
    );

    // 6. Store updated stabilization state
    {
        let mut map = ctx.stabilization.lock().unwrap();
        map.insert(stab_key, state);
    }

    // 7. Build metric status for the CR
    let metric_statuses = build_metric_statuses(spec, &metrics, produce_rate);

    // 8. Apply scaling action
    let (desired_replicas, new_scale_time, new_scale_direction) = match action {
        ScalingAction::ScaleUp {
            current: _,
            desired,
        } => {
            info!(
                autoscaler = %cr_name,
                current = current_replicas,
                desired,
                "Scaling up cluster '{}'",
                spec.cluster_ref.name
            );
            patch_cluster_replicas(&cluster_api, &spec.cluster_ref.name, desired).await?;
            let now = epoch_secs();
            (desired, Some(now.to_string()), Some("up".to_string()))
        }
        ScalingAction::ScaleDown {
            current: _,
            desired,
        } => {
            info!(
                autoscaler = %cr_name,
                current = current_replicas,
                desired,
                "Scaling down cluster '{}'",
                spec.cluster_ref.name
            );
            patch_cluster_replicas(&cluster_api, &spec.cluster_ref.name, desired).await?;
            let now = epoch_secs();
            (desired, Some(now.to_string()), Some("down".to_string()))
        }
        ScalingAction::NoChange => (
            current_replicas,
            current_status.and_then(|s| s.last_scale_time.clone()),
            current_status.and_then(|s| s.last_scale_direction.clone()),
        ),
    };

    // 9. Update autoscaler status
    let within_limits =
        current_replicas >= spec.min_replicas && current_replicas <= spec.max_replicas;

    let conditions = vec![
        make_condition(
            "ScalingActive",
            "True",
            "Active",
            "Autoscaler is monitoring cluster metrics",
        ),
        make_condition(
            "WithinLimits",
            if within_limits { "True" } else { "False" },
            if within_limits {
                "WithinBounds"
            } else {
                "OutOfBounds"
            },
            &format!(
                "Replicas {} within [{}, {}]",
                current_replicas, spec.min_replicas, spec.max_replicas
            ),
        ),
    ];

    update_status(
        client,
        &cr_name,
        &namespace,
        true,
        Some(current_replicas),
        Some(desired_replicas),
        new_scale_time,
        new_scale_direction,
        metric_statuses,
        conditions,
    )
    .await?;

    Ok(Action::requeue(Duration::from_secs(
        constants::defaults::REQUEUE_AUTOSCALER_SECS,
    )))
}

/// Patch the ChronikCluster spec.replicas field.
async fn patch_cluster_replicas(
    api: &Api<ChronikCluster>,
    name: &str,
    desired: i32,
) -> Result<(), OperatorError> {
    let patch = serde_json::json!({
        "spec": {
            "replicas": desired
        }
    });
    api.patch(
        name,
        &PatchParams::apply("chronik-operator"),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

/// Build metric status entries from current metrics.
fn build_metric_statuses(
    spec: &crate::crds::autoscaler::ChronikAutoScalerSpec,
    metrics: &ClusterMetrics,
    produce_rate: Option<f64>,
) -> Vec<MetricStatus> {
    spec.metrics
        .iter()
        .map(|m| {
            let value = match m.metric_type {
                ScalingMetricType::Cpu => metrics.avg_cpu_percent.map(|v| v as u64),
                ScalingMetricType::Memory => metrics.avg_memory_percent.map(|v| v as u64),
                ScalingMetricType::Disk => metrics.avg_disk_percent.map(|v| v as u64),
                ScalingMetricType::ProduceThroughput => produce_rate.map(|v| v as u64),
            };
            MetricStatus {
                metric_type: m.metric_type.clone(),
                current_value: value.unwrap_or(0),
            }
        })
        .collect()
}

/// Get current epoch seconds.
fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Update the ChronikAutoScaler status subresource.
#[allow(clippy::too_many_arguments)]
async fn update_status(
    client: &Client,
    name: &str,
    namespace: &str,
    active: bool,
    current_replicas: Option<i32>,
    desired_replicas: Option<i32>,
    last_scale_time: Option<String>,
    last_scale_direction: Option<String>,
    current_metrics: Vec<MetricStatus>,
    conditions: Vec<Condition>,
) -> Result<(), OperatorError> {
    let status = ChronikAutoScalerStatus {
        active,
        current_replicas,
        desired_replicas,
        last_scale_time,
        last_scale_direction,
        current_metrics,
        conditions,
    };

    let status_patch = serde_json::json!({ "status": status });
    let ns_api: Api<ChronikAutoScaler> = Api::namespaced(client.clone(), namespace);
    ns_api
        .patch_status(
            name,
            &PatchParams::apply("chronik-operator"),
            &Patch::Merge(&status_patch),
        )
        .await?;

    Ok(())
}

/// Build a status condition.
fn make_condition(type_: &str, status: &str, reason: &str, message: &str) -> Condition {
    Condition {
        type_: type_.into(),
        status: status.into(),
        reason: Some(reason.into()),
        message: Some(message.into()),
        last_transition_time: None,
    }
}

/// Error policy.
fn error_policy(
    _autoscaler: Arc<ChronikAutoScaler>,
    error: &OperatorError,
    _ctx: Arc<Context>,
) -> Action {
    warn!("AutoScaler reconciliation error: {error}");
    if error.is_transient() {
        Action::requeue(Duration::from_secs(15))
    } else {
        Action::requeue(Duration::from_secs(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_condition() {
        let cond = make_condition("ScalingActive", "True", "Active", "Autoscaler is active");
        assert_eq!(cond.type_, "ScalingActive");
        assert_eq!(cond.status, "True");
        assert_eq!(cond.reason.as_deref(), Some("Active"));
        assert_eq!(cond.message.as_deref(), Some("Autoscaler is active"));
    }

    #[test]
    fn test_build_metric_statuses() {
        use crate::crds::autoscaler::{
            ChronikAutoScalerSpec, ClusterRef, ScalingMetric, ScalingMetricType,
        };

        let spec = ChronikAutoScalerSpec {
            cluster_ref: ClusterRef {
                name: "test".into(),
            },
            min_replicas: 3,
            max_replicas: 9,
            metrics: vec![
                ScalingMetric {
                    metric_type: ScalingMetricType::Cpu,
                    target_value: 75,
                    tolerance_percent: 10,
                },
                ScalingMetric {
                    metric_type: ScalingMetricType::Disk,
                    target_value: 80,
                    tolerance_percent: 5,
                },
                ScalingMetric {
                    metric_type: ScalingMetricType::ProduceThroughput,
                    target_value: 50000,
                    tolerance_percent: 15,
                },
            ],
            scale_up_cooldown_secs: 600,
            scale_down_cooldown_secs: 1800,
            scale_up_stabilization_count: 3,
            scale_down_stabilization_count: 6,
        };

        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(65.0),
            avg_disk_percent: Some(42.0),
            ..Default::default()
        };

        let statuses = build_metric_statuses(&spec, &metrics, Some(12345.0));
        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[0].metric_type, ScalingMetricType::Cpu);
        assert_eq!(statuses[0].current_value, 65);
        assert_eq!(statuses[1].metric_type, ScalingMetricType::Disk);
        assert_eq!(statuses[1].current_value, 42);
        assert_eq!(
            statuses[2].metric_type,
            ScalingMetricType::ProduceThroughput
        );
        assert_eq!(statuses[2].current_value, 12345);
    }

    #[test]
    fn test_epoch_secs_is_nonzero() {
        assert!(epoch_secs() > 0);
    }
}
