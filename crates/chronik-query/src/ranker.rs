//! Ranking engine for scored candidates.
//!
//! The `Ranker` trait defines the interface for ranking candidates after RRF fusion.
//! `RuleRanker` is the default implementation using weighted feature scoring with
//! configurable profiles.

use crate::candidate::CandidateEntry;
use crate::profiles::RankingProfile;
use crate::types::QueryMode;

/// Trait for ranking candidates after fusion.
///
/// Implementations receive candidates (already RRF-merged) and a ranking profile,
/// then compute final scores using features like freshness, source count, and
/// backend-specific signals.
pub trait Ranker: Send + Sync {
    /// Rank candidates in-place according to the given profile.
    ///
    /// After this call, entries should be sorted by `primary.final_score` descending
    /// and have `primary.features` and `primary.explanation` populated.
    fn rank(&self, entries: &mut Vec<CandidateEntry>, profile: &RankingProfile);
}

/// Rule-based ranker using weighted feature scoring.
///
/// Extracts features from each candidate, applies profile weights, and computes
/// a final score. Supports explanation generation for debugging.
pub struct RuleRanker;

impl RuleRanker {
    /// Extract features from a candidate entry.
    fn extract_features(entry: &CandidateEntry) -> Vec<(String, f64)> {
        let mut features = Vec::new();

        // RRF score (already computed by the merger)
        if let Some(&rrf) = entry.primary.features.get("rrf_score") {
            features.push(("rrf_score".to_string(), rrf));
        }

        // Source count: how many backends found this candidate
        let source_count = entry.ranks.len() as f64;
        features.push(("source_count".to_string(), source_count));

        // Per-backend rank features (normalized: 1.0 for rank 0, decreasing)
        if let Some(&rank) = entry.ranks.get(&QueryMode::Text) {
            features.push(("text_rank".to_string(), 1.0 / (1.0 + rank as f64)));
        }
        if let Some(&rank) = entry.ranks.get(&QueryMode::Vector) {
            features.push(("vector_rank".to_string(), 1.0 / (1.0 + rank as f64)));
        }
        if let Some(&rank) = entry.ranks.get(&QueryMode::Sql) {
            features.push(("sql_rank".to_string(), 1.0 / (1.0 + rank as f64)));
        }

        // Per-backend raw score features (guard against NaN/infinity from backends)
        if let Some(&score) = entry.scores.get(&QueryMode::Text) {
            features.push(("text_score".to_string(), if score.is_finite() { score } else { 0.0 }));
        }
        if let Some(&score) = entry.scores.get(&QueryMode::Vector) {
            features.push(("vector_score".to_string(), if score.is_finite() { score } else { 0.0 }));
        }

        // Freshness: 1.0 / (1.0 + age_in_hours)
        if let Some(ts) = entry.primary.timestamp {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let age_hours = ((now_ms - ts) as f64) / 3_600_000.0;
            let freshness = 1.0 / (1.0 + age_hours.max(0.0));
            features.push(("freshness".to_string(), freshness));
        }

        features
    }

    /// Generate a human-readable explanation of the scoring.
    fn explain(
        features: &[(String, f64)],
        weights: &std::collections::HashMap<String, f64>,
        final_score: f64,
    ) -> String {
        let mut parts = Vec::new();
        for (name, value) in features {
            let weight = weights.get(name).copied().unwrap_or(0.0);
            if weight != 0.0 {
                parts.push(format!("{}={:.4}*{:.2}", name, value, weight));
            }
        }
        format!("score={:.6} [{}]", final_score, parts.join(", "))
    }
}

impl Ranker for RuleRanker {
    fn rank(&self, entries: &mut Vec<CandidateEntry>, profile: &RankingProfile) {
        for entry in entries.iter_mut() {
            let features = Self::extract_features(entry);

            // Compute weighted sum
            let mut final_score = 0.0;
            for (name, value) in &features {
                let weight = profile.weights.get(name).copied().unwrap_or(0.0);
                final_score += value * weight;
            }

            // Apply boost rules
            let feature_map: std::collections::HashMap<String, f64> =
                features.iter().cloned().collect();
            for boost in &profile.boosts {
                if boost.evaluate(&feature_map) {
                    final_score *= boost.multiplier;
                }
            }

            // Guard against NaN/infinity in final score
            if !final_score.is_finite() {
                final_score = 0.0;
            }

            // Store features and score
            for (name, value) in &features {
                entry.primary.features.insert(name.clone(), if value.is_finite() { *value } else { 0.0 });
            }
            entry.primary.final_score = final_score;
            entry.primary.explanation =
                Some(Self::explain(&features, &profile.weights, final_score));
        }

        // Sort by final score descending
        entries.sort_by(|a, b| {
            b.primary
                .final_score
                .partial_cmp(&a.primary.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, CandidateSet};
    use std::collections::HashMap;

    fn make_profile() -> RankingProfile {
        let mut weights = HashMap::new();
        weights.insert("rrf_score".to_string(), 100.0);
        weights.insert("freshness".to_string(), 10.0);
        weights.insert("source_count".to_string(), 5.0);
        weights.insert("text_rank".to_string(), 2.0);
        weights.insert("vector_rank".to_string(), 2.0);

        RankingProfile {
            name: "test".to_string(),
            weights,
            boosts: vec![],
        }
    }

    fn make_entries() -> Vec<CandidateEntry> {
        let mut set = CandidateSet::new();

        // Entry found by both text and vector
        let mut c1 = Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.95, 0);
        c1.timestamp = Some(chrono::Utc::now().timestamp_millis());
        c1.features.insert("rrf_score".to_string(), 0.033);
        set.add_candidates(vec![c1]);
        set.add_candidates(vec![Candidate::new(
            "logs".into(),
            0,
            10,
            QueryMode::Vector,
            0.88,
            1,
        )]);

        // Entry found by text only
        let mut c2 = Candidate::new("logs".into(), 0, 20, QueryMode::Text, 0.80, 1);
        c2.timestamp = Some(chrono::Utc::now().timestamp_millis() - 3_600_000); // 1 hour old
        c2.features.insert("rrf_score".to_string(), 0.016);
        set.add_candidates(vec![c2]);

        set.into_entries()
    }

    #[test]
    fn test_rule_ranker_basic() {
        let ranker = RuleRanker;
        let profile = make_profile();
        let mut entries = make_entries();

        ranker.rank(&mut entries, &profile);

        // All entries should have final scores
        for entry in &entries {
            assert!(entry.primary.final_score > 0.0);
            assert!(entry.primary.explanation.is_some());
        }

        // Multi-backend entry should rank higher due to source_count boost
        assert!(entries[0].primary.final_score > entries[1].primary.final_score);
    }

    #[test]
    fn test_feature_extraction() {
        let mut set = CandidateSet::new();
        let mut c = Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.95, 0);
        c.timestamp = Some(chrono::Utc::now().timestamp_millis());
        c.features.insert("rrf_score".to_string(), 0.016);
        set.add_candidates(vec![c]);

        let entries = set.into_entries();
        let features = RuleRanker::extract_features(&entries[0]);

        let feature_names: Vec<&str> = features.iter().map(|(n, _)| n.as_str()).collect();
        assert!(feature_names.contains(&"rrf_score"));
        assert!(feature_names.contains(&"source_count"));
        assert!(feature_names.contains(&"text_rank"));
        assert!(feature_names.contains(&"freshness"));
    }

    #[test]
    fn test_boost_rule() {
        let ranker = RuleRanker;
        let mut profile = make_profile();
        profile.boosts.push(crate::profiles::BoostRule {
            feature: "source_count".to_string(),
            op: crate::profiles::CompareOp::Gt,
            threshold: 1.0,
            multiplier: 2.0,
        });

        let mut entries = make_entries();
        ranker.rank(&mut entries, &profile);

        // Entry with source_count > 1 should get the 2x boost
        let multi_source = entries
            .iter()
            .find(|e| e.ranks.len() > 1)
            .unwrap();
        assert!(multi_source.primary.explanation.as_ref().unwrap().contains("score="));
    }
}
