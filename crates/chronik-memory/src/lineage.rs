//! AM-3.4 provenance graph primitives.
//!
//! `GET /memory/v1/{memory_id}/lineage` walks a DAG:
//! - **Ancestors** — the raw turns cited by `source.{topic,offsets}`
//!   plus any earlier versions of the same `(namespace, key)`
//!   (compaction losers).
//! - **Descendants** — summaries, concept pages, or later fact
//!   versions that cite this memory as a source.
//!
//! The forward direction (ancestors) is directly readable from the
//! [`MemoryRecord`] envelope. The backward direction (descendants)
//! requires a reverse index — this module provides it.
//!
//! # Scope
//!
//! - **In-memory** — the same shape as [`crate::memory_index`]. Cold
//!   start rehydrates from beginning-of-log; snapshotting is a
//!   follow-up.
//! - **Pure data structures** — no Kafka / HTTP. A consumer wire-up
//!   lives in the server / worker layer.

use crate::schema::MemoryRecord;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// One node in the lineage DAG. Serialized as the HTTP response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct LineageRef {
    /// The referenced memory's id (or synthetic id for raw turns —
    /// `"raw:{topic}:{offset}"`).
    pub memory_id: String,
    /// Kind: `memory` for typed memories, `raw` for raw-turn pointers.
    pub kind: String,
    /// Source topic — always populated. For raw turns this is the
    /// `mem.raw.{tenant}.{...}` topic; for typed memories it's the
    /// source topic recorded in the envelope.
    pub topic: String,
    /// Offsets cited on the source topic. May be empty when the child
    /// records only cite an upstream memory_id, not raw offsets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offsets: Vec<i64>,
}

impl LineageRef {
    /// Synthetic ref pointing at a raw turn.
    pub fn raw(topic: impl Into<String>, offset: i64) -> Self {
        let topic = topic.into();
        Self {
            memory_id: format!("raw:{topic}:{offset}"),
            kind: "raw".into(),
            topic,
            offsets: vec![offset],
        }
    }

    /// Ref pointing at another typed memory.
    pub fn memory(memory_id: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            memory_id: memory_id.into(),
            kind: "memory".into(),
            topic: topic.into(),
            offsets: Vec::new(),
        }
    }
}

/// One record's ancestor + descendant lists. Serialized as the HTTP
/// response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LineageGraph {
    /// The memory this graph is anchored on.
    pub memory_id: String,
    /// Upstream refs — raw turns cited by `source.offsets` plus any
    /// upstream memories (once cross-memory citations land).
    pub ancestors: Vec<LineageRef>,
    /// Downstream refs — later versions of the same `(namespace, key)`,
    /// summaries that cite this memory, concept pages, etc.
    pub descendants: Vec<LineageRef>,
}

/// In-memory reverse-citation index. Cheap to clone (`Arc` interior).
///
/// `(memory_id) → Vec<child_memory_id>` — populated by observing every
/// typed memory as it lands and recording its `source` back-pointers.
#[derive(Debug, Default, Clone)]
pub struct LineageIndex {
    /// Forward: `memory_id → its recorded ancestors`.
    ancestors: Arc<DashMap<String, Vec<LineageRef>>>,
    /// Backward: `memory_id → descendants that cite it`.
    descendants: Arc<DashMap<String, Vec<LineageRef>>>,
}

impl LineageIndex {
    /// Fresh empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of `memory_id` entries with recorded ancestors.
    pub fn nodes(&self) -> usize {
        self.ancestors.len()
    }

    /// Observe a memory record. Populates both directions:
    /// - This record's `memory_id` → its raw-turn ancestors (from
    ///   `source.{topic, offsets}`).
    /// - Reverse: (raw refs → this record) is intentionally NOT
    ///   populated because raw turns aren't lookup targets.
    pub fn observe(&self, record: &MemoryRecord) {
        let ancestors: Vec<LineageRef> = record
            .source
            .offsets
            .iter()
            .map(|off| LineageRef::raw(&record.source.topic, *off))
            .collect();
        self.ancestors.insert(record.memory_id.clone(), ancestors);
    }

    /// Record that `child` cites `parent`. Populates the reverse
    /// index. Called when a summary / concept page / later version is
    /// observed with an upstream memory reference in its provenance.
    pub fn add_citation(
        &self,
        parent_memory_id: &str,
        child: LineageRef,
    ) {
        self.descendants
            .entry(parent_memory_id.to_string())
            .or_default()
            .push(child);
    }

    /// Look up the lineage graph for `memory_id`. Returns `None` when
    /// the memory hasn't been observed yet.
    pub fn get(&self, memory_id: &str) -> Option<LineageGraph> {
        let ancestors = self.ancestors.get(memory_id).map(|v| v.value().clone());
        let descendants = self
            .descendants
            .get(memory_id)
            .map(|v| v.value().clone())
            .unwrap_or_default();
        ancestors.map(|a| LineageGraph {
            memory_id: memory_id.to_string(),
            ancestors: a,
            descendants,
        })
    }

    /// Drop a memory_id's entries entirely (both directions).
    pub fn forget(&self, memory_id: &str) -> bool {
        let a = self.ancestors.remove(memory_id).is_some();
        let d = self.descendants.remove(memory_id).is_some();
        a || d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, FactBody, MemoryRecord, Source};
    use chrono::Utc;

    fn fact(memory_id: &str, source_topic: &str, offsets: Vec<i64>) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: memory_id.into(),
            tenant_id: "t".into(),
            namespace: "ns".into(),
            key: Some("u|p".into()),
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: source_topic.into(),
                offsets,
                extractor: "test@1".into(), excerpt: None,
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: "u".into(),
                predicate: "p".into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: "u p o".into(),
                speaker: "user".into(),
            }),
        }
    }

    #[test]
    fn empty_index_returns_none() {
        let idx = LineageIndex::new();
        assert!(idx.get("m1").is_none());
        assert_eq!(idx.nodes(), 0);
    }

    #[test]
    fn observe_records_ancestors_from_source_offsets() {
        let idx = LineageIndex::new();
        idx.observe(&fact("m1", "mem.raw.t.agent.conv", vec![10, 11]));
        let g = idx.get("m1").unwrap();
        assert_eq!(g.memory_id, "m1");
        assert_eq!(g.ancestors.len(), 2);
        assert_eq!(g.ancestors[0].kind, "raw");
        assert_eq!(g.ancestors[0].offsets, vec![10]);
        assert_eq!(g.ancestors[1].offsets, vec![11]);
        assert!(g.descendants.is_empty());
    }

    #[test]
    fn add_citation_populates_descendants() {
        let idx = LineageIndex::new();
        idx.observe(&fact("m1", "mem.raw.t", vec![10]));
        idx.observe(&fact("m2", "mem.raw.t", vec![20]));
        idx.add_citation("m1", LineageRef::memory("m2", "mem.fact.t"));

        let g = idx.get("m1").unwrap();
        assert_eq!(g.descendants.len(), 1);
        assert_eq!(g.descendants[0].memory_id, "m2");
        assert_eq!(g.descendants[0].kind, "memory");
    }

    #[test]
    fn multiple_citations_accumulate() {
        let idx = LineageIndex::new();
        idx.observe(&fact("m1", "mem.raw.t", vec![10]));
        idx.add_citation("m1", LineageRef::memory("m2", "mem.fact.t"));
        idx.add_citation("m1", LineageRef::memory("m3", "mem.summary.t"));
        let g = idx.get("m1").unwrap();
        assert_eq!(g.descendants.len(), 2);
    }

    #[test]
    fn observing_same_id_replaces_ancestors() {
        let idx = LineageIndex::new();
        idx.observe(&fact("m1", "mem.raw.t", vec![10]));
        idx.observe(&fact("m1", "mem.raw.t", vec![20, 21])); // re-observation, e.g. after replay
        let g = idx.get("m1").unwrap();
        assert_eq!(g.ancestors.len(), 2);
        assert_eq!(g.ancestors[0].offsets, vec![20]);
    }

    #[test]
    fn forget_drops_both_directions() {
        let idx = LineageIndex::new();
        idx.observe(&fact("m1", "mem.raw.t", vec![10]));
        idx.add_citation("m1", LineageRef::memory("m2", "mem.fact.t"));
        assert!(idx.forget("m1"));
        assert!(!idx.forget("m1"), "second forget returns false");
        assert!(idx.get("m1").is_none());
    }

    #[test]
    fn get_of_uncited_memory_returns_ancestor_only() {
        let idx = LineageIndex::new();
        idx.observe(&fact("m1", "mem.raw.t", vec![10]));
        let g = idx.get("m1").unwrap();
        assert_eq!(g.ancestors.len(), 1);
        assert!(g.descendants.is_empty());
    }

    #[test]
    fn lineage_ref_raw_has_synthetic_id() {
        let r = LineageRef::raw("mem.raw.t", 42);
        assert_eq!(r.memory_id, "raw:mem.raw.t:42");
        assert_eq!(r.kind, "raw");
        assert_eq!(r.offsets, vec![42]);
    }

    #[test]
    fn lineage_ref_memory_omits_offsets_from_json() {
        let r = LineageRef::memory("m1", "mem.fact.t");
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"offsets\""));
    }

    #[test]
    fn lineage_graph_json_roundtrip() {
        let g = LineageGraph {
            memory_id: "m1".into(),
            ancestors: vec![LineageRef::raw("mem.raw.t", 10)],
            descendants: vec![LineageRef::memory("m2", "mem.fact.t")],
        };
        let s = serde_json::to_string(&g).unwrap();
        let back: LineageGraph = serde_json::from_str(&s).unwrap();
        assert_eq!(back, g);
    }
}
