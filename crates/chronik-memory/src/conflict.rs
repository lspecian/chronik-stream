//! AM-3.2 conflict detection primitives.
//!
//! Two facts on the same `(namespace, subject, predicate)` key are in
//! conflict when their `object` values disagree and neither is a
//! null/erase marker. Where compaction-based supersession
//! ([`crate::recall::dedup_results_keep_max_score`]) already picks the
//! latest version, conflict detection flags the disagreement so
//! callers can:
//!
//! - Mark the newer fact with `polarity="conflicting"` on emit
//!   (extractor-side) — surfaces the tension in provenance without
//!   dropping either claim.
//! - Down-score both facts at recall time by [`CONFLICT_PENALTY`]
//!   until an operator resolves the conflict via
//!   `POST /memory/v1/{id}/resolve` (endpoint follow-up).
//!
//! This module is pure — no Kafka, no LLM, no HTTP. Wire-up to the
//! extractor and recall paths happens in the caller.

use crate::schema::{Body, MemoryRecord};
use serde::{Deserialize, Serialize};

/// Multiplicative penalty applied to a memory's recall score when it
/// is in an unresolved conflict. Default `0.5` per the roadmap
/// (`AM-3.2` — "Recall surfaces conflicts: `score *= conflict_penalty`,
/// default 0.5, until resolved").
pub const CONFLICT_PENALTY: f64 = 0.5;

/// Polarity marker written into the [`crate::schema::FactBody::polarity`]
/// field when a conflict is detected. Distinct from the default
/// `"asserted"` so downstream filters can pick it up without keeping
/// a side channel.
pub const POLARITY_CONFLICTING: &str = "conflicting";

/// Per-tenant conflict-resolution strategy. Configured via
/// `mem.config.{tenant}` (`conflict.strategy` key). Callers not
/// reading config default to [`Self::SurfaceBoth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStrategy {
    /// Return both memories with their scores penalized. Default —
    /// forces the caller to make the call.
    SurfaceBoth,
    /// Keep the record with the higher `version` (compaction semantics
    /// already do this at storage; this option makes it explicit at
    /// recall time so the loser is dropped).
    LatestWins,
    /// Keep the record with the higher `confidence`.
    HighestConfidenceWins,
}

impl Default for ConflictStrategy {
    fn default() -> Self {
        Self::SurfaceBoth
    }
}

impl ConflictStrategy {
    /// Parse from a string tag as written in `mem.config.{tenant}`.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "surface_both" => Some(Self::SurfaceBoth),
            "latest_wins" => Some(Self::LatestWins),
            "highest_confidence_wins" => Some(Self::HighestConfidenceWins),
            _ => None,
        }
    }

    /// Wire tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SurfaceBoth => "surface_both",
            Self::LatestWins => "latest_wins",
            Self::HighestConfidenceWins => "highest_confidence_wins",
        }
    }
}

/// Outcome of [`detect_conflict`].
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictDecision {
    /// Objects match — no conflict.
    NoConflict,
    /// Objects disagree — the caller should mark the new fact as
    /// conflicting and surface both memories at recall time (or apply
    /// the tenant's [`ConflictStrategy`]).
    Conflict {
        /// `memory_id` of the existing (older) fact.
        existing_id: String,
        /// Object rendered as a JSON string, from the existing fact.
        existing_object: String,
        /// Object rendered as a JSON string, from the new fact.
        new_object: String,
    },
}

/// Compare a new fact against an existing fact on the same
/// `(namespace, subject, predicate)` key. Returns [`ConflictDecision::NoConflict`]
/// when either record is not a Fact, when the objects match (JSON
/// equality after `serde_json::Value::to_string`), or when either
/// object is a null / empty marker.
///
/// Pure — no side effects.
pub fn detect_conflict(existing: &MemoryRecord, new: &MemoryRecord) -> ConflictDecision {
    let (Body::Fact(ef), Body::Fact(nf)) = (&existing.body, &new.body) else {
        return ConflictDecision::NoConflict;
    };
    if ef.subject != nf.subject || ef.predicate != nf.predicate {
        return ConflictDecision::NoConflict;
    }
    if ef.object.is_null() || nf.object.is_null() {
        return ConflictDecision::NoConflict;
    }
    if is_empty_object_value(&ef.object) || is_empty_object_value(&nf.object) {
        return ConflictDecision::NoConflict;
    }
    if ef.object == nf.object {
        return ConflictDecision::NoConflict;
    }
    ConflictDecision::Conflict {
        existing_id: existing.memory_id.clone(),
        existing_object: ef.object.to_string(),
        new_object: nf.object.to_string(),
    }
}

fn is_empty_object_value(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => s.is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Apply [`CONFLICT_PENALTY`] to a `f64` recall score. Used by the
/// recall layer once conflict-flagging is wired.
pub fn apply_conflict_penalty(score: f64) -> f64 {
    score * CONFLICT_PENALTY
}

/// Given a set of results on the same `(namespace, subject, predicate)`
/// key and a strategy, decide which memories survive. Used by recall
/// as an alternative to [`crate::recall::dedup_results_keep_max_score`]
/// when conflict-aware ranking is enabled for the tenant.
///
/// - [`ConflictStrategy::SurfaceBoth`] — every record survives.
/// - [`ConflictStrategy::LatestWins`] — highest `version` survives.
/// - [`ConflictStrategy::HighestConfidenceWins`] — highest
///   `confidence` survives (ties broken by `version`).
pub fn resolve_conflict(
    records: &[MemoryRecord],
    strategy: ConflictStrategy,
) -> Vec<MemoryRecord> {
    if records.is_empty() {
        return Vec::new();
    }
    match strategy {
        ConflictStrategy::SurfaceBoth => records.to_vec(),
        ConflictStrategy::LatestWins => {
            let winner = records
                .iter()
                .max_by(|a, b| {
                    a.version
                        .cmp(&b.version)
                        .then(a.confidence.partial_cmp(&b.confidence).unwrap_or(std::cmp::Ordering::Equal))
                })
                .expect("non-empty checked above");
            vec![winner.clone()]
        }
        ConflictStrategy::HighestConfidenceWins => {
            let winner = records
                .iter()
                .max_by(|a, b| {
                    a.confidence
                        .partial_cmp(&b.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.version.cmp(&b.version))
                })
                .expect("non-empty checked above");
            vec![winner.clone()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, EventBody, FactBody, MemoryRecord, Source};
    use chrono::Utc;

    fn fact_record(id: &str, obj: serde_json::Value, version: u64, confidence: f32) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: id.into(),
            tenant_id: "t".into(),
            namespace: "ns".into(),
            key: Some("u|budget".into()),
            version,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence,
            source: Source {
                topic: "mem.raw.t".into(),
                offsets: vec![0],
                extractor: "test@1".into(), excerpt: None,
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: "u".into(),
                predicate: "budget".into(),
                object: obj,
                polarity: "asserted".into(),
                text: "u budget".into(),
                speaker: "user".into(),
            }),
        }
    }

    fn event_record(id: &str) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: id.into(),
            tenant_id: "t".into(),
            namespace: "ns".into(),
            key: None,
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "mem.event.t".into(),
                offsets: vec![0],
                extractor: "test@1".into(), excerpt: None,
            },
            tombstoned: false,
            body: Body::Event(EventBody {
                actor: "u".into(),
                verb: "v".into(),
                object: None,
                channel: None,
                context: None,
                ts: now,
            }),
        }
    }

    #[test]
    fn same_object_is_no_conflict() {
        let a = fact_record("a", serde_json::json!(750_000), 1, 0.9);
        let b = fact_record("b", serde_json::json!(750_000), 2, 0.9);
        assert_eq!(detect_conflict(&a, &b), ConflictDecision::NoConflict);
    }

    #[test]
    fn distinct_objects_are_conflict() {
        let a = fact_record("a", serde_json::json!(750_000), 1, 0.9);
        let b = fact_record("b", serde_json::json!(820_000), 2, 0.9);
        let d = detect_conflict(&a, &b);
        match d {
            ConflictDecision::Conflict { existing_id, .. } => {
                assert_eq!(existing_id, "a");
            }
            _ => panic!("expected Conflict"),
        }
    }

    #[test]
    fn null_object_is_not_a_conflict() {
        let a = fact_record("a", serde_json::json!(null), 1, 0.9);
        let b = fact_record("b", serde_json::json!(820_000), 2, 0.9);
        assert_eq!(detect_conflict(&a, &b), ConflictDecision::NoConflict);
    }

    #[test]
    fn empty_string_object_is_not_a_conflict() {
        let a = fact_record("a", serde_json::json!(""), 1, 0.9);
        let b = fact_record("b", serde_json::json!("Lapa"), 2, 0.9);
        assert_eq!(detect_conflict(&a, &b), ConflictDecision::NoConflict);
    }

    #[test]
    fn non_fact_bodies_return_no_conflict() {
        let ev = event_record("e");
        let f = fact_record("f", serde_json::json!(750_000), 1, 0.9);
        assert_eq!(detect_conflict(&ev, &f), ConflictDecision::NoConflict);
    }

    #[test]
    fn different_subjects_or_predicates_are_not_conflicts() {
        let mut a = fact_record("a", serde_json::json!(750_000), 1, 0.9);
        let mut b = fact_record("b", serde_json::json!(820_000), 2, 0.9);
        if let Body::Fact(nb) = &mut b.body {
            nb.subject = "user2".into();
        }
        assert_eq!(detect_conflict(&a, &b), ConflictDecision::NoConflict);
        if let Body::Fact(nb) = &mut b.body {
            nb.subject = "u".into();
            nb.predicate = "other".into();
        }
        // Same subject, different predicate — not a conflict.
        assert_eq!(detect_conflict(&a, &b), ConflictDecision::NoConflict);
        // Sanity: restore predicate → conflict is back.
        if let Body::Fact(nb) = &mut b.body {
            nb.predicate = "budget".into();
        }
        if let Body::Fact(na) = &mut a.body {
            na.object = serde_json::json!(750_000);
        }
        assert!(matches!(
            detect_conflict(&a, &b),
            ConflictDecision::Conflict { .. }
        ));
    }

    #[test]
    fn apply_penalty_multiplies_by_half() {
        assert!((apply_conflict_penalty(0.8) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn resolve_surface_both_keeps_all() {
        let recs = vec![
            fact_record("a", serde_json::json!(1), 1, 0.7),
            fact_record("b", serde_json::json!(2), 2, 0.9),
        ];
        let out = resolve_conflict(&recs, ConflictStrategy::SurfaceBoth);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn resolve_latest_wins_keeps_highest_version() {
        let recs = vec![
            fact_record("a", serde_json::json!(1), 1, 0.9),
            fact_record("b", serde_json::json!(2), 3, 0.7),
            fact_record("c", serde_json::json!(3), 2, 0.8),
        ];
        let out = resolve_conflict(&recs, ConflictStrategy::LatestWins);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].memory_id, "b");
    }

    #[test]
    fn resolve_highest_confidence_wins_ties_broken_by_version() {
        let recs = vec![
            fact_record("a", serde_json::json!(1), 1, 0.9),
            fact_record("b", serde_json::json!(2), 3, 0.9),
            fact_record("c", serde_json::json!(3), 2, 0.8),
        ];
        let out = resolve_conflict(&recs, ConflictStrategy::HighestConfidenceWins);
        assert_eq!(out.len(), 1);
        // Both a and b have 0.9 confidence — highest version wins the tie.
        assert_eq!(out[0].memory_id, "b");
    }

    #[test]
    fn resolve_empty_input_returns_empty() {
        let out = resolve_conflict(&[], ConflictStrategy::LatestWins);
        assert!(out.is_empty());
    }

    #[test]
    fn strategy_string_roundtrip() {
        for s in ["surface_both", "latest_wins", "highest_confidence_wins"] {
            let parsed = ConflictStrategy::from_str(s).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!(ConflictStrategy::from_str("nonsense").is_none());
    }

    #[test]
    fn strategy_default_is_surface_both() {
        assert_eq!(ConflictStrategy::default(), ConflictStrategy::SurfaceBoth);
    }

    #[test]
    fn conflict_penalty_constant_is_half() {
        assert!((CONFLICT_PENALTY - 0.5).abs() < 1e-9);
    }

    #[test]
    fn polarity_conflicting_matches_wire_convention() {
        assert_eq!(POLARITY_CONFLICTING, "conflicting");
    }
}
