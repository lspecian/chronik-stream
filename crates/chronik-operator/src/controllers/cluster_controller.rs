use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Secret, Service};
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
    generated_inter_broker_secret_name, ChronikCluster, ChronikClusterSpec, ChronikClusterStatus,
    ClusterNodeStatus, InterBrokerSecretSource, GENERATED_INTER_BROKER_SECRET_KEY,
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

    // 3b. Broker-to-broker SASL secret. Must exist before any Pod references
    // it, or the kubelet holds every broker in CreateContainerConfigError.
    if spec.inter_broker_secret_source() == Some(InterBrokerSecretSource::Generated) {
        let secret_api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
        ensure_inter_broker_secret(&secret_api, &name, &namespace, owner_ref.clone()).await?;
    }

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

            let spec_hash = compute_cluster_spec_hash(spec);
            let pod = pod_builder::build_cluster_node_pod(
                &name,
                &namespace,
                node_id,
                spec,
                owner_ref.clone(),
                Some(&spec_hash),
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

    // Which node leads the cluster catalog: each ready pod's unauthenticated
    // `/admin/health` says whether it is the leader. The status used to carry
    // `isLeader: false` on every node and no `leaderNodeId`, because this was
    // never filled in.
    let leader_node_id = poll_leader(&mut node_statuses, spec.unified_api_port).await;

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
        leader_node_id,
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

/// Length of a generated inter-broker secret. Alphanumeric, so ~5.95 bits per
/// character: 48 characters is ~285 bits, far above the broker's 16-character
/// minimum, and safe to paste into a shell or a TOML file.
const GENERATED_INTER_BROKER_SECRET_LEN: usize = 48;

/// Generate an inter-broker secret from the operating system's CSPRNG.
fn generate_inter_broker_secret() -> String {
    use rand::distributions::{Alphanumeric, DistString};
    Alphanumeric.sample_string(&mut rand::rngs::OsRng, GENERATED_INTER_BROKER_SECRET_LEN)
}

/// Build the `<cluster>-inter-broker` Secret holding a fresh random secret.
fn build_inter_broker_secret(
    cluster_name: &str,
    namespace: &str,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> Secret {
    Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(generated_inter_broker_secret_name(cluster_name)),
            namespace: Some(namespace.into()),
            owner_references: Some(vec![owner_ref]),
            labels: Some(BTreeMap::from([
                (labels::MANAGED_BY.into(), values::MANAGED_BY.into()),
                (labels::INSTANCE.into(), cluster_name.into()),
            ])),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from([(
            GENERATED_INTER_BROKER_SECRET_KEY.into(),
            generate_inter_broker_secret(),
        )])),
        type_: Some("Opaque".into()),
        ..Default::default()
    }
}

/// Ensure the generated inter-broker Secret exists. Created once and never
/// rewritten: every broker must hold the same value, so regenerating it would
/// split a running cluster mid-roll — the Pods already started keep the old
/// secret while new ones get the new, and followers can no longer fetch.
async fn ensure_inter_broker_secret(
    secret_api: &Api<Secret>,
    cluster_name: &str,
    namespace: &str,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> Result<(), OperatorError> {
    let secret_name = generated_inter_broker_secret_name(cluster_name);
    if let Some(existing) = secret_api.get_opt(&secret_name).await? {
        // Someone may have edited it. Say so rather than "fix" it: a rewrite is
        // exactly the split described above.
        let len = existing
            .data
            .as_ref()
            .and_then(|d| d.get(GENERATED_INTER_BROKER_SECRET_KEY))
            .map(|v| String::from_utf8_lossy(&v.0).trim().len())
            .unwrap_or(0);
        if len < 16 {
            warn!(
                name = %cluster_name,
                secret = %secret_name,
                key = GENERATED_INTER_BROKER_SECRET_KEY,
                "Inter-broker Secret has no usable value (brokers need at least 16 characters); \
                 brokers with SASL required will refuse to start. Not regenerating it — delete \
                 the Secret to have the operator create a new one, then restart every broker."
            );
        }
        return Ok(());
    }

    let secret = build_inter_broker_secret(cluster_name, namespace, owner_ref);
    info!(name = %cluster_name, secret = %secret_name, "Creating inter-broker SASL Secret");
    match secret_api.create(&PostParams::default(), &secret).await {
        Ok(_) => Ok(()),
        // Another reconcile created it between our get and create. Keep theirs.
        Err(kube::Error::Api(status)) if status.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Compute a deterministic hash of the ChronikCluster spec fields that affect pod config.
/// Used as a pod annotation to detect when the spec has changed and pods need recreation.
fn compute_cluster_spec_hash(spec: &ChronikClusterSpec) -> String {
    cluster_spec_hash_at(spec, POD_SHAPE_GENERATION)
}

/// [`compute_cluster_spec_hash`] at an explicit Pod-shape generation. Split out
/// so a test can prove that bumping the generation really moves the hash — the
/// whole point of the generation is lost if it silently stops participating.
fn cluster_spec_hash_at(spec: &ChronikClusterSpec, pod_shape_generation: u32) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    spec.image.hash(&mut hasher);
    spec.image_pull_policy.hash(&mut hasher);
    spec.wal_profile.hash(&mut hasher);
    spec.produce_profile.hash(&mut hasher);
    spec.log_level.hash(&mut hasher);
    spec.columnar_enabled.hash(&mut hasher);

    // Sort env vars by name for deterministic hashing
    let sorted_env: BTreeMap<_, _> = spec
        .env
        .iter()
        .map(|e| {
            (
                &e.name,
                (
                    e.value.as_deref().unwrap_or(""),
                    e.value_from_secret.as_ref(),
                ),
            )
        })
        .collect();
    for (name, (value, secret_ref)) in &sorted_env {
        name.hash(&mut hasher);
        value.hash(&mut hasher);
        if let Some(sr) = secret_ref {
            sr.name.hash(&mut hasher);
            sr.key.hash(&mut hasher);
        }
    }

    // Include resources
    if let Some(ref res) = spec.resources {
        format!("{:?}", res).hash(&mut hasher);
    }

    // Include vector search config
    if let Some(ref vs) = spec.vector_search {
        vs.enabled.hash(&mut hasher);
        vs.provider.hash(&mut hasher);
    }

    // Include object store config
    if let Some(ref os) = spec.object_store {
        format!("{:?}", os).hash(&mut hasher);
    }

    // Both of these change the generated Pod, so leaving them out means a user
    // can flip them and watch nothing happen until some unrelated edit finally
    // recreates the Pod — at which point the change lands by surprise.
    //
    // Mixed in only when set away from the default, so that adding them here
    // does not change the hash of every cluster that has never used them and
    // turn an operator upgrade into a rebuild of every broker Pod.
    if spec.fix_data_ownership == Some(false) {
        "fix_data_ownership=false".hash(&mut hasher);
    }
    if let Some(ref sc) = spec.pod_security_context {
        format!("{:?}", sc).hash(&mut hasher);
    }

    // The injected CHRONIK_INTER_BROKER_SECRET. Mixed in only when it is
    // injected, which needs SASL enabled in `env` — so a cluster without SASL
    // hashes exactly as before this existed, while a SASL cluster's Pods are
    // rebuilt to carry it (a broker on an image that requires it would
    // otherwise crash-loop the next time its container restarted).
    match spec.inter_broker_secret_source() {
        None => {}
        Some(InterBrokerSecretSource::Generated) => {
            "inter_broker_secret=generated".hash(&mut hasher);
        }
        Some(InterBrokerSecretSource::User(r)) => {
            "inter_broker_secret=user".hash(&mut hasher);
            r.name.hash(&mut hasher);
            r.key.hash(&mut hasher);
        }
    }

    // The shape of the Pod this operator builds, independent of the spec.
    //
    // Everything above is a user-set field: the hash changes when the user
    // changes something. But the operator also decides parts of the Pod the
    // spec says nothing about, and when one of those is *wrong* a fix to it
    // reaches nobody — the hash is unchanged, so no Pod is recreated, and the
    // fix lands only if some unrelated edit happens to rebuild the Pod.
    //
    // Bumping this is the deliberate way to say "the Pod I build has changed in
    // a way existing Pods must pick up", and it costs a rolling restart of every
    // managed cluster. Do not bump it for cosmetics.
    pod_shape_generation.hash(&mut hasher);

    format!("{:016x}", hasher.finish())
}

/// Generation of the operator-decided Pod shape. See `compute_cluster_spec_hash`.
///
/// 2: added a startup probe. Boot opens the Kafka port only after replaying the
///    WAL and metadata catalog, so on a cluster with many topics the liveness
///    probe raced the boot and won — a broker holding ~5,600 topics took 114s to
///    open 9092 against a ~95s liveness budget and was SIGKILLed 5s after
///    becoming ready, 710 times, never serving a request. Brokers already
///    running carry the old probe and are one restart away from the same thing,
///    which is why this has to reach them rather than wait for a future edit.
pub(crate) const POD_SHAPE_GENERATION: u32 = 2;

// Generation 2 is the one that carries the startup probe. Lowering it would put
// the boot crash-loop back, so that is a compile error rather than a test that
// might not be run.
const _: () = assert!(
    POD_SHAPE_GENERATION >= 2,
    "POD_SHAPE_GENERATION below 2 drops the startup probe and reintroduces the boot crash-loop"
);

/// Check if a cluster node Pod needs recreation by comparing spec hash annotation.
fn cluster_pod_needs_update(pod: &Pod, spec: &ChronikClusterSpec) -> bool {
    let desired_hash = compute_cluster_spec_hash(spec);
    let current_hash = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("chronik.io/spec-hash"))
        .map(|s| s.as_str())
        .unwrap_or("");

    if desired_hash != current_hash {
        info!(
            desired = %desired_hash,
            current = %current_hash,
            "Spec hash mismatch, pod needs recreation"
        );
        return true;
    }

    false
}

/// Update the ChronikCluster status subresource.
#[allow(clippy::too_many_arguments)]
/// Ask every ready node with a pod IP whether it leads, in parallel and with a
/// short timeout, mark it in `nodes`, and return its id. A node that cannot be
/// reached is simply not the leader for this pass.
async fn poll_leader(nodes: &mut [ClusterNodeStatus], unified_api_port: i32) -> Option<i64> {
    let admin = crate::admin_client::AdminClient::new(None);
    let probes = nodes.iter().map(|n| {
        let url = match (&n.pod_ip, n.ready) {
            (Some(ip), true) => Some(format!("http://{ip}:{unified_api_port}")),
            _ => None,
        };
        let admin = &admin;
        async move {
            match url {
                Some(url) => tokio::time::timeout(Duration::from_secs(3), admin.health(&url))
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => None,
            }
        }
    });
    let answers = futures::future::join_all(probes).await;
    let reports: Vec<(i64, Option<(u64, bool)>)> = nodes
        .iter()
        .zip(answers)
        .map(|(n, h)| (n.node_id, h.map(|h| (h.node_id, h.is_leader))))
        .collect();
    let leader = pick_leader(&reports);
    for n in nodes.iter_mut() {
        n.is_leader = Some(n.node_id) == leader;
    }
    leader
}

/// The leader among `(node_id, health)` reports: a node that answered for
/// itself (its reported id matches) and says it leads. During an election two
/// nodes can briefly both claim it; the lowest id is taken and the next pass
/// corrects it.
fn pick_leader(reports: &[(i64, Option<(u64, bool)>)]) -> Option<i64> {
    reports
        .iter()
        .filter_map(|(id, h)| match h {
            Some((reported, true)) if *reported as i64 == *id => Some(*id),
            _ => None,
        })
        .min()
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
    matches!(e, kube::Error::Api(status) if status.code == 404)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_that_reports_leading_is_the_leader() {
        let reports = vec![
            (1, Some((1, false))),
            (2, Some((2, false))),
            (3, Some((3, true))),
        ];
        assert_eq!(pick_leader(&reports), Some(3));
    }

    #[test]
    fn no_leader_when_nobody_claims_it_or_nobody_answers() {
        assert_eq!(
            pick_leader(&[(1, Some((1, false))), (2, None), (3, None)]),
            None
        );
        assert_eq!(pick_leader(&[(1, None), (2, None)]), None);
    }

    #[test]
    fn a_claim_for_a_different_node_id_is_ignored() {
        // A pod IP reused by another broker answers with its own id.
        assert_eq!(
            pick_leader(&[(1, Some((2, true))), (2, Some((2, false)))]),
            None
        );
    }

    #[test]
    fn two_claims_during_an_election_take_the_lowest() {
        assert_eq!(
            pick_leader(&[(2, Some((2, true))), (3, Some((3, true)))]),
            Some(2)
        );
    }

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
        // Spec matching the pod's current config
        let matching_spec: ChronikClusterSpec =
            serde_json::from_str(r#"{"image": "chronik:v2.2.22"}"#).unwrap();
        let matching_hash = compute_cluster_spec_hash(&matching_spec);

        let pod = Pod {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                annotations: Some(BTreeMap::from([(
                    "chronik.io/spec-hash".to_string(),
                    matching_hash,
                )])),
                ..Default::default()
            },
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers: vec![k8s_openapi::api::core::v1::Container {
                    name: "chronik-server".into(),
                    image: Some("chronik:v2.2.22".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        // Image changed → needs update
        let spec: ChronikClusterSpec =
            serde_json::from_str(r#"{"image": "chronik:v2.2.23"}"#).unwrap();
        assert!(cluster_pod_needs_update(&pod, &spec));

        // No change → no update needed
        let spec: ChronikClusterSpec =
            serde_json::from_str(r#"{"image": "chronik:v2.2.22"}"#).unwrap();
        assert!(!cluster_pod_needs_update(&pod, &spec));
    }

    #[test]
    fn test_cluster_pod_needs_update_env_change() {
        // Spec with env vars
        let spec_v1: ChronikClusterSpec =
            serde_json::from_str(r#"{"image": "chronik:v2.3.0"}"#).unwrap();
        let hash_v1 = compute_cluster_spec_hash(&spec_v1);

        let pod = Pod {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                annotations: Some(BTreeMap::from([(
                    "chronik.io/spec-hash".to_string(),
                    hash_v1,
                )])),
                ..Default::default()
            },
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers: vec![k8s_openapi::api::core::v1::Container {
                    name: "chronik-server".into(),
                    image: Some("chronik:v2.3.0".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        // Same spec → no update
        assert!(!cluster_pod_needs_update(&pod, &spec_v1));

        // Pod without annotation → always needs update
        let pod_no_annotation = Pod {
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers: vec![k8s_openapi::api::core::v1::Container {
                    name: "chronik-server".into(),
                    image: Some("chronik:v2.3.0".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(cluster_pod_needs_update(&pod_no_annotation, &spec_v1));
    }

    /// The hash is an upgrade contract, not an implementation detail.
    ///
    /// `cluster_pod_needs_update` recreates a broker Pod whenever the computed
    /// hash differs from the Pod's annotation, so changing how the hash is
    /// computed restarts every broker of every managed cluster the moment the
    /// new operator takes leadership — a whole-fleet roll nobody asked for.
    ///
    /// If this test fails you have changed the hash of a spec that sets none of
    /// the new fields. That is the dangerous case: decide deliberately whether
    /// the fleet-wide restart is acceptable, and only then update the constant.
    /// Adding a field is safe as long as it is mixed in only when set away from
    /// its default.
    #[test]
    fn the_default_spec_hash_is_stable_across_operator_versions() {
        let spec: ChronikClusterSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(
            compute_cluster_spec_hash(&spec),
            // Changed once, deliberately, for POD_SHAPE_GENERATION = 2 (the
            // startup probe). The old probe left a data-heavy broker unable to
            // ever finish starting, and brokers already running carry it, so the
            // fix had to reach them: a supervised rolling restart now is better
            // than each cluster discovering the crash-loop at its next restart.
            // Was "4a01ffeff1524553" at generation 1.
            //
            // Changed again, deliberately, when the default image moved from
            // ghcr.io/chronik-stream/chronik-server:latest, which was never
            // published, to ghcr.io/lspecian/chronik-stream:latest. A cluster
            // that left `image` unset pulled nothing and has no running broker
            // to restart; recreating its Pods is what fixes it. Clusters that
            // set `image` (all known ones) keep their hash.
            // Was "a754f5cb9f9a4558" with the unpublished default.
            "d0307e0bbda03cd2",
            "the hash of a default spec changed — every broker Pod under this \
             operator would be recreated on upgrade"
        );
    }

    /// The escape hatch has to actually work: bumping the generation must move
    /// the hash, or a future operator-side Pod fix will silently reach nobody.
    #[test]
    fn the_pod_shape_generation_is_mixed_into_the_hash() {
        let spec: ChronikClusterSpec = serde_json::from_str("{}").unwrap();

        // Same spec, only the generation differs: the hash must move, or a bump
        // recreates nothing and the operator-side fix reaches no existing Pod.
        assert_ne!(
            cluster_spec_hash_at(&spec, POD_SHAPE_GENERATION),
            cluster_spec_hash_at(&spec, POD_SHAPE_GENERATION + 1),
            "bumping POD_SHAPE_GENERATION must change the hash"
        );

        // And the public entry point is that function at the current generation,
        // not some other computation that happens to look similar.
        assert_eq!(
            compute_cluster_spec_hash(&spec),
            cluster_spec_hash_at(&spec, POD_SHAPE_GENERATION),
        );
    }

    /// ...but a field that genuinely changes the Pod must change the hash, or
    /// flipping it does nothing until something unrelated recreates the Pod.
    #[test]
    fn opting_out_of_the_ownership_repair_changes_the_hash() {
        let default: ChronikClusterSpec = serde_json::from_str("{}").unwrap();
        let opted_out: ChronikClusterSpec =
            serde_json::from_str(r#"{"fixDataOwnership": false}"#).unwrap();

        assert_ne!(
            compute_cluster_spec_hash(&default),
            compute_cluster_spec_hash(&opted_out),
            "fixDataOwnership: false removes a container from the Pod, so it has \
             to be visible to the recreate decision"
        );

        let explicit_on: ChronikClusterSpec =
            serde_json::from_str(r#"{"fixDataOwnership": true}"#).unwrap();
        assert_eq!(
            compute_cluster_spec_hash(&default),
            compute_cluster_spec_hash(&explicit_on),
            "spelling out the default builds the same Pod, so it must not force \
             a restart"
        );
    }

    /// Inter-broker secret support must not roll a single Pod of a cluster
    /// without SASL. The golden values were computed by the operator BEFORE
    /// `interBrokerSecret` existed (same POD_SHAPE_GENERATION), so a change here
    /// means every non-SASL broker under the operator would be recreated.
    #[test]
    fn inter_broker_secret_support_leaves_non_sasl_hashes_unchanged() {
        let cases = [
            // Default image corrected; see the_default_spec_hash_is_stable_across_operator_versions.
            ("{}", "d0307e0bbda03cd2"),
            // Explicitly disabled SASL, plus unrelated env and an admin key.
            (
                r#"{"image":"chronik-server:v2.16.0","walProfile":"high",
                    "env":[{"name":"CHRONIK_HOT_TEXT_MAX_DOCS","value":"2000"},
                           {"name":"CHRONIK_SASL_ENABLED","value":"false"}],
                    "adminApiKeySecret":{"name":"adm","key":"k"}}"#,
                "a3c20af37cd8e0b9",
            ),
            // interBrokerSecret set but SASL off: nothing is injected (a broker
            // with SASL disabled cannot accept the broker login), so the Pod and
            // the hash are what they were.
            (
                r#"{"image":"chronik-server:v2.16.0","walProfile":"high",
                    "env":[{"name":"CHRONIK_HOT_TEXT_MAX_DOCS","value":"2000"},
                           {"name":"CHRONIK_SASL_ENABLED","value":"false"}],
                    "adminApiKeySecret":{"name":"adm","key":"k"},
                    "interBrokerSecret":{"name":"mine","key":"s"}}"#,
                "a3c20af37cd8e0b9",
            ),
        ];
        for (json, golden) in cases {
            let spec: ChronikClusterSpec = serde_json::from_str(json).unwrap();
            assert_eq!(spec.inter_broker_secret_source(), None, "spec: {json}");
            assert_eq!(
                compute_cluster_spec_hash(&spec),
                golden,
                "a non-SASL cluster's hash moved — upgrading the operator would \
                 recreate its brokers. spec: {json}"
            );
        }
    }

    /// A SASL cluster's Pods gain CHRONIK_INTER_BROKER_SECRET, so its hash has
    /// to move or existing brokers never pick it up — and the next container
    /// restart on an image that requires it crash-loops.
    #[test]
    fn a_sasl_cluster_hash_moves_to_carry_the_inter_broker_secret() {
        let generated: ChronikClusterSpec = serde_json::from_str(
            r#"{"image":"chronik-server:v2.16.0",
                "env":[{"name":"CHRONIK_SASL_ENABLED","value":"required"}]}"#,
        )
        .unwrap();
        // The pre-change operator's hash for this exact spec.
        assert_ne!(compute_cluster_spec_hash(&generated), "1be857562a7bc6b3");

        let user: ChronikClusterSpec = serde_json::from_str(
            r#"{"image":"chronik-server:v2.16.0",
                "env":[{"name":"CHRONIK_SASL_ENABLED","value":"required"}],
                "interBrokerSecret":{"name":"mine","key":"s"}}"#,
        )
        .unwrap();
        let user_other_key: ChronikClusterSpec = serde_json::from_str(
            r#"{"image":"chronik-server:v2.16.0",
                "env":[{"name":"CHRONIK_SASL_ENABLED","value":"required"}],
                "interBrokerSecret":{"name":"mine","key":"other"}}"#,
        )
        .unwrap();
        let hashes = [
            compute_cluster_spec_hash(&generated),
            compute_cluster_spec_hash(&user),
            compute_cluster_spec_hash(&user_other_key),
        ];
        assert_ne!(hashes[0], hashes[1], "switching to a user secret must roll");
        assert_ne!(hashes[1], hashes[2], "pointing at another key must roll");
    }

    #[test]
    fn the_generated_inter_broker_secret_is_owned_random_and_long_enough() {
        let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "chronik.io/v1alpha1".into(),
            kind: "ChronikCluster".into(),
            name: "prod".into(),
            uid: "uid-1".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        };
        let a = build_inter_broker_secret("prod", "ns", owner.clone());
        let b = build_inter_broker_secret("prod", "ns", owner);

        assert_eq!(a.metadata.name.as_deref(), Some("prod-inter-broker"));
        assert_eq!(a.metadata.namespace.as_deref(), Some("ns"));
        let owners = a.metadata.owner_references.as_ref().unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].kind, "ChronikCluster");
        assert_eq!(owners[0].uid, "uid-1");

        let value = |s: &Secret| s.string_data.as_ref().unwrap()["secret"].clone();
        let va = value(&a);
        assert_eq!(va.len(), GENERATED_INTER_BROKER_SECRET_LEN);
        assert!(
            va.len() >= 32,
            "must comfortably exceed the broker's 16-char minimum"
        );
        assert!(va.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(va, value(&b), "each generation must be fresh randomness");
    }

    /// Operational check: compute the spec hash of a live cluster's `.spec` JSON.
    ///
    /// Deploying a new operator build must not silently recreate every broker
    /// pod, and `cluster_pod_needs_update` decides that purely on this hash.
    /// Point it at a spec dumped from a running cluster and compare against the
    /// pod's `chronik.io/spec-hash` annotation before rolling an operator out:
    ///
    /// ```text
    /// kubectl get chronikcluster <name> -n <ns> -o jsonpath='{.spec}' > spec.json
    /// CHRONIK_SPEC_JSON=spec.json cargo test -p chronik-operator \
    ///     hash_of_a_live_spec -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "operational tool: needs CHRONIK_SPEC_JSON pointing at a live spec"]
    fn hash_of_a_live_spec() {
        let path = std::env::var("CHRONIK_SPEC_JSON")
            .expect("set CHRONIK_SPEC_JSON to a file holding a ChronikCluster .spec");
        let raw = std::fs::read_to_string(&path).expect("spec file should be readable");
        let spec: ChronikClusterSpec =
            serde_json::from_str(&raw).expect("spec JSON should deserialize");
        println!("{}\t{}", compute_cluster_spec_hash(&spec), path);
    }
}
