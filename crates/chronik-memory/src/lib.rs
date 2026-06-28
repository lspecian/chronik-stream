//! Chronik Memory — event-native agent memory, server-internal implementation.
//!
//! This crate is the Rust implementation behind the `/memory/v1/*` HTTP endpoints
//! mounted on Chronik's Unified API (port 6092). It is **not** a public client SDK
//! — see `docs/ROADMAP_AGENT_MEMORY.md` for the architectural decision (AD-1) and
//! the endpoint reference (Appendix A). Customers interact with agent memory via
//! HTTP/JSON; this crate's types are not a stable public API.
//!
//! # Quickstart (Phase 1)
//!
//! ```no_run
//! use chronik_memory::Memory;
//!
//! # async fn run() -> chronik_memory::Result<()> {
//! let mem = Memory::builder()
//!     .chronik_kafka("localhost:9092")
//!     .chronik_api("http://localhost:6092")
//!     .namespace("acme:agent:demo:user:luis")
//!     .build()
//!     .await?;
//!
//! mem.init_namespace().await?;
//! mem.ingest("user", "I want a 3BR in Lapa, max 800k euros").await?;
//! // recall(...) lands in AMS-1.4; AnthropicExtractor in AMS-1.3
//! # Ok(())
//! # }
//! ```
//!
//! # Modules
//!
//! - [`schema`] — envelope and type bodies (fact, event, instruction, task)
//! - [`error`] — `MemoryError` and `Result<T>`
//! - [`topics`] — topic naming conventions
//!
//! # Idempotency model (AMS-2.6)
//!
//! The SDK provides three layers of duplicate-protection, in order of
//! strength:
//!
//! 1. **Caller-provided `external_id`** (strongest, cross-process). Set
//!    `Turn::external_id` to a unique upstream id (e.g. WhatsApp message id,
//!    webhook event id). The SDK uses it as the Kafka record key for
//!    `mem.raw.{ns}` — so identical `external_id` from two processes both
//!    land on the same partition and Chronik can dedup at-rest via log
//!    compaction (when the topic is configured for it).
//!
//! 2. **In-process LRU + TTL** (medium, per-process best-effort). Without an
//!    `external_id`, the SDK derives a key as
//!    `sha256(namespace || role || content)` and consults a 1k-entry,
//!    5-minute LRU. Hits short-circuit before the Kafka produce — the
//!    returned `IngestAck` has `deduped = true`. **Per-process only** — a
//!    second SDK instance / a process restart loses the cache.
//!
//! 3. **Compaction at rest** (lazy, eventually consistent). Typed memories
//!    on compactable topics (`mem.fact.*`, `mem.instruction.*`, `mem.task.*`)
//!    are keyed by `{namespace}|{key}` so log compaction drops superseded
//!    versions. Compaction is async — recall during compaction lag may
//!    return both versions briefly; query-time dedup by `(namespace, key)`
//!    + `version` (in `recall::send`) hides this from callers.
//!
//! ## What to use when
//!
//! | Scenario | Recommended layer |
//! |---|---|
//! | Webhook delivery (WhatsApp, Slack, Stripe) | layer 1 (`external_id` = webhook id) |
//! | Same-process retry of a Kafka produce | layer 2 (auto LRU) |
//! | Two replicas of an extractor worker producing the same extraction | layer 3 (key + version) |
//!
//! ```no_run
//! # use chronik_memory::{Memory, Turn};
//! # async fn run(mem: Memory) -> chronik_memory::Result<()> {
//! // Strict cross-process idempotency for a WhatsApp webhook:
//! mem.ingest_turn(Turn {
//!     role: "user".into(),
//!     content: "I want a 3BR in Lapa".into(),
//!     ts: None,
//!     channel: Some("whatsapp".into()),
//!     external_id: Some("wa-msg-001".into()),
//! }).await?;
//! # Ok(())
//! # }
//! ```
//!
//! Phase 1 surface (AMS-1.1 in progress; AMS-1.2–1.6 pending): client, ingest, recall,
//! extractor. Phase 2 adds the background worker. Phase 3 adds Python/TS bindings.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod audit;
pub mod client;
pub mod concept;
pub mod embeddings;
pub mod error;
pub mod eval;
pub mod extractor;
pub mod forget;
pub mod idempotency;
pub mod ingest;
pub mod lifecycle;
pub mod otel;
pub mod ranking;
pub mod recall;
pub mod registry;
pub mod remember;
pub mod schema;
pub mod topics;
pub mod wikilinks;
pub mod worker;

pub use audit::{audit_topic, emit_audit, AuditEvent};
pub use client::{ExtractionAck, Memory, MemoryBuilder};
pub use concept::{synthesize_concept, ConceptSynthesisError};
pub use error::{MemoryError, Result};
pub use extractor::providers::anthropic::PromptVersion;
pub use extractor::{
    AnthropicExtractor, ChainedExtractor, Extracted, Extractor, OllamaExtractor,
    OllamaPromptVersion, OpenAIExtractor, OpenAIPromptVersion, RuleExtractor, Turn,
    TwoPassExtractor,
};
pub use ingest::IngestAck;
pub use ranking::{
    detect_intent, half_life, intent_boost, Channel, QueryIntent, RRF_K,
};
pub use recall::{
    extract_subject_candidates, RecallBuilder, RecallResult, SqlFilter, SynthesizedAnswer,
    ABSTAIN_LITERAL,
};
pub use schema::{
    Body, ConceptBody, EventBody, FactBody, InstructionBody, MemoryRecord, MemoryType, Source,
    TaskBody, TaskState,
};
pub use embeddings::{cosine_similarity, Embedder, OpenAIEmbedder, TextGenerator};
pub use lifecycle::{DedupDecision, SemanticDedup, DEFAULT_SIMILARITY_THRESHOLD};
pub use registry::{MemoryRegistry, RegistryConfig};
pub use topics::{NamespacePath, TopicLayout};
pub use wikilinks::extract_wikilinks;
pub use worker::{Worker, WorkerConfig, WorkerStats};
