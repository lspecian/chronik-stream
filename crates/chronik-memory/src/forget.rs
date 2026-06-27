//! Tombstone helper — emits a null-value Kafka record on a compacted typed topic.
//!
//! Log compaction will remove the prior record with the same key on the next
//! compaction tick. For non-compacted topics (events), forget is a no-op
//! semantically — events are append-only and can only be hidden via SQL filters
//! or downstream policy, not deleted from the log.
//!
//! Resolution order for the Kafka record key:
//! 1. Caller passed `key=Some("...")` — namespace-qualify it: `{namespace}|{key}`
//! 2. Caller passed `memory_id=Some("...")` — use it as-is (events only)
//!
//! Caller must supply exactly one. Both or neither = `InvalidArgument`.

use crate::error::{MemoryError, Result};
use crate::schema::MemoryType;

/// Resolve the tombstone key + topic for a forget request.
///
/// Returns `(topic, kafka_record_key)`.
pub(crate) fn resolve_tombstone_target(
    tenant: &str,
    namespace: &str,
    kind: MemoryType,
    key: Option<&str>,
    memory_id: Option<&str>,
) -> Result<(String, String)> {
    match (key, memory_id) {
        (Some(k), None) => {
            if k.is_empty() {
                return Err(MemoryError::InvalidArgument("forget key is empty".into()));
            }
            // Compactable types only — forgetting an event by key isn't meaningful.
            if matches!(kind, MemoryType::Event) {
                return Err(MemoryError::InvalidArgument(
                    "events are append-only — use forget(memory_id=...) and accept best-effort"
                        .into(),
                ));
            }
            let topic = format!("mem.{}.{}", kind.as_str(), tenant);
            let record_key = format!("{}|{}", namespace, k);
            Ok((topic, record_key))
        }
        (None, Some(id)) => {
            if id.is_empty() {
                return Err(MemoryError::InvalidArgument(
                    "forget memory_id is empty".into(),
                ));
            }
            let topic = format!("mem.{}.{}", kind.as_str(), tenant);
            Ok((topic, id.to_string()))
        }
        (Some(_), Some(_)) => Err(MemoryError::InvalidArgument(
            "forget: pass exactly one of `key` or `memory_id`, not both".into(),
        )),
        (None, None) => Err(MemoryError::InvalidArgument(
            "forget: pass either `key` or `memory_id`".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_by_key_on_fact_topic() {
        let (t, k) = resolve_tombstone_target(
            "acme",
            "agent:bot",
            MemoryType::Fact,
            Some("user:luis|occupation"),
            None,
        )
        .unwrap();
        assert_eq!(t, "mem.fact.acme");
        assert_eq!(k, "agent:bot|user:luis|occupation");
    }

    #[test]
    fn forget_by_memory_id_on_event_topic() {
        let (t, k) = resolve_tombstone_target(
            "acme",
            "agent:bot",
            MemoryType::Event,
            None,
            Some("01HV5..."),
        )
        .unwrap();
        assert_eq!(t, "mem.event.acme");
        assert_eq!(k, "01HV5...");
    }

    #[test]
    fn forget_by_key_on_event_is_invalid() {
        assert!(resolve_tombstone_target(
            "acme",
            "ns",
            MemoryType::Event,
            Some("k"),
            None,
        )
        .is_err());
    }

    #[test]
    fn forget_requires_exactly_one_of_key_or_id() {
        assert!(resolve_tombstone_target("a", "n", MemoryType::Fact, None, None).is_err());
        assert!(resolve_tombstone_target(
            "a",
            "n",
            MemoryType::Fact,
            Some("k"),
            Some("id"),
        )
        .is_err());
    }

    #[test]
    fn forget_rejects_empty_strings() {
        assert!(resolve_tombstone_target("a", "n", MemoryType::Fact, Some(""), None).is_err());
        assert!(resolve_tombstone_target("a", "n", MemoryType::Fact, None, Some("")).is_err());
    }
}
