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
use crate::kafka_access::resolve_kafka_access;
use crate::kafka_admin::{self, KafkaAdmin};

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

    // A failure below leaves the user Pending with the reason, and retries.
    let pending = |reason: &'static str, message: String| {
        let client = client.clone();
        let (cr_name, namespace, secret_name) = (cr_name.clone(), namespace.clone(), secret_name.clone());
        let generation = user.metadata.generation;
        async move {
            warn!(user = %cr_name, "{reason}: {message}");
            update_status(
                &client,
                &cr_name,
                &namespace,
                "Pending",
                Some(&secret_name),
                None,
                vec![make_condition("Ready", "False", reason, &message)],
                generation,
            )
            .await?;
            Ok::<_, OperatorError>(Action::requeue(Duration::from_secs(
                constants::defaults::REQUEUE_NOT_READY_SECS,
            )))
        }
    };

    // 2. Which credential the user gets (a TLS principal has none).
    let mechanism = match kafka_admin::scram_mechanism(&spec.authentication.type_) {
        Ok(m) => m,
        Err(e) => {
            // Not retryable until the spec changes.
            update_status(
                &client,
                &cr_name,
                &namespace,
                "Error",
                Some(&secret_name),
                None,
                vec![make_condition("Ready", "False", "UnsupportedAuthentication", &e.to_string())],
                user.metadata.generation,
            )
            .await?;
            return Ok(Action::await_change());
        }
    };
    if mechanism.is_some() && password.is_empty() {
        return pending("NoPassword", format!("Secret '{secret_name}' holds no password")).await;
    }

    // 3. Connect to the cluster's Kafka port.
    let access = match resolve_kafka_access(&client, &namespace, &spec.cluster_ref).await {
        Ok(a) => a,
        Err(e) => return pending("ClusterNotReachable", e.to_string()).await,
    };
    let mut admin = match KafkaAdmin::connect(&access).await {
        Ok(a) => a,
        Err(e) => return pending("KafkaUnreachable", e.to_string()).await,
    };

    // 4. The SCRAM credential, written only when it is new or the password
    //    changed (each write replaces the stored salt and keys).
    if let Some(mechanism) = mechanism {
        let key = credential_key(&namespace, &spec.cluster_ref.name, &cr_name, mechanism);
        let fingerprint = password_fingerprint(&password);
        if credential_cache().lock().unwrap().get(&key) != Some(&fingerprint) {
            if let Err(e) = admin.upsert_scram_user(&cr_name, &password, mechanism).await {
                return pending("CredentialFailed", e.to_string()).await;
            }
            info!(user = %cr_name, mechanism = mechanism.as_str(), "SCRAM credential set");
            credential_cache().lock().unwrap().insert(key, fingerprint);
        }
    }

    // 5. ACLs: exactly the listed ones (none listed removes any left over).
    let desired = spec
        .authorization
        .as_ref()
        .map(|a| kafka_admin::expand_acl_rules(&a.acls))
        .unwrap_or_default();
    let acl_count = match admin.set_user_acls(&cr_name, &desired).await {
        Ok(n) => n as i32,
        Err(e) => return pending("AclsFailed", e.to_string()).await,
    };

    // 6. Ready.
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

    Ok(Action::requeue(Duration::from_secs(
        constants::defaults::REQUEUE_USER_SECS,
    )))
}

type CredentialKey = (String, String, String, &'static str);

/// Which credential this process last wrote, per (namespace, cluster, user,
/// mechanism): the password's fingerprint. After a restart each credential is
/// written once more, then only when its password changes.
fn credential_cache() -> &'static std::sync::Mutex<std::collections::HashMap<CredentialKey, [u8; 32]>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<CredentialKey, [u8; 32]>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn credential_key(
    namespace: &str,
    cluster: &str,
    user: &str,
    mechanism: chronik_protocol::sasl::SaslMechanism,
) -> CredentialKey {
    (namespace.into(), cluster.into(), user.into(), mechanism.as_str())
}

fn password_fingerprint(password: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(password.as_bytes()).into()
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

    // Remove the user's ACLs and credentials. A cluster that is gone or
    // unreachable must not block finalizer removal.
    let removed = match resolve_kafka_access(&client, &namespace, &spec.cluster_ref).await {
        Ok(access) => match KafkaAdmin::connect(&access).await {
            Ok(mut admin) => match admin.set_user_acls(&cr_name, &[]).await {
                Ok(_) => admin.delete_scram_user(&cr_name).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    match removed {
        Ok(()) => info!(user = %cr_name, "User and its ACLs removed"),
        Err(e) => warn!(user = %cr_name, "Could not remove the user: {e}"),
    }
    credential_cache()
        .lock()
        .unwrap()
        .retain(|(ns, cluster, user, _), _| !(ns == &namespace && cluster == &spec.cluster_ref.name && user == &cr_name));

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
