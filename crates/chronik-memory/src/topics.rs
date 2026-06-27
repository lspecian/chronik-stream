//! Topic naming conventions and per-topic configuration templates.
//!
//! # Layout per tenant
//!
//! ```text
//! mem.raw.{tenant}.{agent}.{conversation}    # raw conversation turns (per-conversation, append-only)
//! mem.fact.{tenant}                           # extracted facts (compacted, key=namespace|subject|predicate)
//! mem.event.{tenant}                          # extracted events (append-only, key=memory_id)
//! mem.instruction.{tenant}                    # agent rules (compacted)
//! mem.task.{tenant}                           # task transitions (event-sourced)
//! mem.task.current.{tenant}                   # latest task state per task_id (compacted view, materialized)
//! ```
//!
//! Phase 1 only needs `mem.raw.*`, `mem.fact.{tenant}`, and `mem.event.{tenant}`.
//! Instruction, task, and the compacted task view come in Phase 2 (AMS-2.1).

use crate::error::{MemoryError, Result};
use crate::schema::MemoryType;

/// Parsed namespace path: `agent:{agent}:user:{user}` (or any colon-segmented form).
///
/// The SDK doesn't require a specific namespace shape — only that it's non-empty
/// and doesn't contain Kafka-illegal characters. The convention is documented but
/// not enforced beyond basic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespacePath {
    /// Tenant identifier — first segment. Drives the topic prefix.
    pub tenant: String,
    /// Full namespace path (including tenant).
    pub full: String,
}

impl NamespacePath {
    /// Parse a namespace string. Format: `{tenant}:{...}` or just `{tenant}`.
    ///
    /// Validation: non-empty, no `/` `\n` or `\0`, no whitespace.
    pub fn parse(ns: impl Into<String>) -> Result<Self> {
        let full = ns.into();
        if full.is_empty() {
            return Err(MemoryError::InvalidArgument(
                "namespace must not be empty".into(),
            ));
        }
        if full
            .chars()
            .any(|c| c == '/' || c == '\n' || c == '\0' || c.is_whitespace())
        {
            return Err(MemoryError::InvalidArgument(format!(
                "namespace contains illegal characters: {:?}",
                full
            )));
        }
        let tenant = full.split(':').next().unwrap_or("").to_string();
        if tenant.is_empty() {
            return Err(MemoryError::InvalidArgument(
                "namespace must start with a tenant segment".into(),
            ));
        }
        Ok(NamespacePath { tenant, full })
    }
}

/// Topic-name builder for a given namespace.
#[derive(Debug, Clone)]
pub struct TopicLayout {
    namespace: NamespacePath,
}

impl TopicLayout {
    /// Build the layout for a namespace.
    pub fn new(namespace: NamespacePath) -> Self {
        Self { namespace }
    }

    /// Tenant identifier.
    pub fn tenant(&self) -> &str {
        &self.namespace.tenant
    }

    /// Raw conversation topic for an arbitrary conversation slug.
    ///
    /// Convention: `mem.raw.{tenant}.{agent}.{conversation}` — but the SDK
    /// only assembles `mem.raw.{tenant}.{rest}` where `rest` is the namespace
    /// after the tenant prefix, with `:` replaced by `.`. Phase 1 stores all
    /// turns for a namespace in one topic.
    pub fn raw(&self) -> String {
        // Strip the leading `tenant:` from the namespace, replace `:` with `.`.
        let rest = self
            .namespace
            .full
            .strip_prefix(&format!("{}:", self.namespace.tenant))
            .unwrap_or("");
        if rest.is_empty() {
            format!("mem.raw.{}", self.namespace.tenant)
        } else {
            let suffix = rest.replace(':', ".");
            format!("mem.raw.{}.{}", self.namespace.tenant, suffix)
        }
    }

    /// Typed memory topic for a given memory type.
    ///
    /// `mem.fact.{tenant}`, `mem.event.{tenant}`, `mem.instruction.{tenant}`, `mem.task.{tenant}`.
    pub fn typed(&self, kind: MemoryType) -> String {
        format!("mem.{}.{}", kind.as_str(), self.namespace.tenant)
    }

    /// Compacted current-state view topic for tasks.
    pub fn task_current(&self) -> String {
        format!("mem.task.current.{}", self.namespace.tenant)
    }

    /// Compacted concept-page topic — `mem.concept.{tenant}` (AMS-3.7).
    /// Latest synthesis per `concept:{entity_id}` key wins via Kafka log
    /// compaction. The SDK exposes this as a separate accessor (rather
    /// than via `typed(MemoryType::Concept)`) because the compaction key
    /// shape is different — concept keys carry an `entity_id`, not a
    /// memory_id.
    pub fn concept(&self) -> String {
        format!("mem.concept.{}", self.namespace.tenant)
    }

    /// All topics this namespace uses (Phase 1 only creates raw + fact + event;
    /// Phase 2 adds instruction + task; Phase 3 adds concept).
    pub fn all_topics(&self) -> Vec<String> {
        vec![
            self.raw(),
            self.typed(MemoryType::Fact),
            self.typed(MemoryType::Event),
            self.typed(MemoryType::Instruction),
            self.typed(MemoryType::Task),
            self.task_current(),
            self.concept(),
        ]
    }
}

/// Per-topic config template — values to set when creating the topic.
///
/// Only the config keys Chronik's `topic_validator` recognises are sent:
/// - `cleanup.policy` — `delete` or `compact`
/// - `vector.enabled` — HNSW vector index
/// - `columnar.enabled` — Parquet/DataFusion SQL table
///
/// **Search (Tantivy BM25) is not a per-topic key in Chronik today** — it is
/// controlled globally by the `CHRONIK_DEFAULT_SEARCHABLE` env var on the
/// broker. The SDK still tracks an intent flag (`bm25_enabled`) on the
/// template for documentation and future use; it is not currently sent in
/// the CreateTopics request.
#[derive(Debug, Clone)]
pub struct TopicConfig {
    /// Topic name.
    pub name: String,
    /// Cleanup policy: `"delete"` (append-only) or `"compact"` (key-based supersession).
    pub cleanup_policy: &'static str,
    /// Documented intent — full-text BM25 index. Currently set globally on the
    /// broker via `CHRONIK_DEFAULT_SEARCHABLE`; not sent as a per-topic key.
    pub bm25_enabled: bool,
    /// Enable HNSW vector index (per-topic key `vector.enabled`).
    pub vector_enabled: bool,
    /// Enable Parquet columnar storage / DataFusion SQL (per-topic key `columnar.enabled`).
    pub columnar_enabled: bool,
}

impl TopicConfig {
    /// Config template for `mem.raw.*` — append-only, columnar for analytics, no
    /// vector or text indexing on raw turns (those are extracted into typed topics).
    pub fn raw(name: String) -> Self {
        Self {
            name,
            cleanup_policy: "delete",
            bm25_enabled: false,
            vector_enabled: false,
            columnar_enabled: true,
        }
    }

    /// Config template for `mem.fact.*` — compacted, all three indexes on (text + vector + SQL).
    pub fn fact(name: String) -> Self {
        Self {
            name,
            cleanup_policy: "compact",
            bm25_enabled: true,
            vector_enabled: true,
            columnar_enabled: true,
        }
    }

    /// Config template for `mem.event.*` — append-only, all three indexes on.
    pub fn event(name: String) -> Self {
        Self {
            name,
            cleanup_policy: "delete",
            bm25_enabled: true,
            vector_enabled: true,
            columnar_enabled: true,
        }
    }

    /// Config template for `mem.instruction.*` — compacted, text + vector + SQL.
    pub fn instruction(name: String) -> Self {
        Self {
            name,
            cleanup_policy: "compact",
            bm25_enabled: true,
            vector_enabled: true,
            columnar_enabled: true,
        }
    }

    /// Config template for `mem.task.*` — **compacted current-state** (key=task_id).
    ///
    /// Phase 2 simplification: one compacted topic that holds the latest state per
    /// task_id, rather than a separate event-sourced log + compacted view. Each
    /// `update_task` call produces a new record with the same `task_id` key, and
    /// log compaction keeps only the latest. If/when audit-trail history is
    /// needed, a sibling `mem.task.audit.{tenant}` log topic can be added in
    /// Phase 3 — without changing the SDK API.
    pub fn task(name: String) -> Self {
        Self {
            name,
            cleanup_policy: "compact",
            bm25_enabled: true,
            vector_enabled: false,
            columnar_enabled: true,
        }
    }

    /// **Deprecated in Phase 2.** Was the compacted view that materialized from
    /// an event-sourced `mem.task.*` log. Now `mem.task.*` itself is the
    /// compacted current-state topic; this template is kept only so older
    /// deployments parsing the layout don't break.
    #[deprecated(note = "Phase 2 makes `mem.task.*` compacted directly; this is now the same.")]
    pub fn task_current(name: String) -> Self {
        Self::task(name)
    }

    /// Config template for `mem.concept.*` (AMS-3.7) — compacted by
    /// `concept:{entity_id}`, full text + vector index so concept pages are
    /// retrievable by both keyword and semantic similarity.
    pub fn concept(name: String) -> Self {
        Self {
            name,
            cleanup_policy: "compact",
            bm25_enabled: true,
            vector_enabled: true,
            columnar_enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_namespace() {
        let ns = NamespacePath::parse("acme:agent:bot:user:luis").unwrap();
        assert_eq!(ns.tenant, "acme");
        assert_eq!(ns.full, "acme:agent:bot:user:luis");
    }

    #[test]
    fn rejects_empty_or_invalid_namespace() {
        assert!(NamespacePath::parse("").is_err());
        assert!(NamespacePath::parse("with space").is_err());
        assert!(NamespacePath::parse("with/slash").is_err());
        assert!(NamespacePath::parse("with\nnewline").is_err());
    }

    #[test]
    fn topic_layout_builds_expected_names() {
        let ns = NamespacePath::parse("acme:agent:bot:user:luis").unwrap();
        let layout = TopicLayout::new(ns);
        assert_eq!(layout.tenant(), "acme");
        assert_eq!(layout.raw(), "mem.raw.acme.agent.bot.user.luis");
        assert_eq!(layout.typed(MemoryType::Fact), "mem.fact.acme");
        assert_eq!(layout.typed(MemoryType::Event), "mem.event.acme");
        assert_eq!(layout.task_current(), "mem.task.current.acme");
        assert_eq!(layout.concept(), "mem.concept.acme");
        // Was 6 (raw + 4 typed + task_current); now 7 with concept (AMS-3.7).
        assert_eq!(layout.all_topics().len(), 7);
    }

    #[test]
    fn raw_topic_for_tenant_only_namespace() {
        let ns = NamespacePath::parse("acme").unwrap();
        let layout = TopicLayout::new(ns);
        assert_eq!(layout.raw(), "mem.raw.acme");
    }

    #[test]
    fn fact_config_is_compacted_with_all_indexes() {
        let cfg = TopicConfig::fact("mem.fact.acme".into());
        assert_eq!(cfg.cleanup_policy, "compact");
        assert!(cfg.bm25_enabled);
        assert!(cfg.vector_enabled);
        assert!(cfg.columnar_enabled);
    }

    #[test]
    fn event_config_is_append_only() {
        let cfg = TopicConfig::event("mem.event.acme".into());
        assert_eq!(cfg.cleanup_policy, "delete");
    }
}
