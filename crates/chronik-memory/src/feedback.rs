//! AM-3.3 agent feedback loop — `POST /memory/v1/feedback` emitter.
//!
//! Every feedback signal writes a row to `mem.feedback.{tenant}`:
//! - `memory_id` — the memory the agent judged.
//! - `namespace` — full namespace path (drives partition assignment).
//! - `recall_query` — the query that surfaced the memory (optional).
//! - `useful: bool` — did this memory help.
//! - `used_in_response: bool` — did the agent actually cite it.
//! - `caller_tenant` — post-auth tenant (nullable during Phase 1
//!   passthrough).
//! - `ts` — server-side timestamp.
//!
//! The topic is append-only + columnar-enabled (see
//! [`crate::topics::TopicConfig::feedback`]) — offline reranker training
//! reads it via SQL. Feedback emission is best-effort; a produce
//! failure is logged and swallowed so the caller's response is never
//! blocked by observability.

use crate::error::Result;
use crate::ingest::IngestAck;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One row on `mem.feedback.{tenant}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeedbackEvent {
    /// Wall-clock timestamp of the feedback signal.
    pub ts: DateTime<Utc>,
    /// Tenant whose feedback log this row lives on.
    pub tenant: String,
    /// Full namespace path (drives partition assignment).
    pub namespace: String,
    /// The memory the agent judged.
    pub memory_id: String,
    /// Query that surfaced the memory. Truncated to 512 chars at emit
    /// time to keep the feedback topic compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_query: Option<String>,
    /// Was this memory useful for the agent's response?
    pub useful: bool,
    /// Did the agent actually cite this memory in its response?
    pub used_in_response: bool,
    /// Post-auth caller — for cross-tenant access auditing. `None` in
    /// Phase 1 passthrough deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_tenant: Option<String>,
}

impl FeedbackEvent {
    /// Construct a fresh event; caller-supplied fields wrapped in
    /// `Option` are set to `None` by default.
    pub fn new(
        tenant: impl Into<String>,
        namespace: impl Into<String>,
        memory_id: impl Into<String>,
        useful: bool,
        used_in_response: bool,
    ) -> Self {
        Self {
            ts: Utc::now(),
            tenant: tenant.into(),
            namespace: namespace.into(),
            memory_id: memory_id.into(),
            recall_query: None,
            useful,
            used_in_response,
            caller_tenant: None,
        }
    }

    /// Attach the recall query that surfaced the memory. Truncates to
    /// 512 chars.
    pub fn with_query(mut self, q: impl Into<String>) -> Self {
        let mut s = q.into();
        if s.len() > 512 {
            s.truncate(512);
        }
        self.recall_query = Some(s);
        self
    }

    /// Attach the caller-tenant post-auth header.
    pub fn with_caller_tenant(mut self, t: impl Into<String>) -> Self {
        self.caller_tenant = Some(t.into());
        self
    }
}

/// Build the feedback topic name for a tenant.
pub fn feedback_topic(tenant: &str) -> String {
    format!("mem.feedback.{tenant}")
}

/// Best-effort produce of a [`FeedbackEvent`] to `mem.feedback.{tenant}`.
///
/// Returns `Err` only on configuration problems (missing producer);
/// transport errors are logged and swallowed. The feedback topic must
/// be created beforehand (via
/// [`crate::Memory::init_namespace_full`](crate::Memory::init_namespace_full)
/// or out-of-band).
pub async fn emit_feedback(
    producer: &rdkafka::producer::FutureProducer,
    event: &FeedbackEvent,
) -> Result<IngestAck> {
    use rdkafka::producer::FutureRecord;
    use rdkafka::util::Timeout;
    use std::time::Duration;

    let topic = feedback_topic(&event.tenant);
    let payload = serde_json::to_vec(event)
        .map_err(|e| crate::error::MemoryError::Schema(format!("feedback event JSON: {e}")))?;
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
                memory_id = %event.memory_id,
                useful = event.useful,
                used = event.used_in_response,
                partition,
                offset,
                "feedback event emitted"
            );
            Ok(IngestAck {
                topic,
                partition,
                offset,
                deduped: false,
            })
        }
        Err((kafka_err, _)) => {
            tracing::warn!(
                topic = %topic,
                memory_id = %event.memory_id,
                error = %kafka_err,
                "feedback emission failed (best-effort)"
            );
            Err(crate::error::MemoryError::Kafka(format!(
                "feedback emit: {kafka_err}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_topic_name_format() {
        assert_eq!(feedback_topic("acme"), "mem.feedback.acme");
    }

    #[test]
    fn new_event_defaults_are_sensible() {
        let e = FeedbackEvent::new("acme", "acme:agent:bot", "mem-1", true, false);
        assert_eq!(e.tenant, "acme");
        assert_eq!(e.memory_id, "mem-1");
        assert!(e.useful);
        assert!(!e.used_in_response);
        assert!(e.recall_query.is_none());
        assert!(e.caller_tenant.is_none());
    }

    #[test]
    fn with_query_truncates_over_512() {
        let long = "x".repeat(1000);
        let e = FeedbackEvent::new("acme", "ns", "m", true, true).with_query(long);
        assert_eq!(e.recall_query.as_ref().unwrap().len(), 512);
    }

    #[test]
    fn with_query_under_512_is_unchanged() {
        let e = FeedbackEvent::new("acme", "ns", "m", true, true).with_query("hello");
        assert_eq!(e.recall_query.as_deref(), Some("hello"));
    }

    #[test]
    fn with_caller_tenant_sets_field() {
        let e = FeedbackEvent::new("acme", "ns", "m", true, true).with_caller_tenant("beta");
        assert_eq!(e.caller_tenant.as_deref(), Some("beta"));
    }

    #[test]
    fn json_roundtrip_preserves_all_fields() {
        let e = FeedbackEvent::new("acme", "ns", "m", true, false)
            .with_query("hello world")
            .with_caller_tenant("acme");
        let s = serde_json::to_string(&e).unwrap();
        let back: FeedbackEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn json_omits_optional_fields_when_none() {
        let e = FeedbackEvent::new("acme", "ns", "m", true, true);
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("recall_query"));
        assert!(!s.contains("caller_tenant"));
    }
}
