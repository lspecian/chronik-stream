use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Service};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{finalizer, Event as Finalizer};
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::constants::{self, FINALIZER};
use crate::crds::common::Condition;
use crate::crds::standalone::{ChronikStandalone, ChronikStandaloneStatus};
use crate::error::OperatorError;
use crate::resources::{configmap_builder, pod_builder, pvc_builder, service_builder};

/// Shared context for the standalone reconciler.
pub struct Context {
    pub client: Client,
}

/// Start the standalone controller.
pub async fn run(client: Client) {
    let api: Api<ChronikStandalone> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::all(client.clone());
    let services: Api<Service> = Api::all(client.clone());
    let configmaps: Api<ConfigMap> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        client: client.clone(),
    });

    Controller::new(api, watcher::Config::default())
        .owns(pods, watcher::Config::default())
        .owns(pvcs, watcher::Config::default())
        .owns(services, watcher::Config::default())
        .owns(configmaps, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => {
                    error!("Reconciliation error: {:?}", e);
                }
            }
        })
        .await;
}

/// Main reconciliation function.
async fn reconcile(
    csa: Arc<ChronikStandalone>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let client = &ctx.client;
    let namespace = csa.namespace().unwrap_or_else(|| "default".into());

    let ns_api: Api<ChronikStandalone> = Api::namespaced(client.clone(), &namespace);

    finalizer(&ns_api, FINALIZER, csa, |event| async {
        match event {
            Finalizer::Apply(csa) => apply(client.clone(), &csa).await,
            Finalizer::Cleanup(csa) => cleanup(client.clone(), &csa).await,
        }
    })
    .await
    .map_err(|e| OperatorError::Finalizer(e.to_string()))
}

/// Apply the desired state — create or update PVC, ConfigMap, Pod, Service, and status.
async fn apply(client: Client, csa: &ChronikStandalone) -> Result<Action, OperatorError> {
    let name = csa.name_any();
    let namespace = csa.namespace().unwrap_or_else(|| "default".into());
    let spec = &csa.spec;

    let owner_ref = owner_reference(csa);
    let labels = pod_builder::standalone_labels(&name, &spec.image);

    info!(name = %name, namespace = %namespace, "Reconciling ChronikStandalone");

    // 1. Ensure PVC
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let pvc_name = format!("{name}-data");
    if pvc_api.get_opt(&pvc_name).await?.is_none() {
        let pvc = pvc_builder::build_standalone_pvc(
            &name,
            &namespace,
            spec.storage.as_ref(),
            labels.clone(),
            owner_ref.clone(),
        );
        info!(name = %name, "Creating PVC {pvc_name}");
        pvc_api.create(&PostParams::default(), &pvc).await?;
    }

    // 2. Ensure ConfigMap
    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
    let cm_name = format!("{name}-config");
    if cm_api.get_opt(&cm_name).await?.is_none() {
        let cm = configmap_builder::build_standalone_configmap(
            &name,
            &namespace,
            labels.clone(),
            owner_ref.clone(),
        );
        info!(name = %name, "Creating ConfigMap {cm_name}");
        cm_api.create(&PostParams::default(), &cm).await?;
    }

    // 3. Ensure Service
    let svc_api: Api<Service> = Api::namespaced(client.clone(), &namespace);
    if svc_api.get_opt(&name).await?.is_none() {
        let svc = service_builder::build_standalone_service(
            &name,
            &namespace,
            spec.kafka_port,
            spec.unified_api_port,
            &spec.service_type,
            labels.clone(),
            spec.service_annotations.clone(),
            owner_ref.clone(),
        );
        info!(name = %name, "Creating Service");
        svc_api.create(&PostParams::default(), &svc).await?;
    }

    // 4. Ensure Pod — create if missing, recreate if spec changed
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let existing_pod = pod_api.get_opt(&name).await?;

    let need_recreate = match &existing_pod {
        Some(pod) => pod_needs_update(pod, spec),
        None => true,
    };

    if need_recreate {
        // Delete existing pod if present (it will be recreated)
        if existing_pod.is_some() {
            info!(name = %name, "Deleting Pod for recreation (spec changed)");
            pod_api
                .delete(&name, &Default::default())
                .await
                .map(|_| ())
                .or_else(|e| if is_not_found(&e) { Ok(()) } else { Err(e) })?;
        }

        let pod = pod_builder::build_standalone_pod(&name, &namespace, spec, owner_ref.clone());
        info!(name = %name, "Creating Pod");
        pod_api.create(&PostParams::default(), &pod).await?;
    }

    // 5. Update status
    let current_pod = pod_api.get_opt(&name).await?;
    let (phase, healthy) = match &current_pod {
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

            if pod_phase == "Running" && is_ready {
                ("Running".to_string(), true)
            } else if pod_phase == "Failed" {
                ("Failed".to_string(), false)
            } else {
                ("Pending".to_string(), false)
            }
        }
        None => ("Pending".to_string(), false),
    };

    let kafka_address = format!("{name}.{namespace}.svc.cluster.local:{}", spec.kafka_port);
    let unified_api_address = format!(
        "{name}.{namespace}.svc.cluster.local:{}",
        spec.unified_api_port
    );

    let generation = csa.metadata.generation;
    let condition = Condition {
        type_: "Ready".into(),
        status: if healthy { "True" } else { "False" }.into(),
        reason: Some(if healthy { "PodRunning" } else { "PodNotReady" }.into()),
        message: Some(if healthy {
            "Chronik standalone is healthy".into()
        } else {
            format!("Pod is in {phase} state")
        }),
        last_transition_time: None,
    };

    let status = ChronikStandaloneStatus {
        phase: Some(phase.clone()),
        healthy,
        kafka_address: Some(kafka_address),
        unified_api_address: Some(unified_api_address),
        observed_generation: generation,
        conditions: vec![condition],
    };

    let status_patch = serde_json::json!({ "status": status });
    let ns_api: Api<ChronikStandalone> = Api::namespaced(client.clone(), &namespace);
    ns_api
        .patch_status(
            &name,
            &PatchParams::apply("chronik-operator"),
            &Patch::Merge(&status_patch),
        )
        .await?;

    // Requeue interval based on health
    let requeue = if healthy {
        Duration::from_secs(constants::defaults::REQUEUE_HEALTHY_SECS)
    } else {
        Duration::from_secs(constants::defaults::REQUEUE_NOT_READY_SECS)
    };

    Ok(Action::requeue(requeue))
}

/// Cleanup on CR deletion.
async fn cleanup(_client: Client, csa: &ChronikStandalone) -> Result<Action, OperatorError> {
    let name = csa.name_any();
    let namespace = csa.namespace().unwrap_or_else(|| "default".into());

    info!(name = %name, namespace = %namespace, "Cleaning up ChronikStandalone");

    // Owner references handle cascading deletion of Pod, Service, ConfigMap.
    // PVC is also owner-referenced, so it will be garbage collected.
    // If we want to retain PVCs on deletion, we would remove the owner reference
    // from the PVC here and skip deletion.

    Ok(Action::await_change())
}

/// Check if a Pod needs to be recreated due to spec changes.
fn pod_needs_update(pod: &Pod, spec: &crate::crds::standalone::ChronikStandaloneSpec) -> bool {
    let Some(pod_spec) = &pod.spec else {
        return true;
    };

    let Some(container) = pod_spec.containers.first() else {
        return true;
    };

    // Check image change
    if container.image.as_deref() != Some(&spec.image) {
        return true;
    }

    // Check resource changes
    if let Some(ref requested) = spec.resources {
        let current_resources = &container.resources;
        match current_resources {
            Some(cr) => {
                if let Some(ref req) = requested.requests {
                    let cur_req = cr.requests.as_ref();
                    if let Some(ref cpu) = req.cpu {
                        if cur_req.and_then(|r| r.get("cpu")).map(|q| &q.0) != Some(cpu) {
                            return true;
                        }
                    }
                    if let Some(ref mem) = req.memory {
                        if cur_req.and_then(|r| r.get("memory")).map(|q| &q.0) != Some(mem) {
                            return true;
                        }
                    }
                }
            }
            None if requested.requests.is_some() || requested.limits.is_some() => return true,
            _ => {}
        }
    }

    // Check env var changes (simplified — compare key env vars)
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

/// Build an OwnerReference for a ChronikStandalone.
fn owner_reference(
    csa: &ChronikStandalone,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
        api_version: ChronikStandalone::api_version(&()).to_string(),
        kind: ChronikStandalone::kind(&()).to_string(),
        name: csa.name_any(),
        uid: csa.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Error policy — retry transient errors, skip permanent ones.
fn error_policy(_csa: Arc<ChronikStandalone>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    warn!("Reconciliation error: {error}");
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
