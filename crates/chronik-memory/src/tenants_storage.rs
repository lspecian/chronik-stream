//! Per-tenant storage accounting + enforcement (AM-2.5).
//!
//! Tracks how many bytes each tenant has produced across the
//! `/memory/v1/*` write surface. Enforces
//! [`crate::TenantQuotas::storage_bytes`] when set — writes that would
//! push the tenant over its cap are rejected with a `Denied` decision.
//!
//! # Scope
//!
//! - **In-memory only.** State lives in an `Arc<DashMap<String, AtomicU64>>`;
//!   restart resets the counters. Recovery on startup (seeding from Kafka
//!   log-size query) is a follow-up — see [`StorageTracker::seed`] for the
//!   admin path.
//! - **Best-effort accounting.** Callers report the *serialized* size of
//!   the record they just produced; producers write both a Kafka key and
//!   value, so the accounting undercounts key bytes and Kafka framing
//!   overhead — good enough for quota enforcement, not a byte-exact log
//!   snapshot.
//! - **Enforcement is per-write.** The tracker doesn't know about the
//!   underlying WAL / segment layout; it just says "you're at N bytes,
//!   quota is Q, would this write push you over?"
//!
//! # Concurrency
//!
//! `try_reserve` and `add` are lock-free (`fetch_add` on an `AtomicU64`
//! per tenant). Race scenario: two concurrent writes near the quota can
//! both `try_reserve` before either `add`s — the tracker accepts both,
//! then observes an overshoot. That's an acceptable slack for the "soft
//! quota" model the roadmap describes; if strict enforcement is needed a
//! CAS loop can replace `fetch_add`.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Outcome of [`StorageTracker::try_reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageDecision {
    /// The write fits within the quota — caller may proceed and should
    /// call [`StorageTracker::add`] with the actual serialized size once
    /// the produce succeeds.
    Allowed,
    /// The write would push the tenant past its
    /// [`crate::TenantQuotas::storage_bytes`] cap.
    Denied {
        /// Current usage (before this write).
        used_bytes: u64,
        /// The tenant's cap.
        limit_bytes: u64,
        /// Requested write size.
        requested_bytes: u64,
    },
}

/// In-memory per-tenant byte counter. Cheap to clone (`Arc` interior).
#[derive(Debug, Default, Clone)]
pub struct StorageTracker {
    inner: Arc<DashMap<String, AtomicU64>>,
}

impl StorageTracker {
    /// Fresh empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes currently attributed to `tenant_id`. Returns 0 when the
    /// tenant has never produced.
    pub fn used(&self, tenant_id: &str) -> u64 {
        self.inner
            .get(tenant_id)
            .map(|v| v.value().load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Check whether a `size`-byte write would fit under `limit_bytes`.
    ///
    /// `None` limit is unlimited: always [`StorageDecision::Allowed`].
    /// Zero-byte writes are always allowed (idempotent tombstones, etc.).
    pub fn try_reserve(
        &self,
        tenant_id: &str,
        size: u64,
        limit_bytes: Option<u64>,
    ) -> StorageDecision {
        let Some(limit) = limit_bytes else {
            return StorageDecision::Allowed;
        };
        if size == 0 {
            return StorageDecision::Allowed;
        }
        let used = self.used(tenant_id);
        if used.saturating_add(size) > limit {
            StorageDecision::Denied {
                used_bytes: used,
                limit_bytes: limit,
                requested_bytes: size,
            }
        } else {
            StorageDecision::Allowed
        }
    }

    /// Add `size` bytes to `tenant_id`'s counter. Called after a
    /// successful produce so failed writes don't inflate the quota.
    pub fn add(&self, tenant_id: &str, size: u64) {
        if size == 0 {
            return;
        }
        self.inner
            .entry(tenant_id.to_string())
            .or_default()
            .fetch_add(size, Ordering::Relaxed);
    }

    /// Set the counter for a tenant. Used by admin recovery paths
    /// ([`Self::seed`]) — most callers should use [`Self::add`].
    pub fn set(&self, tenant_id: &str, bytes: u64) {
        self.inner
            .entry(tenant_id.to_string())
            .or_default()
            .store(bytes, Ordering::Relaxed);
    }

    /// Bulk-seed the counters from a `(tenant_id, bytes)` iterator.
    /// Overwrites existing values. Intended for cold-start seeding from
    /// a Kafka log-size query.
    pub fn seed<I: IntoIterator<Item = (String, u64)>>(&self, entries: I) {
        for (tid, bytes) in entries {
            self.set(&tid, bytes);
        }
    }

    /// Snapshot every `(tenant_id, used_bytes)` pair currently tracked.
    /// Iteration is per-shard so this is safe under concurrent writes;
    /// values may be out-of-date by the time the caller reads them.
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        self.inner
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
            .collect()
    }

    /// Drop a tenant's entry entirely. Used when the tenant is
    /// deprovisioned via `mem.tenants` tombstone.
    pub fn forget(&self, tenant_id: &str) -> bool {
        self.inner.remove(tenant_id).is_some()
    }

    /// Number of tenants with a non-zero counter (or an entry seeded to 0
    /// via [`Self::set`]).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if no tenant is being tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Point-in-time snapshot of a tenant's storage usage. Serialized as
/// part of admin responses (`/memory/v1/metrics` may embed this in a
/// future revision).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageUsage {
    /// Tenant identifier.
    pub tenant_id: String,
    /// Bytes attributed to this tenant since the tracker started.
    pub used_bytes: u64,
    /// The tenant's quota. `None` = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tracker_reports_zero() {
        let t = StorageTracker::new();
        assert!(t.is_empty());
        assert_eq!(t.used("acme"), 0);
    }

    #[test]
    fn add_bumps_and_snapshot_returns_pair() {
        let t = StorageTracker::new();
        t.add("acme", 100);
        t.add("acme", 200);
        t.add("beta", 50);
        assert_eq!(t.used("acme"), 300);
        assert_eq!(t.used("beta"), 50);
        assert_eq!(t.len(), 2);
        let mut snap = t.snapshot();
        snap.sort();
        assert_eq!(snap, vec![("acme".into(), 300), ("beta".into(), 50)]);
    }

    #[test]
    fn add_of_zero_is_noop_and_does_not_create_entry() {
        let t = StorageTracker::new();
        t.add("acme", 0);
        assert!(t.is_empty(), "0-byte adds must not create an entry");
        assert_eq!(t.used("acme"), 0);
    }

    #[test]
    fn try_reserve_no_limit_always_allowed() {
        let t = StorageTracker::new();
        t.add("acme", 10_000);
        assert_eq!(
            t.try_reserve("acme", 1_000_000, None),
            StorageDecision::Allowed
        );
    }

    #[test]
    fn try_reserve_zero_size_always_allowed_even_over_quota() {
        let t = StorageTracker::new();
        t.add("acme", 100);
        assert_eq!(
            t.try_reserve("acme", 0, Some(50)),
            StorageDecision::Allowed,
            "0-byte writes bypass quota (tombstones, etc.)"
        );
    }

    #[test]
    fn try_reserve_under_limit_allowed() {
        let t = StorageTracker::new();
        t.add("acme", 100);
        assert_eq!(
            t.try_reserve("acme", 50, Some(200)),
            StorageDecision::Allowed
        );
    }

    #[test]
    fn try_reserve_over_limit_denied_with_context() {
        let t = StorageTracker::new();
        t.add("acme", 180);
        let d = t.try_reserve("acme", 50, Some(200));
        assert_eq!(
            d,
            StorageDecision::Denied {
                used_bytes: 180,
                limit_bytes: 200,
                requested_bytes: 50,
            }
        );
    }

    #[test]
    fn try_reserve_exact_boundary_allowed() {
        let t = StorageTracker::new();
        t.add("acme", 100);
        // 100 + 100 == 200 == limit: fits exactly.
        assert_eq!(
            t.try_reserve("acme", 100, Some(200)),
            StorageDecision::Allowed
        );
    }

    #[test]
    fn try_reserve_boundary_plus_one_denied() {
        let t = StorageTracker::new();
        t.add("acme", 100);
        // 100 + 101 == 201 > 200: over.
        assert!(matches!(
            t.try_reserve("acme", 101, Some(200)),
            StorageDecision::Denied { .. }
        ));
    }

    #[test]
    fn set_overwrites_used() {
        let t = StorageTracker::new();
        t.add("acme", 500);
        t.set("acme", 100);
        assert_eq!(t.used("acme"), 100);
    }

    #[test]
    fn seed_bulk_writes() {
        let t = StorageTracker::new();
        t.seed(vec![
            ("acme".to_string(), 1_000),
            ("beta".to_string(), 2_000),
        ]);
        assert_eq!(t.used("acme"), 1_000);
        assert_eq!(t.used("beta"), 2_000);
    }

    #[test]
    fn forget_drops_entry() {
        let t = StorageTracker::new();
        t.add("acme", 100);
        assert!(t.forget("acme"));
        assert_eq!(t.used("acme"), 0);
        assert!(!t.forget("acme"), "second forget returns false");
    }

    #[test]
    fn saturating_add_prevents_overflow() {
        let t = StorageTracker::new();
        t.set("acme", u64::MAX - 10);
        // With a very tight quota below the current usage, the reserve
        // check must Deny — and, critically, not panic on `used + size`.
        assert!(matches!(
            t.try_reserve("acme", 100, Some(u64::MAX - 100)),
            StorageDecision::Denied { .. }
        ));
    }

    #[test]
    fn storage_usage_serializes_with_and_without_limit() {
        let with = StorageUsage {
            tenant_id: "acme".into(),
            used_bytes: 100,
            limit_bytes: Some(500),
        };
        let s = serde_json::to_string(&with).unwrap();
        assert!(s.contains("\"limit_bytes\":500"));
        let without = StorageUsage {
            tenant_id: "acme".into(),
            used_bytes: 100,
            limit_bytes: None,
        };
        let s = serde_json::to_string(&without).unwrap();
        assert!(!s.contains("limit_bytes"), "None limit is omitted");
    }
}
