use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{finalizer, Event as Finalizer};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use tracing::{error, info, warn};

use crate::constants::{self, FINALIZER};
use crate::crds::cluster::ChronikCluster;
use crate::crds::common::Condition;
use crate::crds::standalone::ChronikStandalone;
use crate::crds::topic::{ChronikTopic, ChronikTopicStatus};
use crate::error::OperatorError;
use crate::kafka_client::{self, CreateTopicRequest, KafkaAdminClient};

/// Shared context for the topic reconciler.
pub struct Context {
    pub client: Client,
}

/// Start the topic controller.
pub async fn run(client: Client) {
    let api: Api<ChronikTopic> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        client: client.clone(),
    });

    Controller::new(api, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => {
                    error!("Topic reconciliation error: {:?}", e);
                }
            }
        })
        .await;
}

/// Main reconciliation function.
async fn reconcile(topic: Arc<ChronikTopic>, ctx: Arc<Context>) -> Result<Action, OperatorError> {
    let client = &ctx.client;
    let namespace = topic.namespace().unwrap_or_else(|| "default".into());

    let ns_api: Api<ChronikTopic> = Api::namespaced(client.clone(), &namespace);

    finalizer(&ns_api, FINALIZER, topic, |event| async {
        match event {
            Finalizer::Apply(topic) => apply(client.clone(), &topic).await,
            Finalizer::Cleanup(topic) => cleanup(client.clone(), &topic).await,
        }
    })
    .await
    .map_err(|e| OperatorError::Finalizer(e.to_string()))
}

/// Apply the desired topic state.
async fn apply(client: Client, topic: &ChronikTopic) -> Result<Action, OperatorError> {
    let cr_name = topic.name_any();
    let namespace = topic.namespace().unwrap_or_else(|| "default".into());
    let spec = &topic.spec;
    let topic_name = spec.topic_name.as_deref().unwrap_or(&cr_name);

    info!(cr = %cr_name, topic = %topic_name, "Reconciling ChronikTopic");

    // Resolve clusterRef to get admin URL and API key
    let (admin_url, api_key) = resolve_cluster_ref(&client, &namespace, &spec.cluster_ref).await?;

    let admin_client = KafkaAdminClient::new(api_key);

    // Check if topic exists
    let existing = admin_client.describe_topic(&admin_url, topic_name).await;

    match existing {
        Ok(Some(desc)) => {
            // Topic exists — check for updates
            let mut updated = false;

            // Partition increase (never decrease)
            if spec.partitions > desc.partitions {
                info!(
                    topic = %topic_name,
                    current = desc.partitions,
                    desired = spec.partitions,
                    "Increasing partitions"
                );
                admin_client
                    .increase_partitions(&admin_url, topic_name, spec.partitions)
                    .await?;
                updated = true;
            } else if spec.partitions < desc.partitions {
                warn!(
                    topic = %topic_name,
                    current = desc.partitions,
                    desired = spec.partitions,
                    "Cannot decrease partitions (Kafka semantics)"
                );
            }

            // Config updates
            let desired_config = build_topic_config(spec);
            if !desired_config.is_empty() {
                let config_changed = desired_config
                    .iter()
                    .any(|(k, v)| desc.config.get(k).map(|cv| cv != v).unwrap_or(true));

                if config_changed {
                    info!(topic = %topic_name, "Updating topic config");
                    admin_client
                        .update_topic_config(&admin_url, topic_name, &desired_config)
                        .await?;
                    updated = true;
                }
            }

            let phase = if updated { "Updating" } else { "Ready" };
            let actual_partitions = if spec.partitions > desc.partitions {
                spec.partitions
            } else {
                desc.partitions
            };

            update_status(
                &client,
                &cr_name,
                &namespace,
                phase,
                Some(actual_partitions),
                Some(desc.replication_factor),
                desc.topic_id.clone(),
                vec![make_condition(
                    "Ready",
                    "True",
                    "TopicExists",
                    &format!("Topic '{topic_name}' exists with {actual_partitions} partitions"),
                )],
                topic.metadata.generation,
            )
            .await?;
        }
        Ok(None) => {
            // Topic doesn't exist — create it
            info!(topic = %topic_name, partitions = spec.partitions, rf = spec.replication_factor, "Creating topic");

            let config = build_topic_config(spec);
            let request = CreateTopicRequest {
                name: topic_name.to_string(),
                partitions: spec.partitions,
                replication_factor: spec.replication_factor,
                config,
            };

            let resp = admin_client.create_topic(&admin_url, &request).await?;

            let phase = if resp.success { "Ready" } else { "Error" };
            let condition = if resp.success {
                make_condition(
                    "Ready",
                    "True",
                    "TopicCreated",
                    &format!(
                        "Topic '{topic_name}' created with {} partitions",
                        spec.partitions
                    ),
                )
            } else {
                make_condition("Ready", "False", "CreateFailed", &resp.message)
            };

            update_status(
                &client,
                &cr_name,
                &namespace,
                phase,
                Some(spec.partitions),
                Some(spec.replication_factor),
                resp.topic_id,
                vec![condition],
                topic.metadata.generation,
            )
            .await?;
        }
        Err(e) => {
            // Failed to reach admin API
            warn!(topic = %topic_name, "Failed to describe topic: {e}");
            update_status(
                &client,
                &cr_name,
                &namespace,
                "Pending",
                None,
                None,
                None,
                vec![make_condition(
                    "Ready",
                    "False",
                    "AdminApiUnreachable",
                    &format!("Cannot reach admin API: {e}"),
                )],
                topic.metadata.generation,
            )
            .await?;

            // Requeue quickly to retry
            return Ok(Action::requeue(Duration::from_secs(
                constants::defaults::REQUEUE_NOT_READY_SECS,
            )));
        }
    }

    Ok(Action::requeue(Duration::from_secs(
        constants::defaults::REQUEUE_TOPIC_SECS,
    )))
}

/// Cleanup on CR deletion.
async fn cleanup(client: Client, topic: &ChronikTopic) -> Result<Action, OperatorError> {
    let cr_name = topic.name_any();
    let namespace = topic.namespace().unwrap_or_else(|| "default".into());
    let spec = &topic.spec;
    let topic_name = spec.topic_name.as_deref().unwrap_or(&cr_name);

    // Check deletion policy
    let deletion_policy = spec.deletion_policy.as_deref().unwrap_or("delete");
    if deletion_policy == "retain" {
        info!(topic = %topic_name, "Retaining topic (deletionPolicy=retain)");
        return Ok(Action::await_change());
    }

    info!(topic = %topic_name, "Deleting topic");

    let resolve_result = resolve_cluster_ref(&client, &namespace, &spec.cluster_ref).await;

    match resolve_result {
        Ok((admin_url, api_key)) => {
            let admin_client = KafkaAdminClient::new(api_key);
            match admin_client.delete_topic(&admin_url, topic_name).await {
                Ok(_) => info!(topic = %topic_name, "Topic deleted"),
                Err(e) => warn!(topic = %topic_name, "Failed to delete topic: {e}"),
            }
        }
        Err(e) => {
            warn!(topic = %topic_name, "Cannot resolve cluster for cleanup: {e}");
            // Don't block finalizer removal if cluster is gone
        }
    }

    Ok(Action::await_change())
}

/// Build the topic config map from the CRD spec.
fn build_topic_config(spec: &crate::crds::topic::ChronikTopicSpec) -> BTreeMap<String, String> {
    let mut config = spec.config.clone().unwrap_or_default();

    // Columnar storage config
    if let Some(ref col) = spec.columnar {
        if col.enabled {
            config.insert("columnar.enabled".into(), "true".into());
            config.insert("columnar.format".into(), col.format.clone());
            config.insert("columnar.compression".into(), col.compression.clone());
            config.insert("columnar.partitioning".into(), col.partitioning.clone());
        }
    }

    // Vector search config
    if let Some(ref vs) = spec.vector_search {
        if vs.enabled {
            config.insert("vector.enabled".into(), "true".into());
            config.insert(
                "vector.embedding.provider".into(),
                vs.embedding_provider.clone(),
            );
            config.insert("vector.embedding.model".into(), vs.embedding_model.clone());
            config.insert("vector.field".into(), vs.field.clone());
            config.insert("vector.index.metric".into(), vs.index_metric.clone());
        }
    }

    // Full-text search
    if spec.searchable {
        config.insert("searchable".into(), "true".into());
    }

    config
}

/// Resolve a ClusterRef to an admin API URL and optional API key.
///
/// Public so that other controllers (user_controller) can reuse this logic.
pub async fn resolve_cluster_ref(
    client: &Client,
    namespace: &str,
    cluster_ref: &crate::crds::topic::ClusterRef,
) -> Result<(String, Option<String>), OperatorError> {
    let kind = cluster_ref.kind.as_deref().unwrap_or("ChronikCluster");

    match kind {
        "ChronikCluster" => {
            let api: Api<ChronikCluster> =
                Api::namespaced(client.clone(), namespace);
            let cluster = api.get(&cluster_ref.name).await.map_err(|e| {
                OperatorError::NotFound(format!(
                    "ChronikCluster '{}' not found: {e}",
                    cluster_ref.name
                ))
            })?;

            let admin_url =
                kafka_client::cluster_admin_url(&cluster_ref.name, namespace);

            // Get API key from the cluster spec's admin_api_key_secret
            // The actual key resolution from the Secret would be done here.
            // For now, we pass None — the admin API key is injected via env var on the pods.
            let _ = cluster.spec.admin_api_key_secret;

            Ok((admin_url, None))
        }
        "ChronikStandalone" => {
            let api: Api<ChronikStandalone> =
                Api::namespaced(client.clone(), namespace);
            let _standalone = api.get(&cluster_ref.name).await.map_err(|e| {
                OperatorError::NotFound(format!(
                    "ChronikStandalone '{}' not found: {e}",
                    cluster_ref.name
                ))
            })?;

            let admin_url =
                kafka_client::standalone_admin_url(&cluster_ref.name, namespace);

            Ok((admin_url, None))
        }
        other => Err(OperatorError::Config(format!(
            "Unsupported clusterRef kind: '{other}'. Expected 'ChronikCluster' or 'ChronikStandalone'"
        ))),
    }
}

/// Update the ChronikTopic status subresource.
#[allow(clippy::too_many_arguments)]
async fn update_status(
    client: &Client,
    name: &str,
    namespace: &str,
    phase: &str,
    current_partitions: Option<i32>,
    current_replication_factor: Option<i32>,
    topic_id: Option<String>,
    conditions: Vec<Condition>,
    generation: Option<i64>,
) -> Result<(), OperatorError> {
    let status = ChronikTopicStatus {
        phase: Some(phase.into()),
        observed_generation: generation,
        current_partitions,
        current_replication_factor,
        topic_id,
        conditions,
    };

    let status_patch = serde_json::json!({ "status": status });
    let ns_api: Api<ChronikTopic> = Api::namespaced(client.clone(), namespace);
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
fn error_policy(_topic: Arc<ChronikTopic>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    warn!("Topic reconciliation error: {error}");
    if error.is_transient() {
        Action::requeue(Duration::from_secs(15))
    } else {
        Action::requeue(Duration::from_secs(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::topic::{
        ChronikTopicSpec, ClusterRef, ColumnarConfig, TopicVectorSearchConfig,
    };

    #[test]
    fn test_build_topic_config_empty() {
        let spec = ChronikTopicSpec {
            cluster_ref: ClusterRef {
                name: "test".into(),
                kind: None,
            },
            topic_name: None,
            partitions: 1,
            replication_factor: 3,
            config: None,
            columnar: None,
            vector_search: None,
            searchable: false,
            deletion_policy: None,
        };
        let config = build_topic_config(&spec);
        assert!(config.is_empty());
    }

    #[test]
    fn test_build_topic_config_with_user_config() {
        let spec = ChronikTopicSpec {
            cluster_ref: ClusterRef {
                name: "test".into(),
                kind: None,
            },
            topic_name: None,
            partitions: 1,
            replication_factor: 3,
            config: Some(BTreeMap::from([
                ("retention.ms".into(), "604800000".into()),
                ("cleanup.policy".into(), "delete".into()),
            ])),
            columnar: None,
            vector_search: None,
            searchable: false,
            deletion_policy: None,
        };
        let config = build_topic_config(&spec);
        assert_eq!(config.get("retention.ms").unwrap(), "604800000");
        assert_eq!(config.get("cleanup.policy").unwrap(), "delete");
    }

    #[test]
    fn test_build_topic_config_with_columnar() {
        let spec = ChronikTopicSpec {
            cluster_ref: ClusterRef {
                name: "test".into(),
                kind: None,
            },
            topic_name: None,
            partitions: 6,
            replication_factor: 3,
            config: None,
            columnar: Some(ColumnarConfig {
                enabled: true,
                format: "parquet".into(),
                compression: "zstd".into(),
                partitioning: "daily".into(),
            }),
            vector_search: None,
            searchable: true,
            deletion_policy: None,
        };
        let config = build_topic_config(&spec);
        assert_eq!(config.get("columnar.enabled").unwrap(), "true");
        assert_eq!(config.get("columnar.format").unwrap(), "parquet");
        assert_eq!(config.get("columnar.compression").unwrap(), "zstd");
        assert_eq!(config.get("searchable").unwrap(), "true");
    }

    #[test]
    fn test_build_topic_config_with_vector_search() {
        let spec = ChronikTopicSpec {
            cluster_ref: ClusterRef {
                name: "test".into(),
                kind: None,
            },
            topic_name: None,
            partitions: 1,
            replication_factor: 3,
            config: None,
            columnar: None,
            vector_search: Some(TopicVectorSearchConfig {
                enabled: true,
                embedding_provider: "openai".into(),
                embedding_model: "text-embedding-3-small".into(),
                field: "value".into(),
                index_metric: "cosine".into(),
            }),
            searchable: false,
            deletion_policy: None,
        };
        let config = build_topic_config(&spec);
        assert_eq!(config.get("vector.enabled").unwrap(), "true");
        assert_eq!(config.get("vector.embedding.provider").unwrap(), "openai");
        assert_eq!(config.get("vector.index.metric").unwrap(), "cosine");
    }

    #[test]
    fn test_build_topic_config_disabled_features_excluded() {
        let spec = ChronikTopicSpec {
            cluster_ref: ClusterRef {
                name: "test".into(),
                kind: None,
            },
            topic_name: None,
            partitions: 1,
            replication_factor: 3,
            config: None,
            columnar: Some(ColumnarConfig {
                enabled: false,
                format: "parquet".into(),
                compression: "zstd".into(),
                partitioning: "daily".into(),
            }),
            vector_search: Some(TopicVectorSearchConfig {
                enabled: false,
                embedding_provider: "openai".into(),
                embedding_model: "text-embedding-3-small".into(),
                field: "value".into(),
                index_metric: "cosine".into(),
            }),
            searchable: false,
            deletion_policy: None,
        };
        let config = build_topic_config(&spec);
        // Disabled features should not add config entries
        assert!(config.is_empty());
    }
}
