use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{finalizer, Event as Finalizer};
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::config_generator::{self, NodeConfigParams, PeerEntry};
use crate::constants::{self, labels, values, FINALIZER};
use crate::crds::cluster::{
    ChronikCluster, ChronikClusterSpec, ChronikClusterStatus, ClusterNodeStatus,
};
use crate::crds::common::Condition;
use crate::error::OperatorError;
use crate::resources::{configmap_builder, pdb_builder, pod_builder, pvc_builder, service_builder};

/// Shared context for the cluster reconciler.
pub struct Context {
    pub client: Client,
}

/// Start the cluster controller.
pub async fn run(client: Client) {
    let api: Api<ChronikCluster> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::all(client.clone());
    let services: Api<Service> = Api::all(client.clone());
    let configmaps: Api<ConfigMap> = Api::all(client.clone());
    let pdbs: Api<PodDisruptionBudget> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        client: client.clone(),
    });

    Controller::new(api, watcher::Config::default())
        .owns(pods, watcher::Config::default())
        .owns(pvcs, watcher::Config::default())
        .owns(services, watcher::Config::default())
        .owns(configmaps, watcher::Config::default())
        .owns(pdbs, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => {
                    error!("Cluster reconciliation error: {:?}", e);
                }
            }
        })
        .await;
}

/// Main reconciliation function.
async fn reconcile(
    cluster: Arc<ChronikCluster>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let client = &ctx.client;
    let namespace = cluster.namespace().unwrap_or_else(|| "default".into());

    let ns_api: Api<ChronikCluster> = Api::namespaced(client.clone(), &namespace);

    finalizer(&ns_api, FINALIZER, cluster, |event| async {
        match event {
            Finalizer::Apply(cluster) => apply(client.clone(), &cluster).await,
            Finalizer::Cleanup(cluster) => cleanup(client.clone(), &cluster).await,
        }
    })
    .await
    .map_err(|e| OperatorError::Finalizer(e.to_string()))
}

/// Apply the desired cluster state.
async fn apply(client: Client, cluster: &ChronikCluster) -> Result<Action, OperatorError> {
    let name = cluster.name_any();
    let namespace = cluster.namespace().unwrap_or_else(|| "default".into());
    let spec = &cluster.spec;

    // Validate minimum replicas
    if spec.replicas < constants::defaults::MIN_CLUSTER_REPLICAS {
        let msg = format!(
            "Cluster requires at least {} replicas for Raft quorum, got {}",
            constants::defaults::MIN_CLUSTER_REPLICAS,
            spec.replicas
        );
        warn!(name = %name, "{msg}");
        update_status(
            &client,
            &name,
            &namespace,
            "Failed",
            spec.replicas,
            0,
            None,
            vec![],
            vec![make_condition("Ready", "False", "InvalidConfig", &msg)],
            cluster.metadata.generation,
        )
        .await?;
        return Err(OperatorError::Config(msg));
    }

    let owner_ref = owner_reference(cluster);
    let kafka_port = spec
        .listeners
        .first()
        .map(|l| l.port)
        .unwrap_or(constants::ports::KAFKA);

    info!(name = %name, namespace = %namespace, replicas = spec.replicas, "Reconciling ChronikCluster");

    // 1. Ensure headless Service (for stable DNS)
    let svc_api: Api<Service> = Api::namespaced(client.clone(), &namespace);
    let headless_name = format!("{name}-headless");
    if svc_api.get_opt(&headless_name).await?.is_none() {
        let selector_labels = pod_builder::cluster_label_selector(&name);
        let all_labels = cluster_common_labels(&name, &spec.image);
        let svc = service_builder::build_headless_service(
            &name,
            &namespace,
            kafka_port,
            spec.wal_port,
            spec.raft_port,
            spec.unified_api_port,
            selector_labels,
            all_labels,
            owner_ref.clone(),
        );
        info!(name = %name, "Creating headless Service");
        svc_api.create(&PostParams::default(), &svc).await?;
    }

    // 2. Ensure PodDisruptionBudget
    let pdb_api: Api<PodDisruptionBudget> = Api::namespaced(client.clone(), &namespace);
    let pdb_name = format!("{name}-pdb");
    if pdb_api.get_opt(&pdb_name).await?.is_none() {
        let max_unavailable = spec
            .pod_disruption_budget
            .as_ref()
            .map(|p| p.max_unavailable)
            .unwrap_or(1);
        let selector_labels = pod_builder::cluster_label_selector(&name);
        let pdb = pdb_builder::build_cluster_pdb(
            &name,
            &namespace,
            max_unavailable,
            selector_labels,
            owner_ref.clone(),
        );
        info!(name = %name, "Creating PodDisruptionBudget");
        pdb_api.create(&PostParams::default(), &pdb).await?;
    }

    // 3. Build peer list for all nodes
    let peers = config_generator::build_peer_list(
        &name,
        &namespace,
        spec.replicas,
        kafka_port,
        spec.wal_port,
        spec.raft_port,
    );

    // 4. Ensure per-node resources (Service, PVC, ConfigMap, Pod)
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);

    for node_id in 1..=spec.replicas as u64 {
        let node_labels = pod_builder::cluster_node_labels(&name, node_id, &spec.image);

        // 4a. Per-node Service
        let node_svc_name = format!("{name}-{node_id}");
        if svc_api.get_opt(&node_svc_name).await?.is_none() {
            let svc = service_builder::build_cluster_node_service(
                &name,
                &namespace,
                node_id,
                kafka_port,
                spec.wal_port,
                spec.raft_port,
                spec.unified_api_port,
                node_labels.clone(),
                owner_ref.clone(),
            );
            info!(name = %name, node_id, "Creating per-node Service");
            svc_api.create(&PostParams::default(), &svc).await?;
        }

        // 4b. PVC
        let pvc_name = format!("{name}-{node_id}-data");
        if pvc_api.get_opt(&pvc_name).await?.is_none() {
            let pvc = pvc_builder::build_cluster_node_pvc(
                &name,
                &namespace,
                node_id,
                spec.storage.as_ref(),
                node_labels.clone(),
                owner_ref.clone(),
            );
            info!(name = %name, node_id, "Creating PVC");
            pvc_api.create(&PostParams::default(), &pvc).await?;
        }

        // 4c. ConfigMap with TOML
        let cm_name = format!("{name}-{node_id}-config");
        let toml_config = generate_node_toml(&name, &namespace, node_id, spec, &peers);

        let existing_cm = cm_api.get_opt(&cm_name).await?;
        match existing_cm {
            None => {
                let cm = configmap_builder::build_cluster_node_configmap(
                    &name,
                    &namespace,
                    node_id,
                    &toml_config,
                    node_labels.clone(),
                    owner_ref.clone(),
                );
                info!(name = %name, node_id, "Creating ConfigMap");
                cm_api.create(&PostParams::default(), &cm).await?;
            }
            Some(existing) => {
                // Update if TOML changed (e.g., replicas changed, peers updated)
                let current_toml = existing
                    .data
                    .as_ref()
                    .and_then(|d| d.get("cluster.toml"))
                    .cloned()
                    .unwrap_or_default();
                if current_toml != toml_config {
                    let cm = configmap_builder::build_cluster_node_configmap(
                        &name,
                        &namespace,
                        node_id,
                        &toml_config,
                        node_labels.clone(),
                        owner_ref.clone(),
                    );
                    info!(name = %name, node_id, "Updating ConfigMap (config changed)");
                    cm_api
                        .patch(
                            &cm_name,
                            &PatchParams::apply("chronik-operator"),
                            &Patch::Apply(&cm),
                        )
                        .await?;
                }
            }
        }

        // 4d. Pod
        let pod_name = format!("{name}-{node_id}");
        let existing_pod = pod_api.get_opt(&pod_name).await?;

        let need_create = match &existing_pod {
            Some(pod) => cluster_pod_needs_update(pod, spec),
            None => true,
        };

        if need_create {
            if existing_pod.is_some() {
                info!(name = %name, node_id, "Deleting Pod for recreation (spec changed)");
                pod_api
                    .delete(&pod_name, &Default::default())
                    .await
                    .map(|_| ())
                    .or_else(|e| if is_not_found(&e) { Ok(()) } else { Err(e) })?;
            }

            let pod = pod_builder::build_cluster_node_pod(
                &name,
                &namespace,
                node_id,
                spec,
                owner_ref.clone(),
            );
            info!(name = %name, node_id, "Creating Pod");
            pod_api.create(&PostParams::default(), &pod).await?;
        }
    }

    // 5. Scale down: remove nodes that exceed the desired replica count
    // Look for pods with labels matching our cluster but with node_id > replicas
    let all_pods = pod_api
        .list(&kube::api::ListParams::default().labels(&format!(
            "{}={},{}={}",
            labels::INSTANCE,
            name,
            labels::COMPONENT,
            values::COMPONENT_CLUSTER_NODE
        )))
        .await?;

    for pod in &all_pods.items {
        if let Some(pod_labels) = &pod.metadata.labels {
            if let Some(node_id_str) = pod_labels.get(labels::NODE_ID) {
                if let Ok(node_id) = node_id_str.parse::<u64>() {
                    if node_id > spec.replicas as u64 {
                        let pod_name = pod.name_any();
                        info!(name = %name, node_id, "Removing excess node (scale down)");

                        // Delete Pod, ConfigMap, Service (PVC retained for data safety)
                        let _ = pod_api.delete(&pod_name, &Default::default()).await;
                        let _ = cm_api
                            .delete(&format!("{name}-{node_id}-config"), &Default::default())
                            .await;
                        let _ = svc_api
                            .delete(&format!("{name}-{node_id}"), &Default::default())
                            .await;
                    }
                }
            }
        }
    }

    // 6. Compute status from actual Pod states
    let mut node_statuses = Vec::new();
    let mut ready_count = 0;

    for node_id in 1..=spec.replicas as u64 {
        let pod_name = format!("{name}-{node_id}");
        let (phase, ready) = match pod_api.get_opt(&pod_name).await? {
            Some(pod) => {
                let pod_phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.clone())
                    .unwrap_or_else(|| "Pending".into());

                let is_ready = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .map(|conds| {
                        conds
                            .iter()
                            .any(|c| c.type_ == "Ready" && c.status == "True")
                    })
                    .unwrap_or(false);

                let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());

                node_statuses.push(ClusterNodeStatus {
                    node_id: node_id as i64,
                    pod_name: pod_name.clone(),
                    phase: Some(pod_phase.clone()),
                    ready: is_ready,
                    is_leader: false, // Set below if we can poll health
                    pod_ip,
                });

                if is_ready {
                    ready_count += 1;
                }

                (pod_phase, is_ready)
            }
            None => {
                node_statuses.push(ClusterNodeStatus {
                    node_id: node_id as i64,
                    pod_name: pod_name.clone(),
                    phase: Some("Pending".into()),
                    ready: false,
                    is_leader: false,
                    pod_ip: None,
                });
                ("Pending".to_string(), false)
            }
        };

        let _ = (phase, ready); // used above
    }

    // Determine cluster phase
    let cluster_phase = if ready_count == spec.replicas {
        "Running"
    } else if ready_count > 0 {
        "Degraded"
    } else {
        "Bootstrapping"
    };

    let ready_condition = if ready_count == spec.replicas {
        make_condition(
            "Ready",
            "True",
            "AllNodesReady",
            &format!("All {ready_count} nodes are ready"),
        )
    } else {
        make_condition(
            "Ready",
            "False",
            "NodesNotReady",
            &format!("{ready_count}/{} nodes ready", spec.replicas),
        )
    };

    update_status(
        &client,
        &name,
        &namespace,
        cluster_phase,
        spec.replicas,
        ready_count,
        None, // leader_node_id — would require health polling
        node_statuses,
        vec![ready_condition],
        cluster.metadata.generation,
    )
    .await?;

    // Requeue based on cluster state
    let requeue = match cluster_phase {
        "Running" => Duration::from_secs(constants::defaults::REQUEUE_CLUSTER_RUNNING_SECS),
        "Degraded" => Duration::from_secs(constants::defaults::REQUEUE_CLUSTER_DEGRADED_SECS),
        _ => Duration::from_secs(constants::defaults::REQUEUE_CLUSTER_SCALING_SECS),
    };

    Ok(Action::requeue(requeue))
}

/// Generate TOML cluster config for a specific node.
fn generate_node_toml(
    cluster_name: &str,
    namespace: &str,
    node_id: u64,
    spec: &ChronikClusterSpec,
    peers: &[PeerEntry],
) -> String {
    let kafka_port = spec
        .listeners
        .first()
        .map(|l| l.port)
        .unwrap_or(constants::ports::KAFKA);

    let advertise_host = config_generator::node_dns_name(cluster_name, node_id, namespace);

    let params = NodeConfigParams {
        node_id,
        kafka_port,
        wal_port: spec.wal_port,
        raft_port: spec.raft_port,
        advertise_host,
        replication_factor: spec.replication_factor,
        min_insync_replicas: spec.min_insync_replicas,
        auto_recover: spec.auto_recover,
        peers: peers.to_vec(),
    };

    config_generator::generate_node_config(&params)
}

/// Cleanup on CR deletion.
async fn cleanup(_client: Client, cluster: &ChronikCluster) -> Result<Action, OperatorError> {
    let name = cluster.name_any();
    let namespace = cluster.namespace().unwrap_or_else(|| "default".into());

    info!(name = %name, namespace = %namespace, "Cleaning up ChronikCluster");

    // Owner references handle cascading deletion of Pods, Services, ConfigMaps, PDB.
    // PVCs are also owner-referenced and will be garbage collected.

    Ok(Action::await_change())
}

/// Check if a cluster node Pod needs recreation.
fn cluster_pod_needs_update(pod: &Pod, spec: &ChronikClusterSpec) -> bool {
    let Some(pod_spec) = &pod.spec else {
        return true;
    };

    let Some(container) = pod_spec.containers.first() else {
        return true;
    };

    // Check image
    if container.image.as_deref() != Some(&spec.image) {
        return true;
    }

    // Check key env vars
    if let Some(env) = &container.env {
        let find_env = |name: &str| -> Option<&str> {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.as_deref())
        };

        if find_env("CHRONIK_WAL_PROFILE") != Some(&spec.wal_profile) {
            return true;
        }
        if find_env("CHRONIK_PRODUCE_PROFILE") != Some(&spec.produce_profile) {
            return true;
        }
        if find_env("RUST_LOG") != Some(&spec.log_level) {
            return true;
        }
    }

    false
}

/// Update the ChronikCluster status subresource.
#[allow(clippy::too_many_arguments)]
async fn update_status(
    client: &Client,
    name: &str,
    namespace: &str,
    phase: &str,
    desired_nodes: i32,
    ready_nodes: i32,
    leader_node_id: Option<i64>,
    nodes: Vec<ClusterNodeStatus>,
    conditions: Vec<Condition>,
    generation: Option<i64>,
) -> Result<(), OperatorError> {
    let status = ChronikClusterStatus {
        phase: Some(phase.into()),
        ready_nodes,
        desired_nodes,
        leader_node_id,
        observed_generation: generation,
        listeners: vec![],
        headless_service: Some(format!("{name}-headless.{namespace}.svc.cluster.local")),
        nodes,
        partition_summary: None,
        conditions,
    };

    let status_patch = serde_json::json!({ "status": status });
    let ns_api: Api<ChronikCluster> = Api::namespaced(client.clone(), namespace);
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

/// Build common labels for cluster-level resources (headless Service, PDB).
fn cluster_common_labels(
    cluster_name: &str,
    image: &str,
) -> std::collections::BTreeMap<String, String> {
    let version = image.rsplit(':').next().unwrap_or("latest");
    std::collections::BTreeMap::from([
        (labels::NAME.into(), values::APP_NAME.into()),
        (labels::INSTANCE.into(), cluster_name.into()),
        (
            labels::COMPONENT.into(),
            values::COMPONENT_CLUSTER_NODE.into(),
        ),
        (labels::MANAGED_BY.into(), values::MANAGED_BY.into()),
        (labels::VERSION.into(), version.into()),
    ])
}

/// Build an OwnerReference for a ChronikCluster.
fn owner_reference(
    cluster: &ChronikCluster,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
        api_version: ChronikCluster::api_version(&()).to_string(),
        kind: ChronikCluster::kind(&()).to_string(),
        name: cluster.name_any(),
        uid: cluster.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Error policy — retry transient errors.
fn error_policy(
    _cluster: Arc<ChronikCluster>,
    error: &OperatorError,
    _ctx: Arc<Context>,
) -> Action {
    warn!("Cluster reconciliation error: {error}");
    if error.is_transient() {
        Action::requeue(Duration::from_secs(15))
    } else {
        Action::requeue(Duration::from_secs(300))
    }
}

/// Check if a kube::Error is a 404 Not Found.
fn is_not_found(e: &kube::Error) -> bool {
    matches!(
        e,
        kube::Error::Api(kube::core::ErrorResponse { code: 404, .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_node_toml() {
        let spec: ChronikClusterSpec = serde_json::from_str("{}").unwrap();
        let peers = config_generator::build_peer_list("test", "ns", 3, 9092, 9291, 5001);

        let toml = generate_node_toml("test", "ns", 1, &spec, &peers);

        assert!(toml.contains("node_id = 1"));
        assert!(toml.contains("enabled = true"));
        assert!(toml.contains("replication_factor = 3"));
        assert!(toml.contains("min_insync_replicas = 2"));
        assert!(toml.contains("auto_recover = true"));
        assert!(toml.contains("test-1.ns.svc.cluster.local"));
        assert_eq!(toml.matches("[[peers]]").count(), 3);
    }

    #[test]
    fn test_generate_node_toml_different_nodes() {
        let spec: ChronikClusterSpec = serde_json::from_str("{}").unwrap();
        let peers = config_generator::build_peer_list("prod", "default", 3, 9092, 9291, 5001);

        let toml1 = generate_node_toml("prod", "default", 1, &spec, &peers);
        let toml2 = generate_node_toml("prod", "default", 2, &spec, &peers);

        assert!(toml1.contains("node_id = 1"));
        assert!(toml2.contains("node_id = 2"));
        // Different advertise hosts
        assert!(toml1.contains("prod-1.default.svc.cluster.local"));
        assert!(toml2.contains("prod-2.default.svc.cluster.local"));
    }

    #[test]
    fn test_cluster_common_labels() {
        let labels = cluster_common_labels("prod", "chronik:v2.2.23");
        assert_eq!(labels.get(labels::INSTANCE).unwrap(), "prod");
        assert_eq!(labels.get(labels::COMPONENT).unwrap(), "cluster-node");
        assert_eq!(labels.get(labels::VERSION).unwrap(), "v2.2.23");
    }

    #[test]
    fn test_make_condition() {
        let c = make_condition("Ready", "True", "AllNodesReady", "3/3 nodes ready");
        assert_eq!(c.type_, "Ready");
        assert_eq!(c.status, "True");
        assert_eq!(c.reason.as_deref(), Some("AllNodesReady"));
    }

    #[test]
    fn test_cluster_pod_needs_update_image_change() {
        let pod = Pod {
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers: vec![k8s_openapi::api::core::v1::Container {
                    name: "chronik-server".into(),
                    image: Some("chronik:v2.2.22".into()),
                    env: Some(vec![
                        k8s_openapi::api::core::v1::EnvVar {
                            name: "CHRONIK_WAL_PROFILE".into(),
                            value: Some("medium".into()),
                            ..Default::default()
                        },
                        k8s_openapi::api::core::v1::EnvVar {
                            name: "CHRONIK_PRODUCE_PROFILE".into(),
                            value: Some("balanced".into()),
                            ..Default::default()
                        },
                        k8s_openapi::api::core::v1::EnvVar {
                            name: "RUST_LOG".into(),
                            value: Some("info".into()),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        // Image changed
        let spec: ChronikClusterSpec =
            serde_json::from_str(r#"{"image": "chronik:v2.2.23"}"#).unwrap();
        assert!(cluster_pod_needs_update(&pod, &spec));

        // No change
        let spec: ChronikClusterSpec =
            serde_json::from_str(r#"{"image": "chronik:v2.2.22"}"#).unwrap();
        assert!(!cluster_pod_needs_update(&pod, &spec));
    }
}
