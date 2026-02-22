use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, PostParams};
use kube::Client;
use tracing::{debug, error, info, warn};

/// Annotation key for storing the renew epoch timestamp.
const ANNOTATION_RENEW_EPOCH: &str = "chronik.io/renew-epoch";

/// Configuration for Lease-based leader election.
pub struct LeaderElectionConfig {
    /// Name of the Lease object in Kubernetes.
    pub lease_name: String,
    /// Namespace where the Lease lives.
    pub namespace: String,
    /// Unique identity of this operator instance (typically the pod name).
    pub holder_id: String,
    /// How long the lease is valid before it expires (seconds).
    pub lease_duration_secs: i32,
    /// How often to attempt renewal (seconds). Should be ~1/3 of duration.
    pub renew_interval_secs: u64,
}

impl Default for LeaderElectionConfig {
    fn default() -> Self {
        let holder_id = std::env::var("POD_NAME")
            .unwrap_or_else(|_| format!("chronik-operator-{}", uuid::Uuid::new_v4()));
        let namespace = std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "default".into());

        Self {
            lease_name: "chronik-operator-leader".into(),
            namespace,
            holder_id,
            lease_duration_secs: 15,
            renew_interval_secs: 5,
        }
    }
}

/// Shared leader status accessible from multiple tasks.
#[derive(Clone)]
pub struct LeaderStatus {
    is_leader: Arc<AtomicBool>,
}

impl Default for LeaderStatus {
    fn default() -> Self {
        Self {
            is_leader: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl LeaderStatus {
    pub fn new() -> Self {
        Self {
            is_leader: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns true if this instance currently holds leadership.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Relaxed)
    }

    /// Force this instance to be leader (used when leader election is disabled).
    pub fn force_leader(&self) {
        self.is_leader.store(true, Ordering::Relaxed);
    }

    fn set_leader(&self, v: bool) {
        self.is_leader.store(v, Ordering::Relaxed);
    }
}

/// Run the leader election loop. This function never returns normally.
pub async fn run(client: Client, config: LeaderElectionConfig, status: LeaderStatus) {
    let api: Api<Lease> = Api::namespaced(client, &config.namespace);

    loop {
        match try_acquire_or_renew(&api, &config).await {
            Ok(acquired) => {
                if acquired && !status.is_leader() {
                    info!(
                        holder = %config.holder_id,
                        lease = %config.lease_name,
                        "Acquired leadership"
                    );
                } else if !acquired && status.is_leader() {
                    warn!(
                        holder = %config.holder_id,
                        "Lost leadership"
                    );
                }
                status.set_leader(acquired);
            }
            Err(e) => {
                error!("Leader election error: {e}");
                status.set_leader(false);
            }
        }

        tokio::time::sleep(Duration::from_secs(config.renew_interval_secs)).await;
    }
}

/// Gracefully step down from leadership on shutdown.
pub async fn step_down(client: &Client, config: &LeaderElectionConfig) {
    let api: Api<Lease> = Api::namespaced(client.clone(), &config.namespace);

    match api.get(&config.lease_name).await {
        Ok(existing) => {
            let holder = existing
                .spec
                .as_ref()
                .and_then(|s| s.holder_identity.as_deref());

            if holder == Some(config.holder_id.as_str()) {
                let mut updated = existing.clone();
                if let Some(ref mut s) = updated.spec {
                    s.holder_identity = None;
                    s.lease_duration_seconds = Some(1);
                }
                match api
                    .replace(&config.lease_name, &PostParams::default(), &updated)
                    .await
                {
                    Ok(_) => info!("Stepped down from leadership"),
                    Err(e) => warn!("Failed to step down: {e}"),
                }
            }
        }
        Err(e) => {
            debug!("Could not read lease for step-down: {e}");
        }
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn try_acquire_or_renew(
    api: &Api<Lease>,
    config: &LeaderElectionConfig,
) -> anyhow::Result<bool> {
    let now = epoch_secs();

    match api.get(&config.lease_name).await {
        Ok(existing) => {
            let spec = existing.spec.as_ref();
            let holder = spec.and_then(|s| s.holder_identity.as_deref());
            let annotations = existing.metadata.annotations.as_ref();
            let renew_epoch: Option<u64> = annotations
                .and_then(|a| a.get(ANNOTATION_RENEW_EPOCH))
                .and_then(|s| s.parse().ok());
            let duration = spec
                .and_then(|s| s.lease_duration_seconds)
                .unwrap_or(config.lease_duration_secs) as u64;

            if holder == Some(config.holder_id.as_str()) {
                // We hold the lease — renew it.
                let mut updated = existing.clone();
                let ann = updated
                    .metadata
                    .annotations
                    .get_or_insert_with(BTreeMap::new);
                ann.insert(ANNOTATION_RENEW_EPOCH.to_string(), now.to_string());

                match api
                    .replace(&config.lease_name, &PostParams::default(), &updated)
                    .await
                {
                    Ok(_) => Ok(true),
                    Err(kube::Error::Api(e)) if e.code == 409 => {
                        debug!("Conflict renewing lease, will retry");
                        Ok(false)
                    }
                    Err(e) => Err(e.into()),
                }
            } else {
                // Someone else holds it — check if expired.
                let expired = match renew_epoch {
                    Some(ts) => now > ts + duration,
                    None => true,
                };

                if expired {
                    let transitions = spec.and_then(|s| s.lease_transitions).unwrap_or(0);

                    let mut updated = existing.clone();
                    if let Some(ref mut s) = updated.spec {
                        s.holder_identity = Some(config.holder_id.clone());
                        s.lease_transitions = Some(transitions + 1);
                    }
                    let ann = updated
                        .metadata
                        .annotations
                        .get_or_insert_with(BTreeMap::new);
                    ann.insert(ANNOTATION_RENEW_EPOCH.to_string(), now.to_string());

                    match api
                        .replace(&config.lease_name, &PostParams::default(), &updated)
                        .await
                    {
                        Ok(_) => Ok(true),
                        Err(kube::Error::Api(e)) if e.code == 409 => {
                            debug!("Conflict acquiring expired lease");
                            Ok(false)
                        }
                        Err(e) => Err(e.into()),
                    }
                } else {
                    debug!(holder = ?holder, "Lease held by another instance");
                    Ok(false)
                }
            }
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Lease does not exist — create it.
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(config.lease_name.clone()),
                    namespace: Some(config.namespace.clone()),
                    annotations: Some(BTreeMap::from([(
                        ANNOTATION_RENEW_EPOCH.to_string(),
                        now.to_string(),
                    )])),
                    ..Default::default()
                },
                spec: Some(LeaseSpec {
                    holder_identity: Some(config.holder_id.clone()),
                    lease_duration_seconds: Some(config.lease_duration_secs),
                    lease_transitions: Some(0),
                    acquire_time: None,
                    renew_time: None,
                }),
            };

            match api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(e)) if e.code == 409 => {
                    debug!("Lease already created by another instance");
                    Ok(false)
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leader_status_default_not_leader() {
        let status = LeaderStatus::new();
        assert!(!status.is_leader());
    }

    #[test]
    fn test_leader_status_force_leader() {
        let status = LeaderStatus::new();
        assert!(!status.is_leader());
        status.force_leader();
        assert!(status.is_leader());
    }

    #[test]
    fn test_leader_status_clone_shares_state() {
        let status = LeaderStatus::new();
        let clone = status.clone();
        assert!(!clone.is_leader());
        status.force_leader();
        assert!(clone.is_leader());
    }

    #[test]
    fn test_default_config() {
        let config = LeaderElectionConfig::default();
        assert_eq!(config.lease_name, "chronik-operator-leader");
        assert_eq!(config.lease_duration_secs, 15);
        assert_eq!(config.renew_interval_secs, 5);
        assert!(!config.holder_id.is_empty());
    }

    #[test]
    fn test_epoch_secs_nonzero() {
        assert!(epoch_secs() > 0);
    }
}
