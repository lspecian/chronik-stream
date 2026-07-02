//! Per-tenant metrics for `/memory/v1/*` (AM-2.5).
//!
//! Prometheus-compatible atomic counters keyed by
//! `(tenant_id, endpoint, status)`. Emitted as a Prometheus text-format
//! payload on demand.
//!
//! # Cardinality cap
//!
//! For deployments with hundreds of tenants, [`TenantMetrics::format_prometheus`]
//! folds all-but-the-top-N (by ops count) into a synthetic `tenant="other"`
//! bucket. The cap is passed in per call so operators can tune it via env
//! var without touching code.
//!
//! # Wiring
//!
//! Handlers call [`TenantMetrics::record`] after their auth + rate-limit
//! checks (so `caller_tenant` is known). The [`super::UnifiedApiState`]
//! carries an `Arc<TenantMetrics>`; a Prometheus scraper hits
//! `GET /memory/v1/metrics` to pull the payload.
//!
//! # Wire schema (Prometheus)
//!
//! ```text
//! # HELP memory_ops_total Total /memory/v1/* operations per tenant.
//! # TYPE memory_ops_total counter
//! memory_ops_total{tenant="acme",endpoint="recall",status="ok"} 421
//! memory_ops_total{tenant="acme",endpoint="ingest",status="ok"} 15
//! memory_ops_total{tenant="other",endpoint="recall",status="ok"} 3821
//! ```

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The `endpoint` label. Kept as an enum so the label set is closed and
/// operators don't chase typos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MetricEndpoint {
    Ingest,
    Remember,
    Forget,
    Recall,
    Source,
    Feedback,
    InitNamespace,
    Health,
}

impl MetricEndpoint {
    /// Prometheus label value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Remember => "remember",
            Self::Forget => "forget",
            Self::Recall => "recall",
            Self::Source => "source",
            Self::Feedback => "feedback",
            Self::InitNamespace => "init_namespace",
            Self::Health => "health",
        }
    }
}

/// Outcome of the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MetricStatus {
    /// 2xx.
    Ok,
    /// 4xx — includes 429 rate-limited.
    ClientError,
    /// 5xx.
    ServerError,
    /// 429 specifically — a subset of ClientError that operators track
    /// separately to alert on quota exhaustion.
    RateLimited,
}

impl MetricStatus {
    /// Prometheus label value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::RateLimited => "rate_limited",
        }
    }

    /// Bucket an HTTP status code into a metric status. `429` gets its
    /// own dedicated bucket so operators can page on quota exhaustion
    /// separately from generic 4xx noise.
    pub fn from_status(code: u16) -> Self {
        match code {
            200..=299 => Self::Ok,
            429 => Self::RateLimited,
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            // Treat everything else as ClientError to keep the label set closed.
            _ => Self::ClientError,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MetricKey {
    tenant_id: String,
    endpoint: MetricEndpoint,
    status: MetricStatus,
}

/// Per-tenant counters. Cheap to clone (`Arc` interior).
///
/// Three parallel maps: op counts (headline `memory_ops_total`), message
/// counts (`memory_msgs_total` — how many memories touched: batch size
/// for ingest, result count for recall), and latency accumulation
/// (`memory_latency_seconds_sum` / `_count` — a Prometheus summary).
/// A full histogram with fixed buckets is a follow-up; the sum + count
/// pair lets scrapers derive p50 via `rate(_sum) / rate(_count)` and
/// alert on tail latency growth.
#[derive(Debug, Default, Clone)]
pub struct TenantMetrics {
    inner: Arc<DashMap<MetricKey, AtomicU64>>,
    msgs: Arc<DashMap<MetricKey, AtomicU64>>,
    latency_sum_micros: Arc<DashMap<MetricKey, AtomicU64>>,
    latency_count: Arc<DashMap<MetricKey, AtomicU64>>,
}

impl TenantMetrics {
    /// Fresh empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one operation. `tenant_id` is the caller's `X-Tenant-Id`
    /// (or `"anonymous"` when auth is passthrough and the caller sent no
    /// header). Cheap: increments an [`AtomicU64`] on the shared
    /// `DashMap` entry.
    pub fn record(
        &self,
        tenant_id: &str,
        endpoint: MetricEndpoint,
        status: MetricStatus,
    ) {
        let key = MetricKey {
            tenant_id: tenant_id.to_string(),
            endpoint,
            status,
        };
        self.inner
            .entry(key)
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Convenience — bucket an HTTP status code and record.
    pub fn record_status(
        &self,
        tenant_id: &str,
        endpoint: MetricEndpoint,
        code: u16,
    ) {
        self.record(tenant_id, endpoint, MetricStatus::from_status(code));
    }

    /// Add `n` messages to the tenant's `memory_msgs_total` counter for
    /// this `(endpoint, status)`. Called after
    /// [`Self::record_status`] so both counters share the same key.
    /// `n` is the batch size for ingest, the returned result count for
    /// recall, `1` for point ops like remember/forget/source.
    /// Zero-cost when `n == 0`.
    pub fn record_msgs(
        &self,
        tenant_id: &str,
        endpoint: MetricEndpoint,
        status: MetricStatus,
        n: u64,
    ) {
        if n == 0 {
            return;
        }
        let key = MetricKey {
            tenant_id: tenant_id.to_string(),
            endpoint,
            status,
        };
        self.msgs
            .entry(key)
            .or_default()
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Observe a latency measurement. Called at end-of-handler with the
    /// handler's `Instant::elapsed()`. Stored as
    /// `(sum_of_micros, count)` so the Prometheus summary can compute
    /// `avg = sum_micros / count` and scrapers can drop it into their
    /// PromQL as `_sum` / `_count`.
    pub fn observe_latency(
        &self,
        tenant_id: &str,
        endpoint: MetricEndpoint,
        status: MetricStatus,
        elapsed: std::time::Duration,
    ) {
        let key = MetricKey {
            tenant_id: tenant_id.to_string(),
            endpoint,
            status,
        };
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.latency_sum_micros
            .entry(key.clone())
            .or_default()
            .fetch_add(micros, Ordering::Relaxed);
        self.latency_count
            .entry(key)
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Emit a Prometheus text-format payload.
    ///
    /// Every `(tenant, endpoint, status)` triple emits its own line UNLESS
    /// `cap` is `Some(N)` and the tenant is outside the top-N tenants by
    /// total ops — in which case the counter folds into a synthetic
    /// `tenant="other"` line.
    pub fn format_prometheus(&self, cap: Option<usize>) -> String {
        let mut out = String::new();
        out.push_str("# HELP memory_ops_total Total /memory/v1/* operations per tenant.\n");
        out.push_str("# TYPE memory_ops_total counter\n");

        // Materialize a snapshot to sort deterministically.
        let snap: Vec<(MetricKey, u64)> = self
            .inner
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().load(Ordering::Relaxed)))
            .collect();

        // Aggregate per-tenant totals for the cap.
        let (top_tenants, other_total_by_ep_st): (
            std::collections::HashSet<String>,
            std::collections::BTreeMap<(MetricEndpoint, MetricStatus), u64>,
        ) = match cap {
            Some(n) if n > 0 => {
                let mut totals: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                for (k, v) in &snap {
                    *totals.entry(k.tenant_id.clone()).or_default() += v;
                }
                let mut ranked: Vec<(String, u64)> = totals.into_iter().collect();
                ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                let top: std::collections::HashSet<String> =
                    ranked.into_iter().take(n).map(|(t, _)| t).collect();
                let mut folded: std::collections::BTreeMap<
                    (MetricEndpoint, MetricStatus),
                    u64,
                > = std::collections::BTreeMap::new();
                for (k, v) in &snap {
                    if !top.contains(&k.tenant_id) {
                        *folded.entry((k.endpoint, k.status)).or_default() += v;
                    }
                }
                (top, folded)
            }
            _ => (std::collections::HashSet::new(), std::collections::BTreeMap::new()),
        };

        // Emit per-triple lines for top tenants (or all when cap is None
        // or Some(0)). Some(0) is treated identically to None — "no cap".
        let no_cap = matches!(cap, None | Some(0));
        let mut sortable: Vec<(MetricKey, u64)> = snap
            .into_iter()
            .filter(|(k, _)| no_cap || top_tenants.contains(&k.tenant_id))
            .collect();
        sortable.sort_by(|a, b| {
            a.0.tenant_id
                .cmp(&b.0.tenant_id)
                .then_with(|| a.0.endpoint.as_str().cmp(b.0.endpoint.as_str()))
                .then_with(|| a.0.status.as_str().cmp(b.0.status.as_str()))
        });
        for (k, v) in sortable {
            out.push_str(&format!(
                "memory_ops_total{{tenant={:?},endpoint={:?},status={:?}}} {}\n",
                k.tenant_id,
                k.endpoint.as_str(),
                k.status.as_str(),
                v,
            ));
        }
        // Folded `other` bucket.
        for ((endpoint, status), total) in other_total_by_ep_st {
            out.push_str(&format!(
                "memory_ops_total{{tenant=\"other\",endpoint={:?},status={:?}}} {}\n",
                endpoint.as_str(),
                status.as_str(),
                total,
            ));
        }

        // ── memory_msgs_total ────────────────────────────────────────
        Self::format_family(
            &mut out,
            &self.msgs,
            cap,
            "memory_msgs_total",
            "Total memories touched (batch size for ingest, result count for recall).",
            /* is_micros = */ false,
        );

        // ── memory_latency_seconds_sum / _count (Prometheus summary) ─
        // Sum first (in seconds, converted from micros), then count.
        Self::format_family(
            &mut out,
            &self.latency_sum_micros,
            cap,
            "memory_latency_seconds_sum",
            "Cumulative /memory/v1/* latency in seconds (Prometheus summary sum).",
            /* is_micros = */ true,
        );
        Self::format_family(
            &mut out,
            &self.latency_count,
            cap,
            "memory_latency_seconds_count",
            "Number of /memory/v1/* observations (Prometheus summary count).",
            /* is_micros = */ false,
        );
        out
    }

    /// Emit one metric family. `is_micros=true` divides the raw
    /// AtomicU64 by 1_000_000 and formats as f64 — used for the
    /// latency-sum family which stores micros internally.
    fn format_family(
        out: &mut String,
        map: &Arc<DashMap<MetricKey, AtomicU64>>,
        cap: Option<usize>,
        name: &str,
        help: &str,
        is_micros: bool,
    ) {
        out.push_str(&format!("# HELP {name} {help}\n"));
        out.push_str(&format!("# TYPE {name} counter\n"));

        let snap: Vec<(MetricKey, u64)> = map
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().load(Ordering::Relaxed)))
            .collect();

        let (top_tenants, folded): (
            std::collections::HashSet<String>,
            std::collections::BTreeMap<(MetricEndpoint, MetricStatus), u64>,
        ) = match cap {
            Some(n) if n > 0 => {
                let mut totals: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                for (k, v) in &snap {
                    *totals.entry(k.tenant_id.clone()).or_default() += v;
                }
                let mut ranked: Vec<(String, u64)> = totals.into_iter().collect();
                ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                let top: std::collections::HashSet<String> =
                    ranked.into_iter().take(n).map(|(t, _)| t).collect();
                let mut folded: std::collections::BTreeMap<
                    (MetricEndpoint, MetricStatus),
                    u64,
                > = std::collections::BTreeMap::new();
                for (k, v) in &snap {
                    if !top.contains(&k.tenant_id) {
                        *folded.entry((k.endpoint, k.status)).or_default() += v;
                    }
                }
                (top, folded)
            }
            _ => (
                std::collections::HashSet::new(),
                std::collections::BTreeMap::new(),
            ),
        };

        let no_cap = matches!(cap, None | Some(0));
        let mut sortable: Vec<(MetricKey, u64)> = snap
            .into_iter()
            .filter(|(k, _)| no_cap || top_tenants.contains(&k.tenant_id))
            .collect();
        sortable.sort_by(|a, b| {
            a.0.tenant_id
                .cmp(&b.0.tenant_id)
                .then_with(|| a.0.endpoint.as_str().cmp(b.0.endpoint.as_str()))
                .then_with(|| a.0.status.as_str().cmp(b.0.status.as_str()))
        });
        for (k, v) in sortable {
            if is_micros {
                let secs = (v as f64) / 1_000_000.0;
                out.push_str(&format!(
                    "{name}{{tenant={:?},endpoint={:?},status={:?}}} {secs}\n",
                    k.tenant_id,
                    k.endpoint.as_str(),
                    k.status.as_str(),
                ));
            } else {
                out.push_str(&format!(
                    "{name}{{tenant={:?},endpoint={:?},status={:?}}} {v}\n",
                    k.tenant_id,
                    k.endpoint.as_str(),
                    k.status.as_str(),
                ));
            }
        }
        for ((endpoint, status), total) in folded {
            if is_micros {
                let secs = (total as f64) / 1_000_000.0;
                out.push_str(&format!(
                    "{name}{{tenant=\"other\",endpoint={:?},status={:?}}} {secs}\n",
                    endpoint.as_str(),
                    status.as_str(),
                ));
            } else {
                out.push_str(&format!(
                    "{name}{{tenant=\"other\",endpoint={:?},status={:?}}} {total}\n",
                    endpoint.as_str(),
                    status.as_str(),
                ));
            }
        }
    }

    /// Test helper: read one counter's current value.
    #[doc(hidden)]
    pub fn peek(
        &self,
        tenant_id: &str,
        endpoint: MetricEndpoint,
        status: MetricStatus,
    ) -> u64 {
        let key = MetricKey {
            tenant_id: tenant_id.to_string(),
            endpoint,
            status,
        };
        self.inner
            .get(&key)
            .map(|v| v.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Number of distinct `(tenant, endpoint, status)` counters stored.
    /// Useful for cardinality assertions in tests.
    pub fn cardinality(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_increments_the_counter() {
        let m = TenantMetrics::new();
        m.record("acme", MetricEndpoint::Recall, MetricStatus::Ok);
        m.record("acme", MetricEndpoint::Recall, MetricStatus::Ok);
        m.record("acme", MetricEndpoint::Recall, MetricStatus::Ok);
        assert_eq!(
            m.peek("acme", MetricEndpoint::Recall, MetricStatus::Ok),
            3
        );
    }

    #[test]
    fn distinct_triples_are_independent() {
        let m = TenantMetrics::new();
        m.record("acme", MetricEndpoint::Ingest, MetricStatus::Ok);
        m.record("acme", MetricEndpoint::Recall, MetricStatus::Ok);
        m.record("beta", MetricEndpoint::Recall, MetricStatus::Ok);
        assert_eq!(m.cardinality(), 3);
    }

    #[test]
    fn record_status_buckets_correctly() {
        assert_eq!(MetricStatus::from_status(200), MetricStatus::Ok);
        assert_eq!(MetricStatus::from_status(202), MetricStatus::Ok);
        assert_eq!(MetricStatus::from_status(400), MetricStatus::ClientError);
        assert_eq!(MetricStatus::from_status(401), MetricStatus::ClientError);
        assert_eq!(MetricStatus::from_status(404), MetricStatus::ClientError);
        assert_eq!(MetricStatus::from_status(429), MetricStatus::RateLimited);
        assert_eq!(MetricStatus::from_status(500), MetricStatus::ServerError);
        assert_eq!(MetricStatus::from_status(503), MetricStatus::ServerError);
        // Odd codes get bucketed as ClientError.
        assert_eq!(MetricStatus::from_status(0), MetricStatus::ClientError);
    }

    #[test]
    fn format_prometheus_no_cap_lists_every_triple() {
        let m = TenantMetrics::new();
        m.record_status("acme", MetricEndpoint::Recall, 200);
        m.record_status("acme", MetricEndpoint::Ingest, 202);
        m.record_status("beta", MetricEndpoint::Recall, 429);
        let out = m.format_prometheus(None);
        assert!(out.contains("# TYPE memory_ops_total counter"));
        assert!(out.contains(r#"memory_ops_total{tenant="acme",endpoint="recall",status="ok"} 1"#));
        assert!(out.contains(r#"memory_ops_total{tenant="acme",endpoint="ingest",status="ok"} 1"#));
        assert!(out.contains(
            r#"memory_ops_total{tenant="beta",endpoint="recall",status="rate_limited"} 1"#
        ));
    }

    #[test]
    fn format_prometheus_with_cap_folds_beyond_top_n() {
        let m = TenantMetrics::new();
        // Top 2: acme (10) and beta (5).
        for _ in 0..10 {
            m.record_status("acme", MetricEndpoint::Recall, 200);
        }
        for _ in 0..5 {
            m.record_status("beta", MetricEndpoint::Recall, 200);
        }
        // Tail: gamma, delta, epsilon each get 1 op.
        for t in ["gamma", "delta", "epsilon"] {
            m.record_status(t, MetricEndpoint::Recall, 200);
        }
        let out = m.format_prometheus(Some(2));
        assert!(out.contains(r#"tenant="acme""#));
        assert!(out.contains(r#"tenant="beta""#));
        assert!(!out.contains(r#"tenant="gamma""#), "gamma should fold");
        assert!(!out.contains(r#"tenant="delta""#), "delta should fold");
        assert!(!out.contains(r#"tenant="epsilon""#), "epsilon should fold");
        assert!(
            out.contains(r#"memory_ops_total{tenant="other",endpoint="recall",status="ok"} 3"#),
            "3 tail tenants folded into 'other', got:\n{out}"
        );
    }

    #[test]
    fn format_prometheus_cap_zero_and_none_both_include_all() {
        let m = TenantMetrics::new();
        m.record_status("acme", MetricEndpoint::Recall, 200);
        let none_out = m.format_prometheus(None);
        let zero_out = m.format_prometheus(Some(0));
        assert!(none_out.contains(r#"tenant="acme""#));
        assert!(zero_out.contains(r#"tenant="acme""#));
        assert!(!zero_out.contains(r#"tenant="other""#));
    }

    #[test]
    fn empty_registry_still_emits_help_and_type_headers() {
        let m = TenantMetrics::new();
        let out = m.format_prometheus(None);
        assert!(out.contains("# HELP memory_ops_total"));
        assert!(out.contains("# TYPE memory_ops_total counter"));
        assert!(out.contains("# HELP memory_msgs_total"));
        assert!(out.contains("# HELP memory_latency_seconds_sum"));
        assert!(out.contains("# HELP memory_latency_seconds_count"));
    }

    #[test]
    fn record_msgs_accumulates_and_emits() {
        let m = TenantMetrics::new();
        m.record_msgs("acme", MetricEndpoint::Ingest, MetricStatus::Ok, 5);
        m.record_msgs("acme", MetricEndpoint::Ingest, MetricStatus::Ok, 7);
        m.record_msgs("beta", MetricEndpoint::Recall, MetricStatus::Ok, 10);
        let out = m.format_prometheus(None);
        assert!(out.contains("memory_msgs_total{tenant=\"acme\",endpoint=\"ingest\",status=\"ok\"} 12"));
        assert!(out.contains("memory_msgs_total{tenant=\"beta\",endpoint=\"recall\",status=\"ok\"} 10"));
    }

    #[test]
    fn record_msgs_zero_is_noop() {
        let m = TenantMetrics::new();
        m.record_msgs("acme", MetricEndpoint::Ingest, MetricStatus::Ok, 0);
        let out = m.format_prometheus(None);
        assert!(
            !out.contains("memory_msgs_total{tenant=\"acme\""),
            "0-count msgs must not create a counter row"
        );
    }

    #[test]
    fn observe_latency_records_sum_and_count() {
        let m = TenantMetrics::new();
        m.observe_latency(
            "acme",
            MetricEndpoint::Recall,
            MetricStatus::Ok,
            std::time::Duration::from_millis(50),
        );
        m.observe_latency(
            "acme",
            MetricEndpoint::Recall,
            MetricStatus::Ok,
            std::time::Duration::from_millis(150),
        );
        let out = m.format_prometheus(None);
        // 50ms + 150ms = 200_000 micros = 0.2 seconds. Formatter emits
        // `0.2` as-is.
        assert!(
            out.contains(
                "memory_latency_seconds_sum{tenant=\"acme\",endpoint=\"recall\",status=\"ok\"} 0.2"
            ),
            "got:\n{out}"
        );
        assert!(out.contains(
            "memory_latency_seconds_count{tenant=\"acme\",endpoint=\"recall\",status=\"ok\"} 2"
        ));
    }

    #[test]
    fn cap_applies_to_msgs_and_latency_families_too() {
        let m = TenantMetrics::new();
        // Two tenants — cap=1 folds the loser into "other".
        m.record_msgs("acme", MetricEndpoint::Ingest, MetricStatus::Ok, 100);
        m.record_msgs("beta", MetricEndpoint::Ingest, MetricStatus::Ok, 5);
        m.observe_latency(
            "acme",
            MetricEndpoint::Recall,
            MetricStatus::Ok,
            std::time::Duration::from_millis(10),
        );
        m.observe_latency(
            "beta",
            MetricEndpoint::Recall,
            MetricStatus::Ok,
            std::time::Duration::from_millis(10),
        );
        let out = m.format_prometheus(Some(1));
        // acme wins the top-1 slot in the msgs family; beta's msgs
        // fold into `tenant="other"`.
        assert!(out.contains("memory_msgs_total{tenant=\"acme\""));
        assert!(out.contains("memory_msgs_total{tenant=\"other\""));
        assert!(
            !out.contains("memory_msgs_total{tenant=\"beta\""),
            "beta should have been folded into 'other'"
        );
    }
}
