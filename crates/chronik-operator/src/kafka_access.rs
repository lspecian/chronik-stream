//! How the operator reaches a cluster's Kafka port for topic and user
//! management: which addresses, and which login.
//!
//! With SASL on, the operator logs in as the broker-to-broker user
//! (`__chronik_broker`) with the secret the broker Pods receive. It is the one
//! principal the operator can always know, and it bypasses ACLs, which managing
//! other users' ACLs requires.

use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};

use crate::constants::ports;
use crate::crds::cluster::{sasl_setting_of, ChronikCluster, SaslSetting, INTER_BROKER_SECRET_ENV};
use crate::crds::common::{EnvVar, KafkaTlsSpec, SecretKeyRef};
use crate::crds::standalone::ChronikStandalone;
use crate::crds::topic::ClusterRef;
use crate::error::OperatorError;
use crate::kafka_admin::{Credentials, KafkaAccess, KafkaTls};

/// The broker-to-broker user, as the broker names it.
const BROKER_USER: &str = chronik_scram::INTER_BROKER_USER;
/// `CHRONIK_TLS_CERT` puts the Kafka port behind TLS (set by `spec.tls`, or by hand).
const KAFKA_TLS_ENV: &str = "CHRONIK_TLS_CERT";

/// Resolve a `clusterRef` (ChronikCluster or ChronikStandalone) to its Kafka
/// addresses and the login the operator should use.
pub async fn resolve_kafka_access(
    client: &Client,
    namespace: &str,
    cluster_ref: &ClusterRef,
) -> Result<KafkaAccess, OperatorError> {
    let kind = cluster_ref.kind.as_deref().unwrap_or("ChronikCluster");
    let name = &cluster_ref.name;
    match kind {
        "ChronikCluster" => {
            let api: Api<ChronikCluster> = Api::namespaced(client.clone(), namespace);
            let cluster = api.get(name).await.map_err(|e| {
                OperatorError::NotFound(format!("ChronikCluster '{name}' not found: {e}"))
            })?;
            let tls = kafka_tls(client, namespace, cluster.spec.tls.as_ref(), &cluster.spec.env).await?;
            let bootstrap = (1..=cluster.spec.replicas.max(1) as u64)
                .map(|id| {
                    format!(
                        "{}:{}",
                        crate::config_generator::node_dns_name(name, id, namespace),
                        ports::KAFKA
                    )
                })
                .collect();
            let secret = match sasl_setting_of(&cluster.spec.env) {
                SaslSetting::Disabled => None,
                setting => Some(match explicit_secret_env(&cluster.spec.env) {
                    Some(var) => env_value(client, namespace, var).await?,
                    None => match cluster.spec.inter_broker_secret_source() {
                        Some(source) => read_secret(client, namespace, &source.secret_key_ref(name)).await?,
                        None => return Err(no_secret(kind, name, setting)),
                    },
                }),
            };
            Ok(KafkaAccess { bootstrap, credentials: secret.map(broker_login), tls })
        }
        "ChronikStandalone" => {
            let api: Api<ChronikStandalone> = Api::namespaced(client.clone(), namespace);
            let standalone = api.get(name).await.map_err(|e| {
                OperatorError::NotFound(format!("ChronikStandalone '{name}' not found: {e}"))
            })?;
            let tls = kafka_tls(client, namespace, standalone.spec.tls.as_ref(), &standalone.spec.env).await?;
            let bootstrap = vec![format!("{name}.{namespace}.svc.cluster.local:{}", ports::KAFKA)];
            // A standalone has no operator-managed broker secret: the login
            // exists only if the user set CHRONIK_INTER_BROKER_SECRET.
            let secret = match sasl_setting_of(&standalone.spec.env) {
                SaslSetting::Disabled => None,
                setting => match explicit_secret_env(&standalone.spec.env) {
                    Some(var) => Some(env_value(client, namespace, var).await?),
                    None => return Err(no_secret(kind, name, setting)),
                },
            };
            Ok(KafkaAccess { bootstrap, credentials: secret.map(broker_login), tls })
        }
        other => Err(OperatorError::Config(format!(
            "Unsupported clusterRef kind: '{other}'. Expected 'ChronikCluster' or 'ChronikStandalone'"
        ))),
    }
}

fn broker_login(secret: String) -> Credentials {
    Credentials {
        username: BROKER_USER.into(),
        password: secret.trim().to_string(),
    }
}

/// How the operator speaks TLS to the Kafka port: with `spec.tls`, the CA and
/// certificate from its Secret. TLS configured only through env cannot be
/// verified (the operator does not know the CA), so it is refused.
async fn kafka_tls(
    client: &Client,
    namespace: &str,
    tls: Option<&KafkaTlsSpec>,
    env: &[EnvVar],
) -> Result<Option<KafkaTls>, OperatorError> {
    let Some(tls) = tls else {
        refuse_tls(env)?;
        return Ok(None);
    };
    let key = |key: &str| SecretKeyRef {
        name: tls.secret_name.clone(),
        key: key.into(),
    };
    Ok(Some(KafkaTls {
        ca_pem: read_secret(client, namespace, &key("ca.crt"))
            .await?
            .into_bytes(),
        cert_pem: read_secret(client, namespace, &key("tls.crt"))
            .await?
            .into_bytes(),
        key_pem: read_secret(client, namespace, &key("tls.key"))
            .await?
            .into_bytes(),
    }))
}

fn refuse_tls(env: &[EnvVar]) -> Result<(), OperatorError> {
    if env.iter().any(|e| e.name == KAFKA_TLS_ENV) {
        return Err(OperatorError::Config(format!(
            "the Kafka port uses TLS through env ({KAFKA_TLS_ENV}), so the operator cannot \
             verify the brokers; configure spec.tls (a Secret with tls.crt, tls.key and ca.crt) \
             instead and the operator manages topics and users over TLS"
        )));
    }
    Ok(())
}

fn explicit_secret_env(env: &[EnvVar]) -> Option<&EnvVar> {
    env.iter().rev().find(|e| e.name == INTER_BROKER_SECRET_ENV)
}

fn no_secret(kind: &str, name: &str, setting: SaslSetting) -> OperatorError {
    let why = match setting {
        SaslSetting::Unknown => {
            "CHRONIK_SASL_ENABLED comes from a Secret, so no broker secret is generated"
        }
        _ => "no broker secret is configured",
    };
    OperatorError::Config(format!(
        "{kind} '{name}' requires SASL and {why}; set {INTER_BROKER_SECRET_ENV} (or, on a \
         ChronikCluster, spec.interBrokerSecret) so the operator can log in"
    ))
}

async fn env_value(
    client: &Client,
    namespace: &str,
    var: &EnvVar,
) -> Result<String, OperatorError> {
    match (&var.value, &var.value_from_secret) {
        (Some(v), _) => Ok(v.clone()),
        (None, Some(r)) => read_secret(client, namespace, r).await,
        (None, None) => Err(OperatorError::Config(format!(
            "{} is set without a value",
            var.name
        ))),
    }
}

async fn read_secret(
    client: &Client,
    namespace: &str,
    r: &SecretKeyRef,
) -> Result<String, OperatorError> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let secret = api.get_opt(&r.name).await?.ok_or_else(|| {
        OperatorError::NotFound(format!("Secret '{}' (broker login) not found", r.name))
    })?;
    let bytes = secret
        .data
        .as_ref()
        .and_then(|d| d.get(&r.key))
        .ok_or_else(|| {
            OperatorError::NotFound(format!("Secret '{}' has no key '{}'", r.name, r.key))
        })?;
    String::from_utf8(bytes.0.clone()).map_err(|_| {
        OperatorError::Config(format!("Secret '{}' key '{}' is not UTF-8", r.name, r.key))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str, value: &str) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: Some(value.into()),
            value_from_secret: None,
        }
    }

    #[test]
    fn tls_clusters_are_refused_with_a_reason() {
        assert!(refuse_tls(&[env("CHRONIK_SASL_ENABLED", "true")]).is_ok());
        let e = refuse_tls(&[env(KAFKA_TLS_ENV, "/certs/tls.crt")]).unwrap_err();
        assert!(e.to_string().contains("TLS"));
    }

    #[test]
    fn the_last_secret_entry_wins() {
        let vars = [
            env(INTER_BROKER_SECRET_ENV, "first"),
            env(INTER_BROKER_SECRET_ENV, "second"),
        ];
        assert_eq!(
            explicit_secret_env(&vars).unwrap().value.as_deref(),
            Some("second")
        );
    }

    #[test]
    fn the_login_is_the_broker_user_with_a_trimmed_secret() {
        let c = broker_login(" s3cret-s3cret-s3cret \n".into());
        assert_eq!(c.username, "__chronik_broker");
        assert_eq!(c.password, "s3cret-s3cret-s3cret");
    }
}
