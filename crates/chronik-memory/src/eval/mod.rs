//! Evaluation harness — fixture types and scoring functions.
//!
//! Provides everything needed to grade extractor + recall output against gold
//! labels:
//! - [`Fixture`] — a labeled conversation: turns + expected extracted memories +
//!   recall queries with expected memory keys.
//! - [`extraction_metrics`] — precision / recall / type-classification accuracy
//!   / cite-source accuracy for a set of extractor outputs vs gold.
//! - [`ndcg_at_k`], [`precision_at_k`], [`mrr`] — recall-quality metrics.
//!
//! All scoring functions are pure — no async, no network. They're used by the
//! integration tests in `tests/` (which drive a live Chronik cluster) but can
//! also be reused from Phase 3 Python/TS bindings.
//!
//! # Sub-modules
//!
//! - [`longmemeval`] (AMS-2.8) — adapter for the LongMemEval benchmark.

pub mod longmemeval;

use crate::extractor::Extracted;
use crate::schema::{Body, MemoryType};
use serde::{Deserialize, Serialize};

// ============================================================================
// Fixture format
// ============================================================================

/// A labeled conversation fixture. Stored as JSON in
/// `crates/chronik-memory/tests/fixtures/agent-memory/<name>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    /// Human-readable identifier — used for test naming and result reporting.
    pub name: String,

    /// Optional one-line description of the scenario.
    #[serde(default)]
    pub description: String,

    /// The raw conversation turns. Indexes are referenced by `source_indexes`
    /// in [`ExpectedMemory`].
    pub conversation: Vec<FixtureTurn>,

    /// Memories that should be extractable from the conversation.
    #[serde(default)]
    pub expected_memories: Vec<ExpectedMemory>,

    /// Queries that should retrieve specific memories.
    #[serde(default)]
    pub recall_queries: Vec<RecallQuery>,

    /// Optional "must NOT extract" assertions — for negative-space tests where
    /// the extractor must refrain from inventing claims (e.g. compliance
    /// refusals, where the agent declined to opine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negative_assertions: Option<NegativeAssertions>,
}

/// Negative-space extraction assertions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NegativeAssertions {
    /// `(subject|predicate)` keys that must not appear in extractor output.
    /// Mirrors the [`Body::Fact`] keying convention `{subject}|{predicate}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_not_extract_predicates: Vec<String>,
    /// Free-text justification for why these are forbidden — surfaces in eval
    /// reports when the assertion fires.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
}

/// Plain conversation turn — minimal, JSON-friendly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureTurn {
    /// Speaker role — `"user"`, `"assistant"`, etc.
    pub role: String,
    /// Message content.
    pub content: String,
    /// RFC-3339 timestamp. Optional in fixtures — defaults to a stable epoch
    /// when not provided so eval is reproducible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
}

/// One expected memory in a fixture.
///
/// Match semantics for grading:
/// - **type** must match exactly.
/// - **key** if set must match the produced memory's key exactly.
/// - **subject_predicate** if set checks `(body.subject, body.predicate)` for facts.
/// - **source_indexes** if set must be a non-empty subset of the produced memory's
///   `source_indexes` (i.e. the extractor cited at least one of the expected sources).
/// - **min_confidence** if set requires the produced memory's confidence to be >= this.
///
/// The grader counts a produced memory as a true positive if it matches at
/// least one expected memory under these rules. Each expected memory can be
/// matched at most once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedMemory {
    /// Memory type (fact / event / instruction / task) — must match exactly.
    #[serde(rename = "type")]
    pub kind: MemoryType,
    /// Optional exact key match for compactable types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Optional `(subject, predicate)` pair to require on facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_predicate: Option<(String, String)>,
    /// Source-turn indexes the produced memory must overlap with (any one suffices).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_indexes: Vec<usize>,
    /// Lower bound on the produced memory's confidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f32>,
}

/// One recall query in a fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallQuery {
    /// The natural-language query passed to `Memory::recall`.
    pub query: String,
    /// Memory keys (compactable types) that should appear in top-k. Order is
    /// the desired ranking — first key = most relevant.
    pub expected_keys: Vec<String>,
    /// k for the ranking metrics. Defaults to 10.
    #[serde(default = "default_k")]
    pub k: usize,
}

fn default_k() -> usize {
    10
}

// ============================================================================
// Extraction metrics
// ============================================================================

/// Result of grading one fixture's extractor output.
#[derive(Debug, Clone, Default)]
pub struct ExtractionMetrics {
    /// Number of expected memories matched by at least one produced memory.
    pub matched: usize,
    /// Total number of expected memories.
    pub expected_total: usize,
    /// Total number of produced memories.
    pub produced_total: usize,
    /// Number of produced memories whose declared type matches at least one
    /// expected memory of the same type (regardless of key match).
    pub type_correct: usize,
    /// Number of produced memories whose `source_indexes` are all in-range
    /// for the input conversation.
    pub cite_in_range: usize,
}

impl ExtractionMetrics {
    /// Precision = matched / produced_total.
    pub fn precision(&self) -> f64 {
        if self.produced_total == 0 {
            return 0.0;
        }
        self.matched as f64 / self.produced_total as f64
    }

    /// Recall = matched / expected_total.
    pub fn recall(&self) -> f64 {
        if self.expected_total == 0 {
            return 0.0;
        }
        self.matched as f64 / self.expected_total as f64
    }

    /// F1 = 2 * P * R / (P + R).
    pub fn f1(&self) -> f64 {
        let p = self.precision();
        let r = self.recall();
        if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        }
    }

    /// Type-classification accuracy = type_correct / produced_total.
    pub fn type_accuracy(&self) -> f64 {
        if self.produced_total == 0 {
            return 0.0;
        }
        self.type_correct as f64 / self.produced_total as f64
    }

    /// Cite-source accuracy = fraction of produced memories whose every cited
    /// source index points at an actual turn in the input batch.
    pub fn cite_accuracy(&self) -> f64 {
        if self.produced_total == 0 {
            return 0.0;
        }
        self.cite_in_range as f64 / self.produced_total as f64
    }
}

/// Check a fixture's negative assertions: returns the list of forbidden
/// predicates that the extractor *did* produce. Empty Vec means no violations.
///
/// Caller is expected to fail the eval if this returns anything non-empty —
/// the extractor invented a claim it was supposed to refuse.
pub fn negative_assertion_violations(
    fixture: &Fixture,
    produced: &[Extracted],
) -> Vec<String> {
    let Some(neg) = &fixture.negative_assertions else {
        return Vec::new();
    };
    let mut violations = Vec::new();
    for ex in produced {
        if let Body::Fact(fb) = &ex.body {
            let key = format!("{}|{}", fb.subject, fb.predicate);
            if neg.must_not_extract_predicates.contains(&key) {
                violations.push(key);
            }
        }
    }
    violations
}

/// Grade extractor output against a fixture's expected memories.
///
/// `produced` is the [`Extracted`] vec returned by the extractor when given
/// `fixture.conversation` as turns. `n_turns` is `fixture.conversation.len()`.
pub fn extraction_metrics(
    fixture: &Fixture,
    produced: &[Extracted],
    n_turns: usize,
) -> ExtractionMetrics {
    let mut m = ExtractionMetrics {
        produced_total: produced.len(),
        expected_total: fixture.expected_memories.len(),
        ..Default::default()
    };
    let mut matched_expected = vec![false; fixture.expected_memories.len()];

    for ex in produced {
        // Cite-source: every index must be in [0, n_turns).
        if !ex.source_indexes.is_empty()
            && ex.source_indexes.iter().all(|&i| i < n_turns)
        {
            m.cite_in_range += 1;
        }

        let produced_kind = ex.body.kind();
        let mut type_seen = false;

        for (i, exp) in fixture.expected_memories.iter().enumerate() {
            if matched_expected[i] {
                continue;
            }
            if exp.kind != produced_kind {
                continue;
            }
            type_seen = true;
            if matches_expected(exp, ex) {
                matched_expected[i] = true;
                m.matched += 1;
                break;
            }
        }
        if type_seen {
            m.type_correct += 1;
        }
    }

    m
}

fn matches_expected(exp: &ExpectedMemory, ex: &Extracted) -> bool {
    if let Some(min_c) = exp.min_confidence {
        if ex.confidence < min_c {
            return false;
        }
    }
    if let Some(k) = &exp.key {
        if ex.key.as_deref() != Some(k.as_str()) {
            return false;
        }
    }
    if let Some((subj, pred)) = &exp.subject_predicate {
        if let Body::Fact(fb) = &ex.body {
            if &fb.subject != subj || &fb.predicate != pred {
                return false;
            }
        } else {
            return false;
        }
    }
    if !exp.source_indexes.is_empty() {
        // Require at least one expected source index to appear in the produced citations.
        let any_overlap = exp
            .source_indexes
            .iter()
            .any(|i| ex.source_indexes.contains(i));
        if !any_overlap {
            return false;
        }
    }
    true
}

// ============================================================================
// Recall ranking metrics
// ============================================================================

/// NDCG@k for a ranked list of result keys.
///
/// Binary relevance: a key is relevant iff it appears in `expected`. The order
/// of `expected` is treated as the ideal ranking — first key has gain 1, decaying
/// according to standard NDCG.
///
/// Returns 0.0 if `expected` is empty (degenerate query).
pub fn ndcg_at_k(ranked: &[String], expected: &[String], k: usize) -> f64 {
    if expected.is_empty() {
        return 0.0;
    }
    let dcg = ranked
        .iter()
        .take(k)
        .enumerate()
        .filter_map(|(i, key)| {
            if expected.contains(key) {
                Some(1.0 / ((i + 2) as f64).log2())
            } else {
                None
            }
        })
        .sum::<f64>();
    let ideal = expected
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, _)| 1.0 / ((i + 2) as f64).log2())
        .sum::<f64>();
    if ideal == 0.0 {
        0.0
    } else {
        dcg / ideal
    }
}

/// Precision@k = (relevant in top-k) / k.
pub fn precision_at_k(ranked: &[String], expected: &[String], k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let hits = ranked
        .iter()
        .take(k)
        .filter(|key| expected.contains(key))
        .count();
    hits as f64 / k as f64
}

/// Mean Reciprocal Rank — for a single query, returns 1/(rank of first relevant).
/// Returns 0.0 if no relevant item is ranked.
pub fn mrr(ranked: &[String], expected: &[String]) -> f64 {
    for (i, key) in ranked.iter().enumerate() {
        if expected.contains(key) {
            return 1.0 / ((i + 1) as f64);
        }
    }
    0.0
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::FactBody;

    fn fact_extracted(subject: &str, predicate: &str, src: Vec<usize>, conf: f32) -> Extracted {
        Extracted {
            body: Body::Fact(FactBody {
                subject: subject.into(),
                predicate: predicate.into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: "t".into(),
                speaker: "user".into(),
            }),
            key: Some(format!("{subject}|{predicate}")),
            confidence: conf,
            source_indexes: src,
        }
    }

    fn fixture_2_facts() -> Fixture {
        Fixture {
            name: "test".into(),
            description: "".into(),
            negative_assertions: None,
            conversation: vec![
                FixtureTurn {
                    role: "user".into(),
                    content: "hi".into(),
                    ts: None,
                },
                FixtureTurn {
                    role: "user".into(),
                    content: "hi".into(),
                    ts: None,
                },
            ],
            expected_memories: vec![
                ExpectedMemory {
                    kind: MemoryType::Fact,
                    key: Some("user|p1".into()),
                    subject_predicate: None,
                    source_indexes: vec![0],
                    min_confidence: Some(0.5),
                },
                ExpectedMemory {
                    kind: MemoryType::Fact,
                    key: Some("user|p2".into()),
                    subject_predicate: None,
                    source_indexes: vec![1],
                    min_confidence: None,
                },
            ],
            recall_queries: vec![],
        }
    }

    #[test]
    fn perfect_extraction_scores_one() {
        let f = fixture_2_facts();
        let produced = vec![
            fact_extracted("user", "p1", vec![0], 0.9),
            fact_extracted("user", "p2", vec![1], 0.9),
        ];
        let m = extraction_metrics(&f, &produced, 2);
        assert_eq!(m.matched, 2);
        assert!((m.precision() - 1.0).abs() < 1e-9);
        assert!((m.recall() - 1.0).abs() < 1e-9);
        assert!((m.f1() - 1.0).abs() < 1e-9);
        assert!((m.type_accuracy() - 1.0).abs() < 1e-9);
        assert!((m.cite_accuracy() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn extra_extraction_lowers_precision() {
        let f = fixture_2_facts();
        let produced = vec![
            fact_extracted("user", "p1", vec![0], 0.9),
            fact_extracted("user", "p2", vec![1], 0.9),
            fact_extracted("user", "garbage", vec![0], 0.9),
        ];
        let m = extraction_metrics(&f, &produced, 2);
        assert_eq!(m.matched, 2);
        assert_eq!(m.produced_total, 3);
        assert!((m.precision() - 2.0 / 3.0).abs() < 1e-9);
        assert!((m.recall() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn missing_extraction_lowers_recall() {
        let f = fixture_2_facts();
        let produced = vec![fact_extracted("user", "p1", vec![0], 0.9)];
        let m = extraction_metrics(&f, &produced, 2);
        assert!((m.precision() - 1.0).abs() < 1e-9);
        assert!((m.recall() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn out_of_range_cite_does_not_count_as_in_range() {
        let f = fixture_2_facts();
        let produced = vec![fact_extracted("user", "p1", vec![5], 0.9)];
        let m = extraction_metrics(&f, &produced, 2);
        assert_eq!(m.cite_in_range, 0);
        assert_eq!(m.matched, 0); // source index doesn't overlap with expected [0]
    }

    #[test]
    fn confidence_below_threshold_is_not_a_match() {
        let f = fixture_2_facts();
        let produced = vec![fact_extracted("user", "p1", vec![0], 0.3)]; // < 0.5
        let m = extraction_metrics(&f, &produced, 2);
        assert_eq!(m.matched, 0);
    }

    #[test]
    fn ndcg_perfect_ranking_is_one() {
        let ranked: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let expected: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        assert!((ndcg_at_k(&ranked, &expected, 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_drops_for_misordered() {
        let ranked: Vec<String> = vec!["c".into(), "b".into(), "a".into()];
        let expected: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        // All relevant hit, just not in ideal order — NDCG@10 is still 1 for binary relevance
        // since DCG = ideal DCG when all expected are present in top-k.
        assert!((ndcg_at_k(&ranked, &expected, 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_zero_when_no_overlap() {
        let ranked: Vec<String> = vec!["x".into(), "y".into()];
        let expected: Vec<String> = vec!["a".into()];
        assert!((ndcg_at_k(&ranked, &expected, 10) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn precision_at_k_basic() {
        let ranked: Vec<String> = vec!["a".into(), "b".into(), "x".into(), "y".into(), "c".into()];
        let expected: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        // top-5: 3 hits / 5 = 0.6
        assert!((precision_at_k(&ranked, &expected, 5) - 0.6).abs() < 1e-9);
        // top-2: 2 hits / 2 = 1.0
        assert!((precision_at_k(&ranked, &expected, 2) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn mrr_first_hit_wins() {
        let ranked: Vec<String> = vec!["x".into(), "a".into(), "b".into()];
        let expected: Vec<String> = vec!["a".into(), "b".into()];
        // 'a' is at rank 2 (1-indexed) → 1/2
        assert!((mrr(&ranked, &expected) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mrr_zero_when_no_hits() {
        let ranked: Vec<String> = vec!["x".into()];
        let expected: Vec<String> = vec!["a".into()];
        assert_eq!(mrr(&ranked, &expected), 0.0);
    }

    #[test]
    fn fixture_round_trips_through_json() {
        let f = fixture_2_facts();
        let json = serde_json::to_string(&f).unwrap();
        let back: Fixture = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, f.name);
        assert_eq!(back.expected_memories.len(), 2);
    }

    #[test]
    fn negative_assertion_catches_forbidden_predicate() {
        use crate::extractor::Extracted;
        let mut f = fixture_2_facts();
        f.negative_assertions = Some(NegativeAssertions {
            must_not_extract_predicates: vec!["user|school_quality_assessment".into()],
            rationale: "agent declined to opine".into(),
        });
        let bad = vec![Extracted {
            body: Body::Fact(crate::schema::FactBody {
                subject: "user".into(),
                predicate: "school_quality_assessment".into(),
                object: serde_json::json!("good"),
                polarity: "asserted".into(),
                text: "user says schools are good".into(),
                speaker: "user".into(),
            }),
            key: Some("user|school_quality_assessment".into()),
            confidence: 0.9,
            source_indexes: vec![0],
        }];
        let v = negative_assertion_violations(&f, &bad);
        assert_eq!(v, vec!["user|school_quality_assessment".to_string()]);
    }

    #[test]
    fn negative_assertion_quiet_when_clean() {
        use crate::extractor::Extracted;
        let mut f = fixture_2_facts();
        f.negative_assertions = Some(NegativeAssertions {
            must_not_extract_predicates: vec!["user|school_quality_assessment".into()],
            rationale: "".into(),
        });
        let ok = vec![Extracted {
            body: Body::Fact(crate::schema::FactBody {
                subject: "user".into(),
                predicate: "preferred_neighborhood".into(),
                object: serde_json::json!("Cascais"),
                polarity: "asserted".into(),
                text: "user prefers Cascais".into(),
                speaker: "user".into(),
            }),
            key: Some("user|preferred_neighborhood".into()),
            confidence: 0.9,
            source_indexes: vec![0],
        }];
        let v = negative_assertion_violations(&f, &ok);
        assert!(v.is_empty());
    }
}
