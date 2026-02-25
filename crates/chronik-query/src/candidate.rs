//! Candidate normalization for multi-backend query results.
//!
//! A `Candidate` is the normalized result format that all query backends
//! produce. This enables uniform ranking regardless of whether the result
//! came from Tantivy (text), HNSW (vector), DataFusion (SQL), or WAL (fetch).

use crate::types::{QueryMode, RankedResult};
use serde::Serialize;
use std::collections::HashMap;

/// A normalized query result from any backend.
///
/// All query backends convert their native results into `Candidate`s,
/// enabling uniform ranking and deduplication.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Source topic name
    pub topic: String,
    /// Partition number
    pub partition: i32,
    /// Message offset within the partition
    pub offset: i64,
    /// Message timestamp (epoch milliseconds, if available)
    pub timestamp: Option<i64>,
    /// Which backend produced this candidate
    pub source: QueryMode,
    /// Backend-specific raw score (BM25, cosine distance, etc.)
    pub raw_score: f64,
    /// Rank position within the backend's result list (0-indexed)
    pub rank: usize,
    /// Optional text preview of the message content
    pub text_preview: Option<String>,
    /// Arbitrary metadata from the backend
    pub metadata: HashMap<String, serde_json::Value>,
    /// Final score after ranking (set by the ranker)
    pub final_score: f64,
    /// Feature values extracted for ranking
    pub features: HashMap<String, f64>,
    /// Human-readable ranking explanation
    pub explanation: Option<String>,
}

impl Candidate {
    /// Create a new candidate with minimal required fields.
    pub fn new(
        topic: String,
        partition: i32,
        offset: i64,
        source: QueryMode,
        raw_score: f64,
        rank: usize,
    ) -> Self {
        Self {
            topic,
            partition,
            offset,
            timestamp: None,
            source,
            raw_score,
            rank,
            text_preview: None,
            metadata: HashMap::new(),
            final_score: 0.0,
            features: HashMap::new(),
            explanation: None,
        }
    }

    /// Set the timestamp
    pub fn with_timestamp(mut self, ts: i64) -> Self {
        self.timestamp = Some(ts);
        self
    }

    /// Set the text preview
    pub fn with_text_preview(mut self, text: String) -> Self {
        self.text_preview = Some(text);
        self
    }

    /// Unique key for deduplication: (topic, partition, offset)
    pub fn dedup_key(&self) -> (String, i32, i64) {
        (self.topic.clone(), self.partition, self.offset)
    }

    /// Convert this candidate into a `RankedResult` for the API response.
    pub fn into_ranked_result(self) -> RankedResult {
        RankedResult {
            topic: self.topic,
            partition: self.partition,
            offset: self.offset,
            final_score: self.final_score,
            features: self.features,
            explanation: self.explanation,
            text_preview: self.text_preview,
        }
    }
}

/// A set of candidates with deduplication support.
///
/// When multiple backends return the same message (same topic/partition/offset),
/// the `CandidateSet` keeps track of all sources that found it. This is used
/// by RRF to merge rank lists.
#[derive(Debug, Default)]
pub struct CandidateSet {
    /// All candidates, grouped by dedup key
    entries: HashMap<(String, i32, i64), CandidateEntry>,
}

/// A deduplicated entry that may have been found by multiple backends.
#[derive(Debug)]
pub struct CandidateEntry {
    /// The primary candidate (first one found)
    pub primary: Candidate,
    /// Ranks from each backend that found this candidate
    pub ranks: HashMap<QueryMode, usize>,
    /// Raw scores from each backend
    pub scores: HashMap<QueryMode, f64>,
}

impl CandidateSet {
    /// Create an empty candidate set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add candidates from a single backend's result list.
    pub fn add_candidates(&mut self, candidates: Vec<Candidate>) {
        for candidate in candidates {
            let key = candidate.dedup_key();
            let source = candidate.source;
            let rank = candidate.rank;
            let raw_score = candidate.raw_score;

            self.entries
                .entry(key)
                .and_modify(|entry| {
                    // Existing entry: record this backend's rank and score
                    entry.ranks.insert(source, rank);
                    entry.scores.insert(source, raw_score);
                })
                .or_insert_with(|| {
                    let mut ranks = HashMap::new();
                    let mut scores = HashMap::new();
                    ranks.insert(source, rank);
                    scores.insert(source, raw_score);
                    CandidateEntry {
                        primary: candidate,
                        ranks,
                        scores,
                    }
                });
        }
    }

    /// Total number of unique candidates.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Consume the set and return all entries.
    pub fn into_entries(self) -> Vec<CandidateEntry> {
        self.entries.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_candidate_creation() {
        let c = Candidate::new(
            "logs".to_string(),
            0,
            42,
            QueryMode::Text,
            0.95,
            0,
        );
        assert_eq!(c.topic, "logs");
        assert_eq!(c.offset, 42);
        assert_eq!(c.source, QueryMode::Text);
        assert_eq!(c.rank, 0);
    }

    #[test]
    fn test_candidate_dedup_key() {
        let c = Candidate::new("logs".to_string(), 2, 100, QueryMode::Text, 0.5, 0);
        assert_eq!(c.dedup_key(), ("logs".to_string(), 2, 100));
    }

    #[test]
    fn test_candidate_set_deduplication() {
        let mut set = CandidateSet::new();

        // Same message found by text search
        let text_results = vec![
            Candidate::new("logs".to_string(), 0, 42, QueryMode::Text, 0.95, 0),
            Candidate::new("logs".to_string(), 0, 43, QueryMode::Text, 0.80, 1),
        ];

        // Same message also found by vector search
        let vector_results = vec![
            Candidate::new("logs".to_string(), 0, 42, QueryMode::Vector, 0.88, 0),
            Candidate::new("logs".to_string(), 0, 44, QueryMode::Vector, 0.75, 1),
        ];

        set.add_candidates(text_results);
        set.add_candidates(vector_results);

        // Offset 42 should be deduplicated (found by both text and vector)
        assert_eq!(set.len(), 3); // 42 (deduped), 43, 44

        let entries = set.into_entries();
        let entry_42 = entries
            .iter()
            .find(|e| e.primary.offset == 42)
            .unwrap();

        // Should have ranks from both backends
        assert_eq!(entry_42.ranks.len(), 2);
        assert!(entry_42.ranks.contains_key(&QueryMode::Text));
        assert!(entry_42.ranks.contains_key(&QueryMode::Vector));
    }

    #[test]
    fn test_candidate_into_ranked_result() {
        let mut c = Candidate::new("logs".to_string(), 0, 42, QueryMode::Text, 0.95, 0);
        c.final_score = 0.92;
        c.features.insert("text_rank".to_string(), 1.0);
        c.explanation = Some("Top text match".to_string());

        let result = c.into_ranked_result();
        assert_eq!(result.topic, "logs");
        assert_eq!(result.final_score, 0.92);
        assert!(result.features.contains_key("text_rank"));
    }
}
