//! Per-tenant token-bucket rate limiter for `/memory/v1/*` (AM-2.5).
//!
//! Two-dimensional keying `(tenant_id, endpoint_kind)`. Each bucket enforces
//! the corresponding [`TenantQuotas`] field:
//!
//! | [`EndpointKind`]  | Enforced against          |
//! |-------------------|---------------------------|
//! | `Ingest`          | `ingest_msgs_per_sec`     |
//! | `Recall`          | `recall_qps`              |
//! | `AdminOrHealth`   | never rate-limited        |
//!
//! Buckets are lazily created on first-hit and cached in a `DashMap` so
//! subsequent hits are lock-free reads (bucket refill uses a per-key
//! `Mutex`).
//!
//! # Semantics
//!
//! - Rate = the tenant's quota field expressed as tokens per second.
//! - Burst capacity = `rate` (one second of headroom); enough to absorb
//!   bursty callers without letting them permanently exceed steady-state.
//! - `None` quota (or `Some(0)`) = unlimited: always allowed.
//! - When the bucket has < 1 token, [`try_acquire`] returns
//!   [`Denied { retry_after }`].
//!
//! # Testability
//!
//! The core is `TokenBucket::try_acquire_at(now)` — pure logic
//! parameterized on a caller-provided `Instant`. The public
//! [`RateLimiter::try_acquire`] wraps it with `Instant::now()`.
//! Unit tests use synthetic `Instant`s to assert exact refill / denial
//! behavior without real sleeps.

use crate::tenants::{Tenant, TenantQuotas};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The kind of `/memory/v1/*` endpoint being called. Different kinds enforce
/// different quota fields (recall_qps vs ingest_msgs_per_sec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointKind {
    /// `POST /memory/v1/ingest` — one call = N messages; the limiter treats
    /// it as N tokens (the caller passes the batch size).
    Ingest,
    /// `POST /memory/v1/recall` — one call = 1 QPS token.
    Recall,
    /// `POST /memory/v1/remember` and `/forget` — count against Ingest
    /// (1 message each). Callers pass `Ingest` and `size=1`.
    ///
    /// (Intentionally the same variant as `Ingest` so we don't proliferate
    /// enum variants for the same underlying quota — see `try_acquire_ingest`.)
    #[doc(hidden)]
    Write,
    /// `GET /memory/v1/health`, `POST /memory/v1/admin/*`, feedback,
    /// source-endpoint — never rate-limited.
    AdminOrHealth,
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, PartialEq)]
pub enum RateDecision {
    /// The caller may proceed.
    Allowed,
    /// The caller must wait at least `retry_after` before retrying.
    Denied {
        /// Suggested retry delay.
        retry_after: Duration,
    },
}

impl RateDecision {
    /// `true` if the caller may proceed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, RateDecision::Allowed)
    }
}

/// Composite key for the bucket cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BucketKey {
    tenant_id: String,
    kind: EndpointKind,
}

/// A single token bucket. Guarded by a `Mutex` inside [`RateLimiter`].
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Steady-state tokens/second.
    pub rate: f64,
    /// Maximum tokens held (burst headroom).
    pub burst: f64,
    /// Current token count.
    pub tokens: f64,
    /// Last refill timestamp.
    pub last_refill: Instant,
}

impl TokenBucket {
    /// Construct with `burst = rate` (1 second of headroom).
    pub fn new(rate: f64, now: Instant) -> Self {
        Self {
            rate,
            burst: rate,
            tokens: rate,
            last_refill: now,
        }
    }

    /// Pure token-bucket step: refill for the elapsed period, try to consume
    /// `size` tokens. Returns the decision + the mutated bucket.
    pub fn try_acquire_at(&mut self, size: f64, now: Instant) -> RateDecision {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.last_refill = now;

        if self.tokens >= size {
            self.tokens -= size;
            RateDecision::Allowed
        } else {
            // Compute how long until the bucket will hold `size` tokens.
            let deficit = size - self.tokens;
            let secs = if self.rate > 0.0 {
                deficit / self.rate
            } else {
                60.0
            };
            RateDecision::Denied {
                retry_after: Duration::from_secs_f64(secs.max(0.0)),
            }
        }
    }
}

/// Per-tenant rate limiter. Cheap to clone (`Arc` interior).
#[derive(Debug, Default, Clone)]
pub struct RateLimiter {
    inner: Arc<DashMap<BucketKey, Mutex<TokenBucket>>>,
}

impl RateLimiter {
    /// Fresh empty limiter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-only bucket count.
    #[doc(hidden)]
    pub fn bucket_count(&self) -> usize {
        self.inner.len()
    }

    /// Try to acquire `size` tokens for `(tenant, kind)`. `size` should be
    /// 1 for most calls; ingest passes the batch size.
    ///
    /// Returns [`RateDecision::Allowed`] when the tenant has no quota set
    /// for the relevant field (unlimited semantics).
    pub fn try_acquire(&self, tenant: &Tenant, kind: EndpointKind, size: usize) -> RateDecision {
        self.try_acquire_at(tenant, kind, size, Instant::now())
    }

    /// [`try_acquire`] with a caller-provided `Instant` (for tests).
    pub fn try_acquire_at(
        &self,
        tenant: &Tenant,
        kind: EndpointKind,
        size: usize,
        now: Instant,
    ) -> RateDecision {
        // AdminOrHealth is never rate-limited.
        if kind == EndpointKind::AdminOrHealth {
            return RateDecision::Allowed;
        }
        let rate = rate_for(&tenant.quotas, kind);
        // None or 0 = unlimited.
        let Some(rate) = rate else {
            return RateDecision::Allowed;
        };
        let key = BucketKey {
            tenant_id: tenant.tenant_id.clone(),
            kind,
        };
        let entry = self
            .inner
            .entry(key)
            .or_insert_with(|| Mutex::new(TokenBucket::new(rate, now)));
        let mut bucket = entry.value().lock();
        // If quota changed at runtime (via TenantRegistry upsert), reset the
        // bucket rate but keep the current token count trimmed to the new burst.
        if (bucket.rate - rate).abs() > f64::EPSILON {
            bucket.rate = rate;
            bucket.burst = rate;
            bucket.tokens = bucket.tokens.min(rate);
        }
        bucket.try_acquire_at(size as f64, now)
    }

    /// Forget every bucket owned by `tenant_id`. Called when the tenant is
    /// tombstoned via the `mem.tenants` consumer.
    pub fn forget_tenant(&self, tenant_id: &str) {
        self.inner
            .retain(|k, _| k.tenant_id != tenant_id);
    }
}

/// Pick the right quota field for a given `EndpointKind`. Returns `None`
/// when unlimited.
fn rate_for(quotas: &TenantQuotas, kind: EndpointKind) -> Option<f64> {
    let n = match kind {
        EndpointKind::Ingest | EndpointKind::Write => quotas.ingest_msgs_per_sec,
        EndpointKind::Recall => quotas.recall_qps,
        EndpointKind::AdminOrHealth => return None,
    };
    match n {
        Some(0) => None, // 0 = unlimited by convention (matches `None`)
        Some(n) => Some(n as f64),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenants::{Tenant, TenantQuotas};

    fn tenant_with_quotas(rate_ingest: Option<u32>, rate_recall: Option<u32>) -> Tenant {
        let mut t = Tenant::new("acme", "key");
        t.quotas = TenantQuotas {
            ingest_msgs_per_sec: rate_ingest,
            recall_qps: rate_recall,
            storage_bytes: None,
        };
        t
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn no_quota_is_unlimited() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(None, None);
        assert!(lim
            .try_acquire_at(&tenant, EndpointKind::Recall, 1, t0())
            .is_allowed());
        for _ in 0..100 {
            assert!(lim
                .try_acquire_at(&tenant, EndpointKind::Recall, 1, t0())
                .is_allowed());
        }
    }

    #[test]
    fn admin_endpoint_never_limited() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(Some(1), Some(1));
        for _ in 0..10 {
            assert!(lim
                .try_acquire_at(&tenant, EndpointKind::AdminOrHealth, 1, t0())
                .is_allowed());
        }
        assert_eq!(lim.bucket_count(), 0, "no bucket created for admin");
    }

    #[test]
    fn quota_of_zero_is_unlimited() {
        // Explicit 0 = unlimited (documented convention).
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(Some(0), Some(0));
        for _ in 0..100 {
            assert!(lim
                .try_acquire_at(&tenant, EndpointKind::Recall, 1, t0())
                .is_allowed());
        }
    }

    #[test]
    fn recall_burst_of_size_rate_then_deny() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(None, Some(5));
        let now = t0();
        // Burst of 5 is allowed at the same instant.
        for _ in 0..5 {
            assert!(lim
                .try_acquire_at(&tenant, EndpointKind::Recall, 1, now)
                .is_allowed());
        }
        // 6th should be denied.
        let d = lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, now);
        assert!(!d.is_allowed(), "6th call over burst=5 must be denied");
    }

    #[test]
    fn recall_refills_over_time() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(None, Some(2));
        let t = t0();
        // Drain the burst.
        assert!(lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t).is_allowed());
        assert!(lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t).is_allowed());
        assert!(!lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t).is_allowed());
        // Wait 1 second → 2 tokens back.
        let t1 = t + Duration::from_secs(1);
        assert!(lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t1).is_allowed());
        assert!(lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t1).is_allowed());
    }

    #[test]
    fn ingest_consumes_batch_size_tokens() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(Some(10), None);
        let now = t0();
        // Ingest of size 4 → consumes 4 of 10 tokens.
        assert!(lim
            .try_acquire_at(&tenant, EndpointKind::Ingest, 4, now)
            .is_allowed());
        // Ingest of size 6 → consumes the remaining 6.
        assert!(lim
            .try_acquire_at(&tenant, EndpointKind::Ingest, 6, now)
            .is_allowed());
        // Ingest of size 1 → denied (0 tokens left).
        assert!(!lim
            .try_acquire_at(&tenant, EndpointKind::Ingest, 1, now)
            .is_allowed());
    }

    #[test]
    fn denied_reports_meaningful_retry_after() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(None, Some(2));
        let now = t0();
        // Drain.
        let _ = lim.try_acquire_at(&tenant, EndpointKind::Recall, 2, now);
        // Ask for one more → deficit of 1 at rate 2 → retry_after ≈ 0.5s
        let d = lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, now);
        match d {
            RateDecision::Denied { retry_after } => {
                assert!(retry_after < Duration::from_secs(1));
                assert!(retry_after > Duration::from_millis(100));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn different_kinds_have_independent_buckets() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(Some(1), Some(1));
        let t = t0();
        assert!(lim.try_acquire_at(&tenant, EndpointKind::Ingest, 1, t).is_allowed());
        assert!(!lim.try_acquire_at(&tenant, EndpointKind::Ingest, 1, t).is_allowed());
        // Recall bucket is separate.
        assert!(lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t).is_allowed());
    }

    #[test]
    fn quota_change_at_runtime_takes_effect_after_refill() {
        let lim = RateLimiter::new();
        let mut tenant = tenant_with_quotas(None, Some(1));
        let t = t0();
        // Drain at rate 1.
        assert!(lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t).is_allowed());
        assert!(!lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t).is_allowed());
        // Bump quota to 10/s.
        tenant.quotas.recall_qps = Some(10);
        // Wait 1s → at the new rate, the bucket refills 10 tokens.
        let t1 = t + Duration::from_secs(1);
        let mut allowed = 0;
        for _ in 0..15 {
            if lim
                .try_acquire_at(&tenant, EndpointKind::Recall, 1, t1)
                .is_allowed()
            {
                allowed += 1;
            }
        }
        assert!(
            allowed >= 10,
            "expected >= 10 acquires after 1s at new rate 10/s, got {allowed}"
        );
    }

    #[test]
    fn forget_tenant_drops_buckets() {
        let lim = RateLimiter::new();
        let tenant = tenant_with_quotas(Some(1), Some(1));
        let t = t0();
        let _ = lim.try_acquire_at(&tenant, EndpointKind::Ingest, 1, t);
        let _ = lim.try_acquire_at(&tenant, EndpointKind::Recall, 1, t);
        assert_eq!(lim.bucket_count(), 2);
        lim.forget_tenant("acme");
        assert_eq!(lim.bucket_count(), 0);
    }

    #[test]
    fn token_bucket_saturates_at_burst() {
        let now = t0();
        let mut b = TokenBucket::new(5.0, now);
        // Wait 1 hour — refill should saturate at burst (5), not accumulate 18000.
        let later = now + Duration::from_secs(3600);
        assert!(matches!(
            b.try_acquire_at(5.0, later),
            RateDecision::Allowed
        ));
        // Sixth token in the same instant would be denied → burst really is 5.
        assert!(!b.try_acquire_at(1.0, later).is_allowed());
    }
}
