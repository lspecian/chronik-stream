//! Ingest path — produce raw conversation turns to `mem.raw.{ns}`.
//!
//! Hot path. Target latency p50 < 5ms (Rust + rdkafka FutureProducer).
//!
//! Idempotency: caller may pass `external_id` (preferred — used as the Kafka
//! record key, so compaction-style dedup works cross-process). Without it, the
//! SDK falls back to SHA-256(`namespace || role || content`) for in-process
//! dedup via [`crate::idempotency`].

use crate::error::{MemoryError, Result};
use crate::extractor::Turn;
use chrono::Utc;
use rdkafka::producer::FutureRecord;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Acknowledgement of a single produced turn.
#[derive(Debug, Clone)]
pub struct IngestAck {
    /// Topic the record was produced to.
    pub topic: String,
    /// Partition assigned by the broker.
    pub partition: i32,
    /// Offset within the partition.
    pub offset: i64,
    /// `true` when the in-process idempotency cache classified this as a duplicate
    /// and the produce was skipped. `partition`/`offset` are then `-1`.
    pub deduped: bool,
}

impl IngestAck {
    /// Sentinel value returned for in-process duplicates.
    pub fn deduped(topic: String) -> Self {
        Self {
            topic,
            partition: -1,
            offset: -1,
            deduped: true,
        }
    }
}

/// Wire-format payload of a raw turn record on `mem.raw.*`.
///
/// Distinct from [`Turn`] only in that `ts` is required (defaulted to `now` if
/// the caller didn't provide one) and `received_at` is set by the SDK.
#[derive(Debug, Clone, Serialize)]
pub struct RawTurnRecord<'a> {
    /// Source role.
    pub role: &'a str,
    /// Message content.
    pub content: &'a str,
    /// Effective timestamp (caller-provided or now).
    pub ts: chrono::DateTime<Utc>,
    /// Wall-clock time the SDK accepted the turn.
    pub received_at: chrono::DateTime<Utc>,
    /// Channel hint, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<&'a str>,
    /// Caller-provided upstream ID, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<&'a str>,
    /// Namespace this turn belongs to (echoed for downstream consumers).
    pub namespace: &'a str,
}

/// Build the Kafka record key for a turn.
///
/// Preference order:
/// 1. Caller-supplied `external_id` (cross-process strict idempotency)
/// 2. SHA-256 of `(namespace || '\0' || role || '\0' || content)` (in-process)
pub(crate) fn turn_record_key(namespace: &str, turn: &Turn) -> String {
    if let Some(eid) = &turn.external_id {
        return eid.clone();
    }
    let mut h = Sha256::new();
    h.update(namespace.as_bytes());
    h.update(b"\0");
    h.update(turn.role.as_bytes());
    h.update(b"\0");
    h.update(turn.content.as_bytes());
    let digest = h.finalize();
    hex::encode(digest)
}

/// Construct a `FutureRecord` for a serialized payload.
///
/// Caller supplies the topic and the serialized JSON bytes. We borrow the key
/// for the lifetime of the record.
///
/// Currently unused — the client constructs records inline. Kept for future
/// helpers (e.g. an mpsc-based produce queue in AMS-2.2).
#[allow(dead_code)]
pub(crate) fn build_record<'a>(
    topic: &'a str,
    key: &'a str,
    payload: &'a [u8],
) -> FutureRecord<'a, str, [u8]> {
    FutureRecord::to(topic).key(key).payload(payload)
}

/// Validate basic invariants on a turn before produce. Cheap.
pub(crate) fn validate_turn(turn: &Turn) -> Result<()> {
    if turn.role.is_empty() {
        return Err(MemoryError::InvalidArgument("turn.role is empty".into()));
    }
    if turn.content.is_empty() {
        return Err(MemoryError::InvalidArgument(
            "turn.content is empty".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, content: &str) -> Turn {
        Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: None,
        }
    }

    #[test]
    fn record_key_uses_external_id_when_set() {
        let t = Turn {
            external_id: Some("wa-msg-9182".into()),
            ..turn("user", "hi")
        };
        assert_eq!(turn_record_key("ns", &t), "wa-msg-9182");
    }

    #[test]
    fn record_key_falls_back_to_sha256() {
        let t = turn("user", "hello world");
        let k1 = turn_record_key("ns1", &t);
        let k2 = turn_record_key("ns1", &t);
        assert_eq!(k1, k2, "deterministic for same inputs");
        assert_eq!(k1.len(), 64, "sha256 hex = 64 chars");
    }

    #[test]
    fn record_key_namespace_separation() {
        let t = turn("user", "hello");
        assert_ne!(
            turn_record_key("ns1", &t),
            turn_record_key("ns2", &t),
            "different namespaces must produce different keys"
        );
    }

    #[test]
    fn validate_rejects_empty_role_or_content() {
        assert!(validate_turn(&turn("", "x")).is_err());
        assert!(validate_turn(&turn("user", "")).is_err());
        assert!(validate_turn(&turn("user", "hi")).is_ok());
    }

    #[test]
    fn ingest_ack_deduped_sentinel() {
        let a = IngestAck::deduped("mem.raw.acme".into());
        assert!(a.deduped);
        assert_eq!(a.partition, -1);
        assert_eq!(a.offset, -1);
    }
}
