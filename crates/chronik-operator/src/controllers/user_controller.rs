use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{finalizer, Event as Finalizer};
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::constants::{self, FINALIZER};
use crate::crds::common::Condition;
use crate::crds::user::{ChronikUser, ChronikUserStatus};
use crate::error::OperatorError;
use crate::kafka_client::{self, CreateUserRequest, KafkaAdminClient, SetAclsRequest};

/// Shared context for the user reconciler.
pub struct Context {
    pub client: Client,
}

/// Start the user controller.
pub async fn run(client: Client) {
    let api: Api<ChronikUser> = Api::all(client.clone());
    let secrets: Api<Secret> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        client: client.clone(),
    });

    Controller::new(api, watcher::Config::default())
        .owns(secrets, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => {
                    error!("User reconciliation error: {:?}", e);
                }
            }
        })
        .await;
}

/// Main reconciliation function.
async fn reconcile(user: Arc<ChronikUser>, ctx: Arc<Context>) -> Result<Action, OperatorError> {
    let client = &ctx.client;
    let namespace = user.namespace().unwrap_or_else(|| "default".into());

    let ns_api: Api<ChronikUser> = Api::namespaced(client.clone(), &namespace);

    finalizer(&ns_api, FINALIZER, user, |event| async {
        match event {
            Finalizer::Apply(user) => apply(client.clone(), &user).await,
            Finalizer::Cleanup(user) => cleanup(client.clone(), &user).await,
        }
    })
    .await
    .map_err(|e| OperatorError::Finalizer(e.to_string()))
}

/// Apply the desired user state.
async fn apply(client: Client, user: &ChronikUser) -> Result<Action, OperatorError> {
    let cr_name = user.name_any();
    let namespace = user.namespace().unwrap_or_else(|| "default".into());
    let spec = &user.spec;

    info!(user = %cr_name, "Reconciling ChronikUser");

    // 1. Ensure credentials Secret exists
    let secret_api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
    let (secret_name, password) =
        ensure_credentials_secret(&secret_api, &cr_name, spec, user).await?;

    // 2. Resolve clusterRef to get admin URL
    let resolve_result = crate::controllers::topic_controller::resolve_cluster_ref(
        &client,
        &namespace,
        &spec.cluster_ref,
    )
    .await;

    match resolve_result {
        Ok((admin_url, api_key)) => {
            let admin_client = KafkaAdminClient::new(api_key);

            // 3. Create/update user in Chronik
            let create_req = CreateUserRequest {
                username: cr_name.clone(),
                password,
                auth_type: spec.authentication.type_.clone(),
            };

            match admin_client.create_user(&admin_url, &create_req).await {
                Ok(resp) => {
                    if !resp.success {
                        warn!(user = %cr_name, "User creation returned: {}", resp.message);
                    }
                }
                Err(e) => {
                    warn!(user = %cr_name, "Failed to create user in Chronik: {e}");
                    update_status(
                        &client,
                        &cr_name,
                        &namespace,
                        "Pending",
                        Some(&secret_name),
                        None,
                        vec![make_condition(
                            "Ready",
                            "False",
                            "AdminApiUnreachable",
                            &format!("Cannot create user: {e}"),
                        )],
                        user.metadata.generation,
                    )
                    .await?;
                    return Ok(Action::requeue(Duration::from_secs(
                        constants::defaults::REQUEUE_NOT_READY_SECS,
                    )));
                }
            }

            // 4. Set ACLs
            let acl_count = if let Some(ref auth) = spec.authorization {
                let acl_entries = kafka_client::expand_acl_rules(&auth.acls);
                let acl_count = acl_entries.len() as i32;

                let set_req = SetAclsRequest {
                    username: cr_name.clone(),
                    acls: acl_entries,
                };

                match admin_client.set_acls(&admin_url, &set_req).await {
                    Ok(resp) => {
                        if !resp.success {
                            warn!(user = %cr_name, "ACL set returned: {}", resp.message);
                        }
                    }
                    Err(e) => {
                        warn!(user = %cr_name, "Failed to set ACLs: {e}");
                    }
                }

                acl_count
            } else {
                0
            };

            // 5. Update status — success
            update_status(
                &client,
                &cr_name,
                &namespace,
                "Ready",
                Some(&secret_name),
                Some(acl_count),
                vec![
                    make_condition("Ready", "True", "UserReady", "User and ACLs configured"),
                    make_condition(
                        "CredentialsReady",
                        "True",
                        "SecretExists",
                        &format!("Credentials stored in Secret '{secret_name}'"),
                    ),
                ],
                user.metadata.generation,
            )
            .await?;
        }
        Err(e) => {
            warn!(user = %cr_name, "Cannot resolve cluster: {e}");
            update_status(
                &client,
                &cr_name,
                &namespace,
                "Pending",
                Some(&secret_name),
                None,
                vec![make_condition(
                    "Ready",
                    "False",
                    "ClusterNotFound",
                    &format!("Cannot resolve clusterRef: {e}"),
                )],
                user.metadata.generation,
            )
            .await?;
            return Ok(Action::requeue(Duration::from_secs(
                constants::defaults::REQUEUE_NOT_READY_SECS,
            )));
        }
    }

    Ok(Action::requeue(Duration::from_secs(
        constants::defaults::REQUEUE_USER_SECS,
    )))
}

/// Ensure the credentials Secret exists. If not, generate a random password
/// and create the Secret.
///
/// Returns (secret_name, password).
async fn ensure_credentials_secret(
    secret_api: &Api<Secret>,
    cr_name: &str,
    spec: &crate::crds::user::ChronikUserSpec,
    user: &ChronikUser,
) -> Result<(String, String), OperatorError> {
    let secret_name = spec
        .authentication
        .password_secret
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| format!("{cr_name}-credentials"));

    let secret_key = spec
        .authentication
        .password_secret
        .as_ref()
        .map(|s| s.key.clone())
        .unwrap_or_else(|| "password".into());

    // Try to get the existing Secret
    match secret_api.get_opt(&secret_name).await? {
        Some(existing) => {
            // Read the password from the Secret
            let password = existing
                .data
                .as_ref()
                .and_then(|data| data.get(&secret_key))
                .map(|bytes| String::from_utf8_lossy(&bytes.0).to_string())
                .unwrap_or_default();

            Ok((secret_name, password))
        }
        None => {
            // Generate a random password and create the Secret
            let password = generate_password();

            let owner_ref = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                api_version: ChronikUser::api_version(&()).to_string(),
                kind: ChronikUser::kind(&()).to_string(),
                name: user.name_any(),
                uid: user.metadata.uid.clone().unwrap_or_default(),
                controller: Some(true),
                block_owner_deletion: Some(true),
            };

            let mut string_data = std::collections::BTreeMap::new();
            string_data.insert(secret_key, password.clone());
            string_data.insert("username".into(), cr_name.into());

            let secret = Secret {
                metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                    name: Some(secret_name.clone()),
                    namespace: user.namespace(),
                    owner_references: Some(vec![owner_ref]),
                    labels: Some(std::collections::BTreeMap::from([(
                        constants::labels::MANAGED_BY.into(),
                        constants::values::MANAGED_BY.into(),
                    )])),
                    ..Default::default()
                },
                string_data: Some(string_data),
                type_: Some("Opaque".into()),
                ..Default::default()
            };

            info!(user = %cr_name, secret = %secret_name, "Creating credentials Secret");
            secret_api.create(&PostParams::default(), &secret).await?;

            Ok((secret_name, password))
        }
    }
}

/// Generate a secure random password (32 chars, alphanumeric + symbols).
fn generate_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Cleanup on CR deletion.
async fn cleanup(client: Client, user: &ChronikUser) -> Result<Action, OperatorError> {
    let cr_name = user.name_any();
    let namespace = user.namespace().unwrap_or_else(|| "default".into());
    let spec = &user.spec;

    info!(user = %cr_name, "Cleaning up ChronikUser");

    // Try to delete the user from Chronik
    let resolve_result = crate::controllers::topic_controller::resolve_cluster_ref(
        &client,
        &namespace,
        &spec.cluster_ref,
    )
    .await;

    match resolve_result {
        Ok((admin_url, api_key)) => {
            let admin_client = KafkaAdminClient::new(api_key);
            match admin_client.delete_user(&admin_url, &cr_name).await {
                Ok(_) => info!(user = %cr_name, "User deleted from Chronik"),
                Err(e) => warn!(user = %cr_name, "Failed to delete user: {e}"),
            }
        }
        Err(e) => {
            warn!(user = %cr_name, "Cannot resolve cluster for cleanup: {e}");
            // Don't block finalizer removal if cluster is gone
        }
    }

    // Secret is owner-referenced and will be garbage collected.

    Ok(Action::await_change())
}

/// Update the ChronikUser status subresource.
#[allow(clippy::too_many_arguments)]
async fn update_status(
    client: &Client,
    name: &str,
    namespace: &str,
    phase: &str,
    credentials_secret: Option<&str>,
    acl_count: Option<i32>,
    conditions: Vec<Condition>,
    generation: Option<i64>,
) -> Result<(), OperatorError> {
    let status = ChronikUserStatus {
        phase: Some(phase.into()),
        observed_generation: generation,
        credentials_secret: credentials_secret.map(|s| s.into()),
        acl_count,
        conditions,
    };

    let status_patch = serde_json::json!({ "status": status });
    let ns_api: Api<ChronikUser> = Api::namespaced(client.clone(), namespace);
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
fn error_policy(_user: Arc<ChronikUser>, error: &OperatorError, _ctx: Arc<Context>) -> Action {
    warn!("User reconciliation error: {error}");
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
    fn test_generate_password_length() {
        let pwd = generate_password();
        assert_eq!(pwd.len(), 32);
    }

    #[test]
    fn test_generate_password_uniqueness() {
        let pwd1 = generate_password();
        let pwd2 = generate_password();
        assert_ne!(pwd1, pwd2);
    }

    #[test]
    fn test_generate_password_charset() {
        let pwd = generate_password();
        let valid_chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*";
        assert!(pwd.chars().all(|c| valid_chars.contains(c)));
    }
}
