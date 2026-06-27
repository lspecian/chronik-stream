//! Ranking primitives — pure functions used by [`crate::recall`].
//!
//! - [`Channel`] — discrete retrieval channel (BM25, Vector, etc.).
//! - [`half_life`] — per-memory-type half-life for time-decay scoring.
//! - [`decay_factor`] — query-time decay multiplier.
//! - [`rrf_contribution`] — RRF rank → score formula.
//! - [`dedup_by_key`] — collapse compactable memories on `(namespace, key)`,
//!   keeping the highest version (matches log-compaction supersession).

use crate::schema::{MemoryRecord, MemoryType};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// Standard RRF constant. Higher = more diversity across channels; lower = first-place wins more.
pub const RRF_K: usize = 60;

/// Discrete retrieval channel.
///
/// Phase 1 implements [`Bm25`](Self::Bm25) only. Other variants are reserved for
/// Phase 2 (AMS-2.5) where the SDK gains key-match, HyDE, and SQL channels in
/// addition to the vector channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Tantivy BM25 — full-text via `/_search`.
    Bm25,
    /// HNSW vector — semantic via `/_vector/{topic}/search` (Phase 2).
    Vector,
    /// Exact subject lookup via `/_sql` (Phase 2).
    KeyMatch,
    /// Hypothetical-document embeddings via vector channel (Phase 2).
    Hyde,
    /// Direct SQL filter via `/_sql` (Phase 2).
    Sql,
}

impl Channel {
    /// Default channel weight in the RRF combiner.
    pub fn default_weight(self) -> f64 {
        match self {
            Channel::Bm25 => 1.0,
            Channel::Vector => 1.0,
            Channel::KeyMatch => 2.0,
            Channel::Hyde => 0.5,
            Channel::Sql => 1.5,
        }
    }
}

/// Default per-type weight for type-weighted RRF.
pub fn type_weight(t: MemoryType) -> f64 {
    match t {
        MemoryType::Fact => 1.0,
        MemoryType::Instruction => 1.2,
        MemoryType::Event => 0.8,
        MemoryType::Task => 1.0,
        // Concept pages are synthesized rollups of many atomic memories.
        // Weighted higher than facts so a single concept page surfaces
        // above the individual facts that fed it (the user wanted "tell
        // me about user:luis" → one coherent page, not 50 atomic facts).
        MemoryType::Concept => 1.5,
    }
}

/// Per-type half-life for time-decay scoring.
///
/// Defaults from the SDK roadmap (AMS-1.4):
/// - fact: 365 days
/// - instruction: 730 days (effectively never decays)
/// - event: 30 days
/// - task: 7 days
/// - concept: 90 days (re-synthesised on a regen-cadence trigger; older
///   concept pages are likely stale even if not yet superseded)
pub fn half_life(t: MemoryType) -> Duration {
    match t {
        MemoryType::Fact => Duration::days(365),
        MemoryType::Instruction => Duration::days(730),
        MemoryType::Event => Duration::days(30),
        MemoryType::Task => Duration::days(7),
        MemoryType::Concept => Duration::days(90),
    }
}

/// Time-decay multiplier in (0, 1].
///
/// `score *= exp(-ln(2) * (now - valid_from) / half_life)`.
/// At one half-life elapsed, the multiplier is exactly 0.5. At zero elapsed
/// (or future-dated `valid_from`), it is 1.0.
pub fn decay_factor(
    valid_from: DateTime<Utc>,
    now: DateTime<Utc>,
    half_life: Duration,
) -> f64 {
    let elapsed = now - valid_from;
    if elapsed <= Duration::zero() {
        return 1.0;
    }
    // Use seconds to avoid integer overflow on long horizons.
    let elapsed_s = elapsed.num_seconds() as f64;
    let half_s = half_life.num_seconds().max(1) as f64;
    (-std::f64::consts::LN_2 * elapsed_s / half_s).exp()
}

/// RRF contribution for a single rank in a single channel.
///
/// `1 / (k + rank)`. Rank is 0-indexed (top hit = rank 0).
pub fn rrf_contribution(rank: usize, k: usize) -> f64 {
    1.0 / ((k + rank) as f64)
}

/// Dedup compactable memories on `(namespace, key)`, keeping the highest
/// `version`. Events (no key) pass through unchanged.
///
/// Mirrors what Kafka log compaction would do at rest, but evaluated at query
/// time so transient duplicates during compaction lag don't both leak into
/// recall results.
pub fn dedup_by_key(memories: Vec<MemoryRecord>) -> Vec<MemoryRecord> {
    let mut keyed: HashMap<(String, String), MemoryRecord> = HashMap::new();
    let mut unkeyed: Vec<MemoryRecord> = Vec::new();

    for m in memories {
        match (m.body.kind(), m.key.clone()) {
            (MemoryType::Event, _) | (_, None) => unkeyed.push(m),
            (_, Some(k)) => {
                let dedup_key = (m.namespace.clone(), k);
                match keyed.get(&dedup_key) {
                    Some(existing) if existing.version >= m.version => { /* drop new */ }
                    _ => {
                        keyed.insert(dedup_key, m);
                    }
                }
            }
        }
    }

    let mut out: Vec<MemoryRecord> = keyed.into_values().collect();
    out.append(&mut unkeyed);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, FactBody, Source};

    fn fact(version: u64, key: &str) -> MemoryRecord {
        MemoryRecord {
            memory_id: format!("id-{version}"),
            tenant_id: "t".into(),
            namespace: "t:ns".into(),
            key: Some(key.into()),
            version,
            created_at: Utc::now(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "x".into(),
                offsets: vec![],
                extractor: "x".into(),
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: "s".into(),
                predicate: "p".into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: "t".into(),
            }),
        }
    }

    #[test]
    fn rrf_contrib_decreases_with_rank() {
        assert!(rrf_contribution(0, RRF_K) > rrf_contribution(1, RRF_K));
        assert!(rrf_contribution(0, RRF_K) > rrf_contribution(100, RRF_K));
        // 1 / (60 + 0) = 0.01667
        assert!((rrf_contribution(0, 60) - 1.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn decay_at_zero_elapsed_is_one() {
        let t = Utc::now();
        assert!((decay_factor(t, t, Duration::days(365)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn decay_after_one_half_life_is_half() {
        let now = Utc::now();
        let then = now - Duration::days(30);
        let f = decay_factor(then, now, Duration::days(30));
        assert!((f - 0.5).abs() < 1e-3, "expected ~0.5, got {f}");
    }

    #[test]
    fn decay_handles_future_valid_from() {
        let now = Utc::now();
        let future = now + Duration::days(7);
        assert!((decay_factor(future, now, Duration::days(7)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn dedup_keeps_highest_version_per_key() {
        let a = fact(1, "user:luis|prefers");
        let b = fact(3, "user:luis|prefers");
        let c = fact(2, "user:luis|prefers");
        let out = dedup_by_key(vec![a, b, c]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].version, 3);
    }

    #[test]
    fn dedup_keeps_distinct_keys() {
        let a = fact(1, "k1");
        let b = fact(1, "k2");
        let out = dedup_by_key(vec![a, b]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedup_passes_events_through_unchanged() {
        // Events have no key — every event survives even if same memory_id.
        use crate::schema::EventBody;
        let mk_event = || MemoryRecord {
            memory_id: "id-evt".into(),
            tenant_id: "t".into(),
            namespace: "t:ns".into(),
            key: None,
            version: 1,
            created_at: Utc::now(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "x".into(),
                offsets: vec![],
                extractor: "x".into(),
            },
            tombstoned: false,
            body: Body::Event(EventBody {
                actor: "u".into(),
                verb: "v".into(),
                object: None,
                channel: None,
                context: None,
                ts: Utc::now(),
            }),
        };
        let out = dedup_by_key(vec![mk_event(), mk_event()]);
        assert_eq!(out.len(), 2, "events should pass through");
    }

    #[test]
    fn type_weights_configured() {
        assert_eq!(type_weight(MemoryType::Instruction), 1.2);
        assert_eq!(type_weight(MemoryType::Event), 0.8);
    }

    #[test]
    fn half_life_configured() {
        assert_eq!(half_life(MemoryType::Fact), Duration::days(365));
        assert_eq!(half_life(MemoryType::Event), Duration::days(30));
    }
}
