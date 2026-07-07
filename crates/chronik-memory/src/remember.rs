//! Direct memory write — bypasses extraction.
//!
//! Use cases:
//! - Agent self-memory (agent writes a fact about itself / the user)
//! - Importing memories from another system
//! - Test fixtures
//!
//! For raw conversation turns, use [`Memory::ingest`](crate::Memory::ingest)
//! instead — that writes to `mem.raw.*` and (in Phase 2) the extraction worker
//! produces typed memories asynchronously.

use crate::error::{MemoryError, Result};
use crate::schema::{Body, MemoryRecord, MemoryType, Source};
use chrono::Utc;
use ulid::Ulid;

/// Build a fully-formed [`MemoryRecord`] envelope for a direct write.
///
/// Used internally by `Memory::remember*` methods. Sets `memory_id` to a fresh
/// ULID, `created_at`/`valid_from` to now, `version` to 1, and `tombstoned`
/// to false. Callers can mutate the result before producing if needed.
pub(crate) fn build_envelope(
    tenant_id: &str,
    namespace: &str,
    body: Body,
    key: Option<String>,
    confidence: f32,
    extractor_id: &str,
    source_topic: &str,
) -> Result<MemoryRecord> {
    validate_key_for_kind(&body, key.as_deref())?;
    if !(0.0..=1.0).contains(&confidence) {
        return Err(MemoryError::InvalidArgument(format!(
            "confidence {} not in [0.0, 1.0]",
            confidence
        )));
    }
    let now = Utc::now();
    Ok(MemoryRecord {
        memory_id: Ulid::new().to_string(),
        tenant_id: tenant_id.to_string(),
        namespace: namespace.to_string(),
        key,
        version: 1,
        created_at: now,
        valid_from: now,
        valid_to: None,
        confidence,
        source: Source {
            topic: source_topic.to_string(),
            offsets: vec![],
            extractor: extractor_id.to_string(), excerpt: None,
        },
        tombstoned: false,
        body,
    })
}

/// Compactable memory types **must** carry a non-empty key. Events must not.
fn validate_key_for_kind(body: &Body, key: Option<&str>) -> Result<()> {
    let kind = body.kind();
    let needs_key = matches!(
        kind,
        MemoryType::Fact | MemoryType::Instruction | MemoryType::Task
    );
    match (needs_key, key) {
        (true, None) => Err(MemoryError::InvalidArgument(format!(
            "memory type {:?} requires a non-empty key",
            kind
        ))),
        (true, Some(s)) if s.is_empty() => Err(MemoryError::InvalidArgument(format!(
            "memory type {:?} requires a non-empty key",
            kind
        ))),
        (false, Some(_)) => {
            // Permissive: events with an explicit key are allowed (caller knows
            // what they're doing — e.g. for cross-event correlation).
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Compute the Kafka record key for a typed memory.
///
/// For compacted topics (fact / instruction / task), the key is `{namespace}|{key}`
/// so log compaction handles supersession correctly. For events, the key is the
/// `memory_id` (the ULID) — append-only with no compaction.
pub(crate) fn record_key(memory: &MemoryRecord) -> String {
    match memory.body.kind() {
        MemoryType::Fact
        | MemoryType::Instruction
        | MemoryType::Task
        | MemoryType::Concept => {
            // Concept pages are also compacted — the key is `{namespace}|concept:{entity_id}`,
            // and the entity_id is stored in the envelope's `key` field
            // by the (still-pending) concept worker.
            let k = memory.key.as_deref().unwrap_or("");
            format!("{}|{}", memory.namespace, k)
        }
        MemoryType::Event => memory.memory_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EventBody, FactBody, InstructionBody, TaskBody, TaskState};

    fn fact() -> Body {
        Body::Fact(FactBody {
            subject: "user:luis".into(),
            predicate: "prefers_neighborhood".into(),
            object: serde_json::json!("Lapa"),
            polarity: "asserted".into(),
            text: "Luis prefers Lapa".into(),
                speaker: "user".into(),
        })
    }

    fn event() -> Body {
        Body::Event(EventBody {
            actor: "user:luis".into(),
            verb: "viewed_property".into(),
            object: Some("property:LX-4471".into()),
            channel: None,
            context: None,
            ts: Utc::now(),
        })
    }

    #[test]
    fn build_fact_envelope() {
        let env = build_envelope(
            "acme",
            "agent:bot:user:luis",
            fact(),
            Some("user:luis|prefers_neighborhood".into()),
            0.92,
            "x@1",
            "mem.raw.acme",
        )
        .unwrap();
        assert_eq!(env.tenant_id, "acme");
        assert_eq!(env.version, 1);
        assert!(!env.tombstoned);
        assert!(env.memory_id.starts_with('0') || env.memory_id.starts_with('1'));
        // ULIDs are 26 chars
        assert_eq!(env.memory_id.len(), 26);
    }

    #[test]
    fn fact_requires_key() {
        let err = build_envelope("acme", "ns", fact(), None, 0.5, "x@1", "mem.raw.acme")
            .unwrap_err();
        match err {
            MemoryError::InvalidArgument(_) => {}
            _ => panic!("wrong error: {:?}", err),
        }
    }

    #[test]
    fn fact_rejects_empty_key() {
        let err = build_envelope(
            "acme",
            "ns",
            fact(),
            Some(String::new()),
            0.5,
            "x@1",
            "mem.raw.acme",
        )
        .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidArgument(_)));
    }

    #[test]
    fn event_does_not_require_key() {
        let env = build_envelope("acme", "ns", event(), None, 0.5, "x@1", "mem.raw.acme")
            .unwrap();
        assert!(env.key.is_none());
    }

    #[test]
    fn confidence_must_be_in_range() {
        assert!(build_envelope("a", "n", fact(), Some("k".into()), -0.1, "x", "t").is_err());
        assert!(build_envelope("a", "n", fact(), Some("k".into()), 1.1, "x", "t").is_err());
        assert!(build_envelope("a", "n", fact(), Some("k".into()), 0.0, "x", "t").is_ok());
        assert!(build_envelope("a", "n", fact(), Some("k".into()), 1.0, "x", "t").is_ok());
    }

    #[test]
    fn record_key_for_compacted_uses_namespace_and_key() {
        let env = build_envelope(
            "acme",
            "agent:bot",
            fact(),
            Some("user:luis|prefers".into()),
            0.5,
            "x@1",
            "mem.raw.acme",
        )
        .unwrap();
        assert_eq!(record_key(&env), "agent:bot|user:luis|prefers");
    }

    #[test]
    fn record_key_for_event_uses_memory_id() {
        let env = build_envelope("acme", "ns", event(), None, 0.5, "x@1", "mem.raw.acme")
            .unwrap();
        assert_eq!(record_key(&env), env.memory_id);
    }

    #[test]
    fn instruction_and_task_require_key() {
        let inst = Body::Instruction(InstructionBody {
            scope: "agent:bot".into(),
            rule: "Be brief".into(),
            trigger: "before_reply".into(),
            priority: 0,
        });
        assert!(build_envelope("a", "n", inst, None, 0.5, "x", "t").is_err());

        let task = Body::Task(TaskBody {
            task_id: "tsk_1".into(),
            title: "schedule".into(),
            state: TaskState::Open,
            due_at: None,
            owner: None,
            depends_on: vec![],
        });
        assert!(build_envelope("a", "n", task, None, 0.5, "x", "t").is_err());
    }
}
