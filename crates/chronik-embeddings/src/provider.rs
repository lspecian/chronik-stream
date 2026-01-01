//! Embedding provider trait and common types.
//!
//! Defines the interface that all embedding providers must implement.

use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

/// Result of embedding a single text.
#[derive(Debug, Clone)]
pub struct EmbeddingResult {
    /// The embedding vector.
    pub vector: Vec<f32>,
    /// Original text (for debugging).
    pub text: String,
    /// Token count (if available).
    pub token_count: Option<usize>,
}

/// Batch of embeddings.
#[derive(Debug, Clone)]
pub struct EmbeddingBatch {
    /// Individual embedding results.
    pub embeddings: Vec<EmbeddingResult>,
    /// Total processing time.
    pub duration: Duration,
    /// Total tokens processed (if available).
    pub total_tokens: Option<usize>,
}

impl EmbeddingBatch {
    /// Create a new batch from embeddings.
    pub fn new(embeddings: Vec<EmbeddingResult>, duration: Duration) -> Self {
        let total_tokens = embeddings
            .iter()
            .filter_map(|e| e.token_count)
            .sum::<usize>();

        Self {
            embeddings,
            duration,
            total_tokens: if total_tokens > 0 { Some(total_tokens) } else { None },
        }
    }

    /// Get just the vectors.
    pub fn vectors(&self) -> Vec<Vec<f32>> {
        self.embeddings.iter().map(|e| e.vector.clone()).collect()
    }

    /// Number of embeddings in the batch.
    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    /// Check if batch is empty.
    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }
}

/// Trait for embedding providers.
///
/// Implementations convert text into high-dimensional vectors suitable
/// for similarity search.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single text string.
    async fn embed(&self, text: &str) -> Result<EmbeddingResult>;

    /// Embed multiple texts in a batch (more efficient).
    async fn embed_batch(&self, texts: &[&str]) -> Result<EmbeddingBatch>;

    /// Get the vector dimensions for this provider.
    fn dimensions(&self) -> usize;

    /// Get the provider name for logging/metrics.
    fn name(&self) -> &str;

    /// Get the model name.
    fn model(&self) -> &str;

    /// Maximum batch size supported by this provider.
    fn max_batch_size(&self) -> usize {
        32
    }

    /// Maximum input tokens per text (for truncation).
    fn max_input_tokens(&self) -> usize {
        8191 // OpenAI default
    }
}

/// Statistics for embedding operations.
#[derive(Debug, Clone, Default)]
pub struct EmbeddingStats {
    /// Total texts embedded.
    pub texts_embedded: u64,
    /// Total tokens processed.
    pub tokens_processed: u64,
    /// Total embedding time.
    pub total_duration: Duration,
    /// Number of API calls made.
    pub api_calls: u64,
    /// Number of errors encountered.
    pub errors: u64,
}

impl EmbeddingStats {
    /// Record a successful batch.
    pub fn record_batch(&mut self, batch: &EmbeddingBatch) {
        self.texts_embedded += batch.len() as u64;
        if let Some(tokens) = batch.total_tokens {
            self.tokens_processed += tokens as u64;
        }
        self.total_duration += batch.duration;
        self.api_calls += 1;
    }

    /// Record an error.
    pub fn record_error(&mut self) {
        self.errors += 1;
    }

    /// Average time per embedding.
    pub fn avg_embedding_time(&self) -> Duration {
        if self.texts_embedded == 0 {
            Duration::ZERO
        } else {
            self.total_duration / self.texts_embedded as u32
        }
    }

    /// Average tokens per text.
    pub fn avg_tokens_per_text(&self) -> f64 {
        if self.texts_embedded == 0 {
            0.0
        } else {
            self.tokens_processed as f64 / self.texts_embedded as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_batch() {
        let embeddings = vec![
            EmbeddingResult {
                vector: vec![0.1, 0.2, 0.3],
                text: "hello".to_string(),
                token_count: Some(1),
            },
            EmbeddingResult {
                vector: vec![0.4, 0.5, 0.6],
                text: "world".to_string(),
                token_count: Some(1),
            },
        ];

        let batch = EmbeddingBatch::new(embeddings, Duration::from_millis(100));

        assert_eq!(batch.len(), 2);
        assert!(!batch.is_empty());
        assert_eq!(batch.total_tokens, Some(2));

        let vectors = batch.vectors();
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0], vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn test_embedding_stats() {
        let mut stats = EmbeddingStats::default();

        let batch = EmbeddingBatch::new(
            vec![
                EmbeddingResult {
                    vector: vec![0.1],
                    text: "test".to_string(),
                    token_count: Some(10),
                },
            ],
            Duration::from_millis(50),
        );

        stats.record_batch(&batch);
        assert_eq!(stats.texts_embedded, 1);
        assert_eq!(stats.tokens_processed, 10);
        assert_eq!(stats.api_calls, 1);

        stats.record_error();
        assert_eq!(stats.errors, 1);
    }
}
