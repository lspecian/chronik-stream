//! Memory lifecycle helpers — opt-in utilities that operate **outside** the
//! hot path. None of this runs automatically; callers compose these helpers
//! into their own pipelines.
//!
//! # Phase 2 contents
//!
//! - [`SemanticDedup`] — compare a freshly extracted fact against existing
//!   memories on the same `(subject, predicate)` key. If cosine similarity of
//!   the rendered text exceeds a threshold (default 0.97), the new extraction
//!   is a near-duplicate.
//! - [`DedupDecision`] — what to do about it: drop, supersede, or keep both.
//!
//! # Why not automatic?
//!
//! Semantic dedup adds latency (an embedding call per new fact) and cost. Most
//! deployments are happy with Kafka log compaction's "exact-key wins" + the
//! `version` field — i.e. when a re-extraction produces the same
//! `(subject, predicate)`, the higher-versioned record wins. Semantic dedup is
//! the "but the wording is different" backstop, useful for noisy extractors
//! and at higher tenant volumes. Wiring it into the hot path is a Phase 3
//! lifecycle-controller concern (AMS-3.6).

use crate::embeddings::{cosine_similarity, Embedder};
use crate::error::Result;
use crate::extractor::Extracted;
use crate::schema::{Body, MemoryRecord};

/// What [`SemanticDedup::decide`] recommends for a freshly extracted fact.
#[derive(Debug, Clone, PartialEq)]
pub enum DedupDecision {
    /// The new extraction is sufficiently distinct — write it.
    Keep,
    /// A near-duplicate already exists with **equal-or-higher** confidence.
    /// Skip the produce.
    Drop {
        /// `memory_id` of the existing memory we matched against.
        existing_id: String,
        /// Cosine similarity between the new extraction and the matched record.
        similarity: f32,
    },
    /// A near-duplicate already exists, but the new extraction has higher
    /// confidence. Produce, supersede the older record (compaction takes care
    /// of cleanup since the key is the same).
    Supersede {
        /// `memory_id` of the existing memory the new record will supersede.
        existing_id: String,
        /// Cosine similarity between the new extraction and the matched record.
        similarity: f32,
    },
}

/// Default cosine threshold above which two facts are considered near-duplicates.
pub const DEFAULT_SIMILARITY_THRESHOLD: f32 = 0.97;

/// Stateless decision helper. Wrap an [`Embedder`] + threshold and call
/// [`decide`](Self::decide) per extraction.
#[derive(Debug, Clone)]
pub struct SemanticDedup<E: Embedder + Clone> {
    embedder: E,
    threshold: f32,
}

impl<E: Embedder + Clone> SemanticDedup<E> {
    /// Build with the given embedder and the default threshold.
    pub fn new(embedder: E) -> Self {
        Self {
            embedder,
            threshold: DEFAULT_SIMILARITY_THRESHOLD,
        }
    }

    /// Override the cosine threshold (default 0.97). Must be in `[-1, 1]`.
    pub fn with_threshold(mut self, t: f32) -> Self {
        self.threshold = t;
        self
    }

    /// Threshold this dedup helper uses.
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Decide what to do with a freshly extracted memory given a slice of
    /// existing memories that share its `(subject, predicate)` key.
    ///
    /// Caller is responsible for fetching the candidate `existing` set —
    /// typically by calling `Memory::recall` with `types=[Fact]` and a query
    /// that matches the new fact's text.
    ///
    /// Returns [`DedupDecision::Keep`] when:
    /// - `existing` is empty, OR
    /// - no existing record exceeds the similarity threshold, OR
    /// - the new fact's confidence is strictly greater than every above-threshold
    ///   match (in that case [`DedupDecision::Supersede`] is returned instead).
    ///
    /// Only operates on facts. Events / instructions / tasks always return
    /// [`DedupDecision::Keep`] — semantic dedup isn't meaningful for them
    /// (events are append-only by definition, instructions key on rule-hash
    /// already, tasks key on task_id).
    pub async fn decide(
        &self,
        new: &Extracted,
        existing: &[MemoryRecord],
    ) -> Result<DedupDecision> {
        let new_text = match &new.body {
            Body::Fact(f) => &f.text,
            _ => return Ok(DedupDecision::Keep),
        };

        if existing.is_empty() {
            return Ok(DedupDecision::Keep);
        }

        let new_vec = self.embedder.embed(new_text).await?;

        let mut best: Option<(usize, f32)> = None;
        // Embed each candidate's text in batch for throughput.
        let texts: Vec<String> = existing
            .iter()
            .filter_map(|m| match &m.body {
                Body::Fact(f) => Some(f.text.clone()),
                _ => None,
            })
            .collect();
        let vectors = self.embedder.embed_batch(&texts).await?;

        // We embedded only fact bodies; build a parallel index back to the
        // matching `existing` entries.
        let mut fact_idx_to_existing_idx: Vec<usize> = Vec::with_capacity(texts.len());
        for (i, m) in existing.iter().enumerate() {
            if matches!(&m.body, Body::Fact(_)) {
                fact_idx_to_existing_idx.push(i);
            }
        }

        for (vi, v) in vectors.iter().enumerate() {
            let sim = cosine_similarity(&new_vec, v);
            match best {
                Some((_, b)) if sim <= b => {}
                _ => best = Some((vi, sim)),
            }
        }

        let (vi, sim) = match best {
            Some(t) => t,
            None => return Ok(DedupDecision::Keep),
        };
        if sim < self.threshold {
            return Ok(DedupDecision::Keep);
        }

        // Above threshold — decide drop vs supersede by confidence.
        let existing_idx = fact_idx_to_existing_idx[vi];
        let existing_record = &existing[existing_idx];
        if new.confidence > existing_record.confidence {
            Ok(DedupDecision::Supersede {
                existing_id: existing_record.memory_id.clone(),
                similarity: sim,
            })
        } else {
            Ok(DedupDecision::Drop {
                existing_id: existing_record.memory_id.clone(),
                similarity: sim,
            })
        }
    }
}

// ============================================================================
// Decay helper (re-export for convenience)
// ============================================================================

pub use crate::ranking::decay_factor;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::Extracted;
    use crate::schema::{Body, EventBody, FactBody, Source};
    use async_trait::async_trait;
    use chrono::Utc;

    /// Local mock — same shape as the one in `embeddings::tests` but inlined
    /// so we don't depend on a sibling test module's private types.
    #[derive(Debug, Clone)]
    struct MockEmbedder {
        dim: usize,
    }

    #[async_trait]
    impl Embedder for MockEmbedder {
        fn id(&self) -> &str {
            "mock-embedder"
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            use sha2::{Digest, Sha256};
            let h = Sha256::digest(text.as_bytes());
            let mut v = Vec::with_capacity(self.dim);
            for i in 0..self.dim {
                v.push(h[i % h.len()] as f32 / 255.0);
            }
            let n = (v.iter().map(|x| x * x).sum::<f32>()).sqrt();
            if n > 0.0 {
                v.iter_mut().for_each(|x| *x /= n);
            }
            Ok(v)
        }
    }

    fn fact_extracted(text: &str, conf: f32) -> Extracted {
        Extracted {
            body: Body::Fact(FactBody {
                subject: "user".into(),
                predicate: "p".into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: text.into(),
                speaker: "user".into(),
            }),
            key: Some("user|p".into()),
            confidence: conf,
            source_indexes: vec![0],
        }
    }

    fn fact_record(text: &str, conf: f32, id: &str) -> MemoryRecord {
        MemoryRecord {
            memory_id: id.into(),
            tenant_id: "t".into(),
            namespace: "t:ns".into(),
            key: Some("user|p".into()),
            version: 1,
            created_at: Utc::now(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: conf,
            source: Source {
                topic: "x".into(),
                offsets: vec![0],
                extractor: "x".into(), excerpt: None,
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: "user".into(),
                predicate: "p".into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: text.into(),
                speaker: "user".into(),
            }),
        }
    }

    fn event_record() -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: "e1".into(),
            tenant_id: "t".into(),
            namespace: "t:ns".into(),
            key: None,
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "x".into(),
                offsets: vec![0],
                extractor: "x".into(), excerpt: None,
            },
            tombstoned: false,
            body: Body::Event(EventBody {
                actor: "a".into(),
                verb: "v".into(),
                object: None,
                channel: None,
                context: None,
                ts: now,
            }),
        }
    }

    #[tokio::test]
    async fn keeps_when_no_existing() {
        let dedup = SemanticDedup::new(MockEmbedder { dim: 32 });
        let new = fact_extracted("Luis prefers Lapa", 0.9);
        let d = dedup.decide(&new, &[]).await.unwrap();
        assert_eq!(d, DedupDecision::Keep);
    }

    #[tokio::test]
    async fn keeps_non_fact_extractions() {
        let dedup = SemanticDedup::new(MockEmbedder { dim: 32 });
        let mut e = fact_extracted("X", 0.9);
        e.body = Body::Event(EventBody {
            actor: "a".into(),
            verb: "v".into(),
            object: None,
            channel: None,
            context: None,
            ts: Utc::now(),
        });
        let d = dedup.decide(&e, &[fact_record("y", 0.5, "id-1")]).await.unwrap();
        assert_eq!(d, DedupDecision::Keep);
    }

    #[tokio::test]
    async fn drops_near_duplicate_with_lower_confidence() {
        // Mock embedder: same text → same vector → cosine sim = 1.0 ≥ threshold.
        let dedup = SemanticDedup::new(MockEmbedder { dim: 32 });
        let new = fact_extracted("Luis prefers Lapa", 0.5);
        let existing = vec![fact_record("Luis prefers Lapa", 0.9, "old-id")];
        let d = dedup.decide(&new, &existing).await.unwrap();
        match d {
            DedupDecision::Drop { existing_id, similarity } => {
                assert_eq!(existing_id, "old-id");
                assert!(similarity > 0.99);
            }
            other => panic!("expected Drop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supersedes_near_duplicate_with_higher_confidence() {
        let dedup = SemanticDedup::new(MockEmbedder { dim: 32 });
        let new = fact_extracted("Luis prefers Lapa", 0.95);
        let existing = vec![fact_record("Luis prefers Lapa", 0.5, "old-id")];
        let d = dedup.decide(&new, &existing).await.unwrap();
        match d {
            DedupDecision::Supersede { existing_id, similarity } => {
                assert_eq!(existing_id, "old-id");
                assert!(similarity > 0.99);
            }
            other => panic!("expected Supersede, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keeps_when_below_threshold() {
        let dedup = SemanticDedup::new(MockEmbedder { dim: 32 }).with_threshold(0.99);
        let new = fact_extracted("entirely different sentence about ducks", 0.9);
        let existing = vec![fact_record("Luis prefers Lapa", 0.5, "old-id")];
        let d = dedup.decide(&new, &existing).await.unwrap();
        assert_eq!(d, DedupDecision::Keep);
    }

    #[tokio::test]
    async fn ignores_non_fact_existing_records() {
        let dedup = SemanticDedup::new(MockEmbedder { dim: 32 });
        let new = fact_extracted("Luis prefers Lapa", 0.9);
        // Existing has only an event — no fact text to compare against.
        let existing = vec![event_record()];
        let d = dedup.decide(&new, &existing).await.unwrap();
        assert_eq!(d, DedupDecision::Keep);
    }
}
