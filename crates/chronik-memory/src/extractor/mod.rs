//! Extractor trait — async transformer from raw conversation turns into typed memories.
//!
//! Phase 1 surface (AMS-1.3) provides:
//! - [`Turn`] — a single conversation message with provenance fields.
//! - [`Extractor`] — async trait `extract(turns) -> Vec<Body>`. Implementations:
//!   - `RuleExtractor` (regex pre-pass) — AMS-1.3
//!   - `AnthropicExtractor` (Claude Haiku via tool-use / structured output) — AMS-1.3
//!
//! Phase 2 (AMS-2.3) adds OpenAI, Ollama, and vLLM providers.
//!
//! The trait is object-safe so callers can hold `Box<dyn Extractor>` in the
//! [`Memory`](crate::Memory) struct without a type parameter.

pub mod cached;
pub mod calibration;
pub mod prompts;
pub mod providers;
pub mod rules;
pub mod two_pass;

pub use providers::anthropic::AnthropicExtractor;
pub use providers::ollama::{OllamaExtractor, OllamaPromptVersion};
pub use providers::openai::{OpenAIExtractor, OpenAIPromptVersion};
pub use rules::RuleExtractor;
pub use two_pass::TwoPassExtractor;

use crate::error::Result;
use crate::schema::Body;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single raw conversation turn.
///
/// Produced by callers via [`Memory::ingest`](crate::Memory::ingest), consumed
/// by [`Extractor::extract`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// Speaker role — typically `"user"`, `"assistant"`, `"system"`, or `"tool"`.
    pub role: String,

    /// Message content. Plain text or markdown.
    pub content: String,

    /// Wall-clock timestamp. Defaults to now if `None` at ingest time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,

    /// Channel hint — `"whatsapp"`, `"web"`, `"slack"`, `"internal"`, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,

    /// Caller-provided unique id from the upstream system (e.g. WhatsApp message ID).
    /// When set, the SDK uses it as the Kafka record key for the raw topic and
    /// for cross-process idempotency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

/// One extracted memory, with the source-turn indexes the extractor cited.
///
/// `source_offsets` are indexes into the *batch* passed to [`Extractor::extract`],
/// not Kafka offsets. The [`Memory`](crate::Memory) client maps these to actual
/// Kafka offsets when producing the typed memory record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Extracted {
    /// The structured body the extractor produced.
    pub body: Body,
    /// Optional key for compactable types (fact / instruction / task).
    pub key: Option<String>,
    /// Confidence in [0.0, 1.0].
    pub confidence: f32,
    /// Indexes into the input `turns` slice that this extraction cites.
    /// **Must be valid indexes** — used as a hallucination guard.
    pub source_indexes: Vec<usize>,
}

/// Async extractor trait.
///
/// Implementations should be cheap to clone or wrap in `Arc` — the [`Memory`]
/// client may call `extract` concurrently across batches in Phase 2's worker.
#[async_trait]
pub trait Extractor: Send + Sync + std::fmt::Debug {
    /// Identifier for provenance — typically `"{name}@{version}"`. Embedded in
    /// `source.extractor` of every produced memory.
    fn id(&self) -> &str;

    /// Run extraction on a batch of conversation turns.
    ///
    /// Implementations **must**:
    /// 1. Return an empty Vec when the batch contains no extractable content
    ///    (rather than erroring).
    /// 2. Cite source indexes in `source_indexes` that are valid into `turns`.
    ///    The client verifies this and rejects citations that out-of-range.
    /// 3. Set `confidence` to a calibrated value in [0.0, 1.0].
    async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>>;
}

/// Run multiple extractors in sequence and concatenate their results.
///
/// Typical use: rule pre-pass + LLM main pass — `ChainedExtractor::new(vec![Arc::new(RuleExtractor), Arc::new(anthropic)])`.
/// Extractions from earlier extractors run first; later extractors may emit
/// extractions on the same `(subject, predicate)` key. Supersession is handled
/// downstream by Kafka log compaction at write time and by `(namespace, key)`
/// dedup at recall time — no merging is done here.
#[derive(Debug, Clone)]
pub struct ChainedExtractor {
    inner: Vec<std::sync::Arc<dyn Extractor>>,
    id: String,
}

impl ChainedExtractor {
    /// Build a chain from multiple extractors. The chain id is derived from
    /// the constituent ids (e.g. `chain[rules@v1+anthropic-v1]`) so provenance
    /// stays meaningful.
    pub fn new(inner: Vec<std::sync::Arc<dyn Extractor>>) -> Self {
        let id = format!(
            "chain[{}]",
            inner
                .iter()
                .map(|e| e.id().to_string())
                .collect::<Vec<_>>()
                .join("+")
        );
        Self { inner, id }
    }
}

#[async_trait]
impl Extractor for ChainedExtractor {
    fn id(&self) -> &str {
        &self.id
    }
    async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>> {
        let mut all = Vec::new();
        for e in &self.inner {
            let mut chunk = e.extract(turns).await?;
            all.append(&mut chunk);
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dummy extractor used in client tests where the trait is needed but
    /// extraction itself is irrelevant.
    #[derive(Debug, Clone)]
    pub struct NoopExtractor;

    #[async_trait]
    impl Extractor for NoopExtractor {
        fn id(&self) -> &str {
            "noop@0"
        }
        async fn extract(&self, _turns: &[Turn]) -> Result<Vec<Extracted>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn noop_extractor_returns_empty() {
        let e = NoopExtractor;
        let out = e.extract(&[]).await.unwrap();
        assert!(out.is_empty());
        assert_eq!(e.id(), "noop@0");
    }
}
