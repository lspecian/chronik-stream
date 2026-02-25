//! Reciprocal Rank Fusion (RRF) for merging multi-backend results.
//!
//! RRF merges ranked lists from different backends using only rank positions,
//! avoiding the impossible problem of normalizing incompatible scoring systems
//! (BM25, cosine similarity, SQL ORDER BY).
//!
//! Formula: `score(d) = Σ 1 / (k + rank_i(d))` where k is a constant (default 60).
//!
//! Used by Elasticsearch 8.x, Weaviate, Pinecone, and other production systems.

use crate::candidate::{CandidateEntry, CandidateSet};
use crate::types::QueryMode;

/// Reciprocal Rank Fusion merger.
///
/// Computes RRF scores from rank positions across multiple backend result lists,
/// then sorts candidates by their combined RRF score.
pub struct RrfMerger {
    /// RRF constant k (default 60). Higher values reduce the impact of
    /// high-ranking documents from any single backend.
    k: f64,
}

impl Default for RrfMerger {
    fn default() -> Self {
        Self { k: 60.0 }
    }
}

impl RrfMerger {
    /// Create a new RRF merger with the given k constant.
    pub fn new(k: f64) -> Self {
        Self { k }
    }

    /// Merge a candidate set using RRF scoring.
    ///
    /// For each candidate, computes: `rrf_score = Σ 1/(k + rank_i)` across all
    /// backends that found it. Candidates found by more backends get higher scores.
    ///
    /// Returns candidates sorted by RRF score descending, limited to `limit`.
    pub fn merge(&self, candidate_set: CandidateSet, limit: usize) -> Vec<CandidateEntry> {
        let mut entries: Vec<CandidateEntry> = candidate_set.into_entries();

        // Compute RRF score for each entry
        for entry in &mut entries {
            let rrf_score = self.compute_rrf_score(&entry.ranks);
            entry.primary.features.insert("rrf_score".to_string(), rrf_score);
            entry.primary.final_score = rrf_score;
        }

        // Sort by RRF score descending
        entries.sort_by(|a, b| {
            b.primary
                .final_score
                .partial_cmp(&a.primary.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        entries.truncate(limit);
        entries
    }

    /// Compute the RRF score from rank positions across backends.
    fn compute_rrf_score(&self, ranks: &std::collections::HashMap<QueryMode, usize>) -> f64 {
        ranks.values().map(|&rank| 1.0 / (self.k + rank as f64)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::Candidate;

    #[test]
    fn test_rrf_single_backend() {
        let mut set = CandidateSet::new();
        set.add_candidates(vec![
            Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.95, 0),
            Candidate::new("logs".into(), 0, 20, QueryMode::Text, 0.80, 1),
            Candidate::new("logs".into(), 0, 30, QueryMode::Text, 0.70, 2),
        ]);

        let merger = RrfMerger::default();
        let results = merger.merge(set, 10);

        assert_eq!(results.len(), 3);
        // Rank 0 should score highest: 1/(60+0) = 0.01667
        assert!(results[0].primary.final_score > results[1].primary.final_score);
        assert!(results[1].primary.final_score > results[2].primary.final_score);
    }

    #[test]
    fn test_rrf_multi_backend_boost() {
        let mut set = CandidateSet::new();

        // Document at offset 10 found by both text (rank 0) and vector (rank 1)
        // Document at offset 20 found by text only (rank 1)
        // Document at offset 30 found by vector only (rank 0)
        set.add_candidates(vec![
            Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.95, 0),
            Candidate::new("logs".into(), 0, 20, QueryMode::Text, 0.80, 1),
        ]);
        set.add_candidates(vec![
            Candidate::new("logs".into(), 0, 30, QueryMode::Vector, 0.90, 0),
            Candidate::new("logs".into(), 0, 10, QueryMode::Vector, 0.85, 1),
        ]);

        let merger = RrfMerger::default();
        let results = merger.merge(set, 10);

        assert_eq!(results.len(), 3);

        // Offset 10 found by both backends should be ranked first
        // RRF = 1/(60+0) + 1/(60+1) = 0.01667 + 0.01639 = 0.03306
        let top = &results[0];
        assert_eq!(top.primary.offset, 10);
        assert_eq!(top.ranks.len(), 2);

        // The other two should have lower scores (single backend)
        // Offset 30: 1/(60+0) = 0.01667
        // Offset 20: 1/(60+1) = 0.01639
        assert!(results[0].primary.final_score > results[1].primary.final_score);
    }

    #[test]
    fn test_rrf_respects_limit() {
        let mut set = CandidateSet::new();
        set.add_candidates(vec![
            Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.9, 0),
            Candidate::new("logs".into(), 0, 20, QueryMode::Text, 0.8, 1),
            Candidate::new("logs".into(), 0, 30, QueryMode::Text, 0.7, 2),
            Candidate::new("logs".into(), 0, 40, QueryMode::Text, 0.6, 3),
            Candidate::new("logs".into(), 0, 50, QueryMode::Text, 0.5, 4),
        ]);

        let merger = RrfMerger::default();
        let results = merger.merge(set, 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_rrf_custom_k() {
        let mut set = CandidateSet::new();
        set.add_candidates(vec![
            Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.9, 0),
        ]);

        // With k=1, rank 0 gives 1/(1+0) = 1.0
        let merger = RrfMerger::new(1.0);
        let results = merger.merge(set, 10);
        let expected = 1.0 / (1.0 + 0.0);
        assert!((results[0].primary.final_score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rrf_empty_set() {
        let set = CandidateSet::new();
        let merger = RrfMerger::default();
        let results = merger.merge(set, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_rrf_score_feature_populated() {
        let mut set = CandidateSet::new();
        set.add_candidates(vec![
            Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.9, 0),
        ]);

        let merger = RrfMerger::default();
        let results = merger.merge(set, 10);

        assert!(results[0].primary.features.contains_key("rrf_score"));
    }
}
