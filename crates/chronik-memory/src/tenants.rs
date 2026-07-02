//! Tenant registry + auth validation (AM-2.5 foundation).
//!
//! Phase 1 (`AM-1.7`) shipped passthrough auth: `X-Tenant-Id` + `X-API-Key`
//! were extracted but not validated. This module provides the validation
//! primitives that handlers can call when `CHRONIK_MEMORY_REQUIRE_AUTH=true`:
//!
//! - [`Tenant`] — config record. Wire-compatible with the eventual
//!   `mem.tenants` topic schema (additive evolution only).
//! - [`TenantRegistry`] — in-memory store. Thread-safe via `parking_lot::RwLock`.
//!   Loading from `mem.tenants` (Kafka compacted topic) is the next chunk;
//!   for now, callers seed the registry programmatically or via
//!   [`TenantRegistry::from_env`].
//! - [`validate_request`] — `(headers, target_namespace) → Result<&Tenant>`.
//!   Returns 401 on missing/wrong API key, 403 on cross-namespace access.
//!
//! Quotas + rate limiting + per-tenant metrics are explicit follow-ups.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// One tenant's configuration. Wire-compatible with the eventual
/// `mem.tenants` topic record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tenant {
    /// Tenant identifier (matches the `tenant` segment of namespace paths).
    pub tenant_id: String,
    /// All API keys this tenant may present. A request with any of them is
    /// authenticated as this tenant.
    pub api_keys: Vec<String>,
    /// Glob-ish patterns the tenant is allowed to write to / read from.
    /// `*` matches any segment. Defaults to `["<tenant_id>:*"]` (i.e. the
    /// tenant owns its tenant prefix).
    #[serde(default)]
    pub namespace_patterns: Vec<String>,
    /// Free-text label for logs.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Soft per-tenant quotas. Enforcement lands as a follow-up (rate
    /// limiter + storage tracker).
    #[serde(default)]
    pub quotas: TenantQuotas,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TenantQuotas {
    /// Max ingest messages per second. `None` = unlimited.
    #[serde(default)]
    pub ingest_msgs_per_sec: Option<u32>,
    /// Max recall queries per second. `None` = unlimited.
    #[serde(default)]
    pub recall_qps: Option<u32>,
    /// Max storage in bytes. `None` = unlimited.
    #[serde(default)]
    pub storage_bytes: Option<u64>,
}

impl Tenant {
    /// Construct a tenant with a single API key and the default namespace
    /// pattern (`"<tenant_id>:*"`).
    pub fn new(tenant_id: impl Into<String>, api_key: impl Into<String>) -> Self {
        let id = tenant_id.into();
        Self {
            namespace_patterns: vec![format!("{id}:*")],
            tenant_id: id,
            api_keys: vec![api_key.into()],
            display_name: None,
            quotas: TenantQuotas::default(),
        }
    }

    /// Return `true` if `key` matches any of this tenant's API keys.
    pub fn accepts_key(&self, key: &str) -> bool {
        self.api_keys.iter().any(|k| k == key)
    }

    /// Return `true` if `namespace` is allowed by any of this tenant's patterns.
    pub fn owns_namespace(&self, namespace: &str) -> bool {
        self.namespace_patterns
            .iter()
            .any(|pat| pattern_matches(pat, namespace))
    }
}

/// Glob-ish pattern matcher. Currently supports:
///   - `*` matches any single segment (between colons)
///   - `**` matches everything to end of string
///
/// Examples:
///   - `acme:*`              matches `acme`, `acme:agent:bot:user:luis`, anything starting with `acme:`
///   - `acme:agent:*`        matches `acme:agent` and any extension
///   - `acme:**`             same as `acme:*`
///   - `acme:agent:bot:user:luis` exact match
fn pattern_matches(pat: &str, ns: &str) -> bool {
    // Strip optional trailing `:*` or `:**` and treat as a prefix match.
    let prefix = pat
        .strip_suffix(":**")
        .or_else(|| pat.strip_suffix(":*"))
        .map(|p| p.to_string());

    if let Some(prefix) = prefix {
        // Match `<prefix>` exactly OR `<prefix>:<anything>`.
        return ns == prefix || ns.starts_with(&format!("{prefix}:"));
    }
    // No suffix wildcard: must be exact (or empty pattern matches empty).
    pat == ns
}

/// Auth/authorization outcome.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing X-Tenant-Id or X-API-Key header")]
    MissingCredentials,
    #[error("unknown tenant {0:?}")]
    UnknownTenant(String),
    #[error("API key does not match tenant {0:?}")]
    InvalidKey(String),
    #[error("tenant {0:?} not authorized for namespace {1:?}")]
    ForbiddenNamespace(String, String),
}

/// In-memory tenant cache. Cheap to clone (`Arc`).
#[derive(Debug, Default, Clone)]
pub struct TenantRegistry {
    inner: Arc<RwLock<HashMap<String, Tenant>>>,
}

impl TenantRegistry {
    /// Build an empty registry. When empty, [`is_empty`](Self::is_empty)
    /// returns true and handlers can choose to operate in passthrough mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tenants registered.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// `true` if no tenants are registered.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Insert or replace a tenant.
    pub fn upsert(&self, tenant: Tenant) {
        let id = tenant.tenant_id.clone();
        self.inner.write().insert(id, tenant);
    }

    /// Remove a tenant. Returns whether it was present.
    pub fn remove(&self, tenant_id: &str) -> bool {
        self.inner.write().remove(tenant_id).is_some()
    }

    /// Look up a tenant by id (clones the record because `RwLock` doesn't
    /// expose long-lived borrows).
    pub fn get(&self, tenant_id: &str) -> Option<Tenant> {
        self.inner.read().get(tenant_id).cloned()
    }

    /// Build a registry from the `CHRONIK_MEMORY_TENANTS` env var. Format:
    ///
    /// ```text
    /// tenant_id:api_key[,tenant_id2:api_key2[,...]]
    /// ```
    ///
    /// Returns an empty registry when the env var is unset or empty. Bad
    /// entries (no colon, empty parts) are skipped with a log warning.
    /// This is a bootstrap convenience; the production source of truth is
    /// the `mem.tenants` Kafka topic (next-chunk work).
    pub fn from_env() -> Self {
        let registry = Self::new();
        let Ok(raw) = std::env::var("CHRONIK_MEMORY_TENANTS") else {
            return registry;
        };
        if raw.trim().is_empty() {
            return registry;
        }
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((id, key)) = entry.split_once(':') else {
                tracing::warn!(entry, "CHRONIK_MEMORY_TENANTS: skipping malformed entry (expected tenant_id:api_key)");
                continue;
            };
            let (id, key) = (id.trim(), key.trim());
            if id.is_empty() || key.is_empty() {
                tracing::warn!(entry, "CHRONIK_MEMORY_TENANTS: skipping empty tenant_id or key");
                continue;
            }
            registry.upsert(Tenant::new(id, key));
        }
        tracing::info!(
            n_tenants = registry.len(),
            "TenantRegistry seeded from CHRONIK_MEMORY_TENANTS"
        );
        registry
    }
}

/// Validate a request. Returns the authenticated tenant if all checks pass.
///
/// Checks (in order):
///   1. Both `tenant_id` and `api_key` are present (else `MissingCredentials`).
///   2. Tenant exists in the registry (else `UnknownTenant`).
///   3. `api_key` is one of the tenant's keys (else `InvalidKey`).
///   4. `target_namespace` matches one of the tenant's `namespace_patterns`
///      (else `ForbiddenNamespace`).
pub fn validate_request(
    registry: &TenantRegistry,
    tenant_id: Option<&str>,
    api_key: Option<&str>,
    target_namespace: &str,
) -> Result<Tenant, AuthError> {
    let tenant_id = tenant_id.ok_or(AuthError::MissingCredentials)?;
    let api_key = api_key.ok_or(AuthError::MissingCredentials)?;
    let tenant = registry
        .get(tenant_id)
        .ok_or_else(|| AuthError::UnknownTenant(tenant_id.to_string()))?;
    if !tenant.accepts_key(api_key) {
        return Err(AuthError::InvalidKey(tenant_id.to_string()));
    }
    if !tenant.owns_namespace(target_namespace) {
        return Err(AuthError::ForbiddenNamespace(
            tenant_id.to_string(),
            target_namespace.to_string(),
        ));
    }
    Ok(tenant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_matches_tenant_prefix_glob() {
        assert!(pattern_matches("acme:*", "acme"));
        assert!(pattern_matches("acme:*", "acme:agent"));
        assert!(pattern_matches("acme:*", "acme:agent:bot:user:luis"));
        assert!(pattern_matches("acme:**", "acme:agent:bot:user:luis"));
    }

    #[test]
    fn pattern_matches_rejects_other_tenants() {
        assert!(!pattern_matches("acme:*", "other"));
        assert!(!pattern_matches("acme:*", "other:agent"));
        // No partial-segment match — "acme2" doesn't match "acme:*".
        assert!(!pattern_matches("acme:*", "acme2"));
    }

    #[test]
    fn pattern_matches_exact() {
        assert!(pattern_matches("acme:agent:bot", "acme:agent:bot"));
        assert!(!pattern_matches("acme:agent:bot", "acme:agent:bot:more"));
    }

    #[test]
    fn registry_is_empty_by_default() {
        let r = TenantRegistry::new();
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
    }

    #[test]
    fn registry_upsert_and_get() {
        let r = TenantRegistry::new();
        r.upsert(Tenant::new("acme", "key-1"));
        assert_eq!(r.len(), 1);
        let t = r.get("acme").expect("present");
        assert!(t.accepts_key("key-1"));
        assert!(!t.accepts_key("wrong"));
        assert!(t.owns_namespace("acme:agent:bot"));
        assert!(!t.owns_namespace("other:agent:bot"));
    }

    #[test]
    fn validate_request_happy_path() {
        let r = TenantRegistry::new();
        r.upsert(Tenant::new("acme", "key-1"));
        let t = validate_request(&r, Some("acme"), Some("key-1"), "acme:agent:bot:user:luis")
            .expect("ok");
        assert_eq!(t.tenant_id, "acme");
    }

    #[test]
    fn validate_request_missing_credentials() {
        let r = TenantRegistry::new();
        r.upsert(Tenant::new("acme", "key-1"));
        assert!(matches!(
            validate_request(&r, None, Some("key-1"), "acme:n"),
            Err(AuthError::MissingCredentials)
        ));
        assert!(matches!(
            validate_request(&r, Some("acme"), None, "acme:n"),
            Err(AuthError::MissingCredentials)
        ));
    }

    #[test]
    fn validate_request_unknown_tenant() {
        let r = TenantRegistry::new();
        let err = validate_request(&r, Some("ghost"), Some("k"), "ghost:n").unwrap_err();
        assert!(matches!(err, AuthError::UnknownTenant(_)));
    }

    #[test]
    fn validate_request_bad_key() {
        let r = TenantRegistry::new();
        r.upsert(Tenant::new("acme", "key-1"));
        let err = validate_request(&r, Some("acme"), Some("wrong"), "acme:n").unwrap_err();
        assert!(matches!(err, AuthError::InvalidKey(_)));
    }

    #[test]
    fn validate_request_cross_tenant_forbidden() {
        let r = TenantRegistry::new();
        r.upsert(Tenant::new("acme", "key-1"));
        let err = validate_request(&r, Some("acme"), Some("key-1"), "other:agent:bot").unwrap_err();
        match err {
            AuthError::ForbiddenNamespace(t, ns) => {
                assert_eq!(t, "acme");
                assert_eq!(ns, "other:agent:bot");
            }
            other => panic!("expected ForbiddenNamespace, got {other:?}"),
        }
    }

    // The two `from_env_*` tests both mutate CHRONIK_MEMORY_TENANTS, which
    // is a process-global. Rust runs tests in a thread pool by default,
    // so they'd race. Serialize via a mutex; combine into one test if the
    // sequence is short.
    static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn from_env_handles_empty_malformed_and_multi() {
        let _g = ENV_LOCK.lock();

        // Unset.
        std::env::remove_var("CHRONIK_MEMORY_TENANTS");
        assert!(TenantRegistry::from_env().is_empty());

        // Whitespace-only.
        std::env::set_var("CHRONIK_MEMORY_TENANTS", "   ");
        assert!(TenantRegistry::from_env().is_empty());

        // Mixed valid / malformed.
        std::env::set_var("CHRONIK_MEMORY_TENANTS", "nokey, :no-id, acme:, :, acme:key-1");
        let r = TenantRegistry::from_env();
        assert_eq!(r.len(), 1, "only 'acme:key-1' should land");
        assert!(r.get("acme").is_some());

        // Multiple tenants.
        std::env::set_var(
            "CHRONIK_MEMORY_TENANTS",
            "acme:key-a, beta:key-b, gamma:key-g",
        );
        let r = TenantRegistry::from_env();
        assert_eq!(r.len(), 3);
        assert!(r.get("acme").unwrap().accepts_key("key-a"));
        assert!(r.get("beta").unwrap().accepts_key("key-b"));
        assert!(r.get("gamma").unwrap().accepts_key("key-g"));

        std::env::remove_var("CHRONIK_MEMORY_TENANTS");
    }

    #[test]
    fn registry_remove() {
        let r = TenantRegistry::new();
        r.upsert(Tenant::new("acme", "k"));
        assert!(r.remove("acme"));
        assert!(!r.remove("acme"), "second remove returns false");
    }
}
