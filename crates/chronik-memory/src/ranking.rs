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

// ───────────────────────── Question-aware boosts ─────────────────────────
//
// Lever #2 + #3 from the LongMemEval ceiling analysis. The 0.444 ceiling
// across pilots 3-6 had three structural gaps; two of them — multi-fact
// arithmetic ("how many?", "$total"?) and temporal reasoning ("how long?",
// "21 days?") — share the same root cause: value-bearing memories ARE in
// the recall result set but not at the top, so synthesis abstains.
//
// The fix is a query-intent classifier (cheap, rule-based) + a per-memory
// boost multiplier applied after the RRF combine. Pure functions; no LLM
// call, no async, easy to unit-test.

/// What kind of value is the query asking for? Used to boost memories
/// that carry the matching value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryIntent {
    /// Aggregator query — "how many", "total", "sum", "count", "$X".
    Numeric,
    /// Temporal query — "when", "how long", "duration", "before/after X".
    Temporal,
    /// "What did you tell/say/recommend..." — the answer was stated by the
    /// ASSISTANT (WS-2, ROADMAP_MEMORY_QUALITY.md). Boosts facts with
    /// `speaker == "assistant"`.
    AssistantStated,
    /// Default — just look for matching facts.
    Identity,
}

/// Detect query intent from the natural-language query string.
///
/// Cheap rule-based classifier. Conservative — when in doubt, returns
/// `Identity` (no boost).
///
/// Detection order matters: assistant-stated phrasings ("what did you
/// tell me") win first — they are unambiguous and often co-occur with
/// numeric/temporal words that would misroute them. Then strong temporal
/// phrases (e.g. "how long", "for how many days") over numeric
/// aggregators ("how many"), then weak temporal markers, then fall through.
pub fn detect_intent(query: &str) -> QueryIntent {
    let q = query.to_lowercase();

    // Pass 0: assistant-stated phrasings (WS-2). The second-person subject
    // makes these unambiguous — the user is asking what the ASSISTANT said.
    const ASSISTANT_STATED: &[&str] = &[
        "did you tell",
        "did you say",
        "did you recommend",
        "did you suggest",
        "did you mention",
        "did you give",
        "you told me",
        "you said",
        "you recommended",
        "you suggested",
        "you mentioned",
        "you gave me",
        "you provided",
        "you quoted",
        "according to you",
    ];
    for p in ASSISTANT_STATED {
        if q.contains(p) {
            return QueryIntent::AssistantStated;
        }
    }

    // Pass 1: strong temporal phrases that imply a time query even when
    // numeric aggregators are present.
    const STRONG_TEMPORAL: &[&str] = &[
        "when ",
        "when's",
        "how long",
        "how often",
        "for how many days",
        "for how many weeks",
        "for how many months",
        "for how many years",
        "for how many hours",
        "for how many minutes",
        "duration",
    ];
    for p in STRONG_TEMPORAL {
        if q.contains(p) {
            return QueryIntent::Temporal;
        }
    }

    // Pass 2: numeric aggregators. Word-boundary-ish; cheap heuristic.
    const NUMERIC_PHRASES: &[&str] = &[
        "how many",
        "how much",
        "total",
        "sum",
        "count",
        "average",
        "median",
        "minimum",
        "maximum",
        "number of",
        "spend",
        "cost",
        "price",
        "budget",
        "amount",
        "$",
    ];
    for p in NUMERIC_PHRASES {
        if q.contains(p) {
            return QueryIntent::Numeric;
        }
    }

    // Pass 3: weak temporal markers.
    const WEAK_TEMPORAL: &[&str] = &[
        " days",
        " weeks",
        " months",
        " years",
        " hours",
        " minutes",
        "before ",
        "after ",
        "since ",
        "until ",
        "earliest",
        "latest",
        "first time",
        "last time",
    ];
    for p in WEAK_TEMPORAL {
        if q.contains(p) {
            return QueryIntent::Temporal;
        }
    }

    QueryIntent::Identity
}

/// Multiplier in [1.0, 2.0] applied to a memory's RRF-combined score when the
/// query's intent matches a value the memory carries.
///
/// Examples:
/// - Numeric query + Fact with numeric `object` → 1.8 (strong match)
/// - Numeric query + Fact with non-numeric `object` → 1.0 (no boost)
/// - Temporal query + any Event (events always have `ts`) → 1.6
/// - Temporal query + Fact whose text mentions a date → 1.3 (weak match)
/// - Identity query → always 1.0
///
/// Conservative ceiling at 2.0 so the boost can't dominate base ranking
/// signal — it's a tiebreaker for the borderline cases that fall just below
/// the synthesis-context cutoff.
pub fn intent_boost(intent: QueryIntent, record: &MemoryRecord) -> f64 {
    // Sprint 4 tried a speaker-aware de-dilution penalty (0.7× on
    // assistant-speaker facts for non-assistant queries) to recover
    // single-session-user. The full-500 run (s4c) showed it FAILED its
    // hypothesis: ssu dropped further (0.386→0.300) while the headline
    // moved only +2 items (noise). Removed in Sprint 5 — the real recall
    // gap is semantic, not a ranking artifact, so vector search is the
    // lever instead. See ROADMAP_MEMORY_QUALITY.md.
    intent_boost_base(intent, record)
}

/// Intent-type boost. See [`intent_boost`].
fn intent_boost_base(intent: QueryIntent, record: &MemoryRecord) -> f64 {
    use crate::schema::Body;
    match intent {
        QueryIntent::Identity => 1.0,
        QueryIntent::AssistantStated => match &record.body {
            // Strong match: the fact was stated by the assistant (WS-2).
            Body::Fact(f) if f.speaker == "assistant" => 1.8,
            // Weak: agent-actor events sometimes carry assistant statements.
            Body::Event(e) if e.actor.starts_with("agent") => 1.2,
            _ => 1.0,
        },
        QueryIntent::Numeric => match &record.body {
            Body::Fact(f) => {
                // Strong match: object is a JSON number.
                if f.object.is_number() {
                    return 1.8;
                }
                // Weak match: text contains a digit or currency symbol.
                if f.text.chars().any(|c| c.is_ascii_digit())
                    || f.text.contains('$')
                    || f.text.contains('€')
                {
                    return 1.3;
                }
                1.0
            }
            // Tasks may carry `due_at`; not a value-bearing match for
            // numeric aggregation. Events carry counts implicitly (count
            // them at synthesis) so weak boost.
            Body::Event(_) => 1.1,
            _ => 1.0,
        },
        QueryIntent::Temporal => match &record.body {
            // Events always have `ts` — strong temporal match.
            Body::Event(_) => 1.6,
            // Tasks may have due_at.
            Body::Task(t) => {
                if t.due_at.is_some() {
                    1.4
                } else {
                    1.0
                }
            }
            // Facts may mention dates in text.
            Body::Fact(f) => {
                let t = f.text.to_lowercase();
                const DATE_HINTS: &[&str] = &[
                    " day", " week", " month", " year", " hour",
                    "monday", "tuesday", "wednesday", "thursday",
                    "friday", "saturday", "sunday",
                    "january", "february", "march", "april", "may",
                    "june", "july", "august", "september", "october",
                    "november", "december",
                    "yesterday", "today", "tomorrow",
                ];
                if DATE_HINTS.iter().any(|h| t.contains(h)) {
                    1.3
                } else {
                    1.0
                }
            }
            _ => 1.0,
        },
    }
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
                extractor: "x".into(), excerpt: None,
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: "s".into(),
                predicate: "p".into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: "t".into(),
                speaker: "user".into(),
            }),
        }
    }

    // ---------------- Sprint 3 / WS-2: assistant-stated intent ----------------

    #[test]
    fn detects_assistant_stated_phrasings() {
        for q in [
            "What language-learning app did you recommend?",
            "What did you tell me about the hostel in Amsterdam?",
            "What was the quote you gave me from Borges?",
            "You mentioned a meditation website — which one?",
            "you said my jumpsuit had a designation, what was it?",
        ] {
            assert_eq!(
                detect_intent(q),
                QueryIntent::AssistantStated,
                "should be AssistantStated: {q}"
            );
        }
    }

    #[test]
    fn assistant_intent_does_not_fire_on_user_stated() {
        for q in [
            "What degree did I graduate with?",
            "How many days did the fence repair take?",
            "Where did my sister move to?",
            "I told you my budget, what apartments fit it?",
        ] {
            assert_ne!(
                detect_intent(q),
                QueryIntent::AssistantStated,
                "should NOT be AssistantStated: {q}"
            );
        }
    }

    #[test]
    fn assistant_boost_prefers_assistant_speaker_facts() {
        let mut assistant_fact = fact(1, "k");
        if let Body::Fact(f) = &mut assistant_fact.body {
            f.speaker = "assistant".into();
        }
        let user_fact = fact(1, "k2");
        let b_a = intent_boost(QueryIntent::AssistantStated, &assistant_fact);
        let b_u = intent_boost(QueryIntent::AssistantStated, &user_fact);
        assert!(b_a > b_u, "assistant fact must outrank user fact: {b_a} vs {b_u}");
        assert!((b_a - 1.8).abs() < 1e-9);
        assert!((b_u - 1.0).abs() < 1e-9);
    }

    // -------- Sprint 5: de-dilution penalty removed (failed in s4c) --------

    fn assistant_fact() -> MemoryRecord {
        let mut m = fact(1, "ak");
        if let Body::Fact(f) = &mut m.body {
            f.speaker = "assistant".into();
        }
        m
    }

    #[test]
    fn assistant_facts_not_penalized_on_non_assistant_queries() {
        // Sprint 4's 0.7× penalty was removed after s4c showed it hurt
        // single-session-user instead of helping. Non-assistant intents now
        // treat assistant and user facts by intent-type alone (no speaker
        // penalty): both get base 1.0 on Identity/Numeric/Temporal.
        let asst = assistant_fact();
        for intent in [QueryIntent::Identity, QueryIntent::Numeric, QueryIntent::Temporal] {
            let b = intent_boost(intent, &asst);
            assert!((b - 1.0).abs() < 1e-9, "no penalty on {intent:?}: {b}");
        }
    }

    #[test]
    fn assistant_facts_boosted_on_assistant_queries() {
        // The AssistantStated boost stays: when the question IS about what the
        // assistant said, assistant facts still get the 1.8× lift.
        let asst = assistant_fact();
        let b = intent_boost(QueryIntent::AssistantStated, &asst);
        assert!((b - 1.8).abs() < 1e-9, "assistant fact boosted: {b}");
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
                extractor: "x".into(), excerpt: None,
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

    // ─── Question-aware boost (lever #2 + #3) ───

    #[test]
    fn intent_detects_numeric_aggregators() {
        assert_eq!(detect_intent("how many properties has the user seen?"), QueryIntent::Numeric);
        assert_eq!(detect_intent("what is the total spend?"), QueryIntent::Numeric);
        assert_eq!(detect_intent("budget cap?"), QueryIntent::Numeric);
        assert_eq!(detect_intent("count of visits"), QueryIntent::Numeric);
        assert_eq!(detect_intent("Total of $720"), QueryIntent::Numeric);
    }

    #[test]
    fn intent_detects_temporal_markers() {
        assert_eq!(detect_intent("when did the user last visit?"), QueryIntent::Temporal);
        assert_eq!(detect_intent("how long was the trip?"), QueryIntent::Temporal);
        assert_eq!(detect_intent("for how many days were they gone?"), QueryIntent::Temporal);
        assert_eq!(detect_intent("the earliest reply"), QueryIntent::Temporal);
    }

    #[test]
    fn intent_falls_back_to_identity() {
        assert_eq!(detect_intent("what neighbourhood does the user prefer?"), QueryIntent::Identity);
        assert_eq!(detect_intent("user's favourite color"), QueryIntent::Identity);
        assert_eq!(detect_intent("does the user like jazz?"), QueryIntent::Identity);
    }

    #[test]
    fn intent_numeric_prefers_over_temporal() {
        // "$30 a day" → has both '$' (numeric) and ' day' (temporal); numeric
        // wins because we scan numeric first.
        assert_eq!(detect_intent("how many dollars per day?"), QueryIntent::Numeric);
    }

    #[test]
    fn boost_numeric_fact_with_number_strong() {
        let mut r = fact(1, "k");
        if let Body::Fact(ref mut f) = r.body {
            f.object = serde_json::json!(4000);
            f.text = "max budget $4000".into();
        }
        let b = intent_boost(QueryIntent::Numeric, &r);
        assert!((b - 1.8).abs() < 1e-9, "expected 1.8, got {b}");
    }

    #[test]
    fn boost_numeric_fact_text_only_weak() {
        let mut r = fact(1, "k");
        if let Body::Fact(ref mut f) = r.body {
            f.object = serde_json::json!("Williamsburg");
            f.text = "budget around 4000".into(); // digit in text but object is string
        }
        let b = intent_boost(QueryIntent::Numeric, &r);
        assert!((b - 1.3).abs() < 1e-9, "weak match expected, got {b}");
    }

    #[test]
    fn boost_numeric_fact_no_number_unchanged() {
        let mut r = fact(1, "k");
        if let Body::Fact(ref mut f) = r.body {
            f.object = serde_json::json!("Williamsburg");
            f.text = "prefers Williamsburg".into();
        }
        assert_eq!(intent_boost(QueryIntent::Numeric, &r), 1.0);
    }

    #[test]
    fn boost_temporal_event_strong() {
        use crate::schema::EventBody;
        let r = MemoryRecord {
            memory_id: "e".into(),
            tenant_id: "t".into(),
            namespace: "t:ns".into(),
            key: None,
            version: 1,
            created_at: Utc::now(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            source: Source { topic: "x".into(), offsets: vec![], extractor: "x".into(), excerpt: None },
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
        assert_eq!(intent_boost(QueryIntent::Temporal, &r), 1.6);
    }

    #[test]
    fn boost_temporal_fact_with_date_word_weak() {
        let mut r = fact(1, "k");
        if let Body::Fact(ref mut f) = r.body {
            f.text = "visited the Met Museum on Tuesday".into();
        }
        assert_eq!(intent_boost(QueryIntent::Temporal, &r), 1.3);
    }

    #[test]
    fn boost_identity_is_always_one() {
        let r = fact(1, "k");
        assert_eq!(intent_boost(QueryIntent::Identity, &r), 1.0);
    }
}
