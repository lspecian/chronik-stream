//! Audit-log emitter for `/memory/v1/*` operations (AM-2.6).
//!
//! Every recall and write produces an [`AuditEvent`] on `mem.audit.{tenant}`.
//! The topic is append-only (no compaction) with `columnar.enabled=true` so
//! the audit trail is queryable via SQL for compliance reporting.
//!
//! Wire shape: handlers in `chronik-server` call `Memory::audit(event)` after
//! producing the primary result. Failure is logged but does not propagate —
//! audit emission must never block the user-visible path.
//!
//! Schema is wire-stable (Confluent-compatible); future fields are additive
//! only (matches Schema Registry `forward` compatibility mode).

use crate::error::Result;
use crate::ingest::IngestAck;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One row in the audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Wall-clock timestamp of the operation (server-side).
    pub ts: DateTime<Utc>,
    /// Tenant whose audit log this row lives on.
    pub tenant: String,
    /// Full namespace path the operation targeted.
    pub namespace: String,
    /// Operation name: `recall`, `ingest`, `remember`, `forget`,
    /// `init-namespace`, `feedback`.
    pub op: String,
    /// Free-text query (for recall) or summary (for writes). Truncated to
    /// 512 chars at emit time to keep the audit topic compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Memory IDs touched by the operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_ids: Vec<String>,
    /// Caller-supplied tenant header (post-auth). Helps detect attempted
    /// cross-tenant access even when the read itself succeeded (Phase 1
    /// passthrough doesn't reject these — auditing them surfaces the gap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_tenant: Option<String>,
    /// HTTP status code returned to the caller.
    pub status_code: u16,
    /// Stable error code on non-2xx responses; `None` on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Latency from request-receive to response-send, in milliseconds.
    #[serde(default)]
    pub latency_ms: u64,
}

impl AuditEvent {
    /// Construct a success event. Use [`with_*`] setters to add optional fields.
    pub fn ok(tenant: impl Into<String>, namespace: impl Into<String>, op: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            tenant: tenant.into(),
            namespace: namespace.into(),
            op: op.into(),
            query: None,
            memory_ids: Vec::new(),
            caller_tenant: None,
            status_code: 200,
            error_code: None,
            latency_ms: 0,
        }
    }

    /// Construct an error event.
    pub fn err(
        tenant: impl Into<String>,
        namespace: impl Into<String>,
        op: impl Into<String>,
        status_code: u16,
        error_code: impl Into<String>,
    ) -> Self {
        Self {
            ts: Utc::now(),
            tenant: tenant.into(),
            namespace: namespace.into(),
            op: op.into(),
            query: None,
            memory_ids: Vec::new(),
            caller_tenant: None,
            status_code,
            error_code: Some(error_code.into()),
            latency_ms: 0,
        }
    }

    pub fn with_query(mut self, q: impl Into<String>) -> Self {
        let mut s: String = q.into();
        if s.len() > 512 {
            s.truncate(512);
            s.push('…');
        }
        self.query = Some(s);
        self
    }

    pub fn with_memory_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.memory_ids = ids.into_iter().collect();
        self
    }

    pub fn with_caller_tenant(mut self, t: impl Into<String>) -> Self {
        self.caller_tenant = Some(t.into());
        self
    }

    pub fn with_latency_ms(mut self, ms: u64) -> Self {
        self.latency_ms = ms;
        self
    }

    pub fn with_status_code(mut self, code: u16) -> Self {
        self.status_code = code;
        self
    }
}

/// Build the audit topic name for a tenant.
pub fn audit_topic(tenant: &str) -> String {
    format!("mem.audit.{tenant}")
}

/// Best-effort produce of an [`AuditEvent`] to `mem.audit.{tenant}`.
///
/// Returns `Err` only on configuration problems (e.g. missing producer);
/// transport errors are logged and swallowed so the caller's hot path stays
/// undisturbed. The audit topic must be created beforehand
/// (via [`crate::Memory::init_namespace_full`](crate::Memory::init_namespace_full)
/// or out-of-band).
pub async fn emit_audit(
    producer: &rdkafka::producer::FutureProducer,
    event: &AuditEvent,
) -> Result<IngestAck> {
    use rdkafka::producer::FutureRecord;
    use rdkafka::util::Timeout;
    use std::time::Duration;

    let topic = audit_topic(&event.tenant);
    let payload = serde_json::to_vec(event)
        .map_err(|e| crate::error::MemoryError::Schema(format!("audit event JSON: {e}")))?;
    // Key by namespace so partition assignment groups one tenant's audit
    // records together — easier to scan SQL later.
    let key = event.namespace.as_bytes();
    let record: FutureRecord<'_, [u8], [u8]> = FutureRecord::to(&topic)
        .payload(payload.as_slice())
        .key(key);

    match producer
        .send(record, Timeout::After(Duration::from_secs(5)))
        .await
    {
        Ok((partition, offset)) => {
            tracing::debug!(
                topic = %topic,
                op = %event.op,
                partition,
                offset,
                "audit event emitted"
            );
            Ok(IngestAck {
                topic,
                partition,
                offset,
                deduped: false,
            })
        }
        Err((kafka_err, _)) => {
            // Audit topic likely doesn't exist yet, or broker is down. Don't
            // fail the caller's request — log + swallow.
            tracing::warn!(
                topic = %topic,
                op = %event.op,
                error = %kafka_err,
                "audit emission failed (best-effort)"
            );
            Err(crate::error::MemoryError::Kafka(format!(
                "audit emit: {kafka_err}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_topic_name_format() {
        assert_eq!(audit_topic("acme"), "mem.audit.acme");
    }

    #[test]
    fn ok_event_has_200_status() {
        let e = AuditEvent::ok("acme", "acme:agent:bot:user:luis", "recall");
        assert_eq!(e.status_code, 200);
        assert!(e.error_code.is_none());
        assert!(e.query.is_none());
        assert!(e.memory_ids.is_empty());
    }

    #[test]
    fn err_event_carries_code() {
        let e = AuditEvent::err("acme", "acme:agent:bot:_:_", "recall", 503, "service_unavailable");
        assert_eq!(e.status_code, 503);
        assert_eq!(e.error_code.as_deref(), Some("service_unavailable"));
    }

    #[test]
    fn with_query_truncates_long_strings() {
        let long: String = "x".repeat(600);
        let e = AuditEvent::ok("t", "t:n", "recall").with_query(long);
        let q = e.query.unwrap();
        // Truncated to 512 chars plus the ellipsis byte length (3 UTF-8 bytes).
        assert!(q.chars().count() <= 513, "expected ≤513 chars after truncation, got {}", q.chars().count());
        assert!(q.ends_with('…'));
    }

    #[test]
    fn with_memory_ids_round_trips() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let e = AuditEvent::ok("t", "t:n", "recall").with_memory_ids(ids.clone());
        assert_eq!(e.memory_ids, ids);
    }

    #[test]
    fn with_caller_tenant_records_cross_tenant_attempts() {
        let e = AuditEvent::ok("acme", "acme:n", "recall").with_caller_tenant("attacker");
        assert_eq!(e.caller_tenant.as_deref(), Some("attacker"));
    }

    #[test]
    fn event_serializes_to_stable_json() {
        // Fixed values so the snapshot is deterministic. AuditEvent::ts uses
        // Utc::now in constructors; for this test we patch it after.
        let mut e = AuditEvent::ok("acme", "acme:agent:bot:user:luis", "recall");
        e.ts = DateTime::parse_from_rfc3339("2026-06-28T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        e.query = Some("what is the user's max budget?".into());
        e.memory_ids = vec!["01HX".into()];
        e.latency_ms = 42;
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"tenant\":\"acme\""));
        assert!(s.contains("\"op\":\"recall\""));
        assert!(s.contains("\"status_code\":200"));
        assert!(s.contains("\"latency_ms\":42"));
        // Missing-by-default fields are omitted.
        assert!(!s.contains("\"error_code\""));
        assert!(!s.contains("\"caller_tenant\""));
    }
}
