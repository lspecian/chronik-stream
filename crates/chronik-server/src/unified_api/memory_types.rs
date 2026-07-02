//! Wire types for `/memory/v1/*` endpoints (AM-1.7).
//!
//! These are intentionally distinct from `chronik_memory`'s internal types
//! so the public HTTP surface can stay stable while the crate evolves.
//! Conversion happens in [`super::memory`] handlers via `From`/`Into`.
//!
//! Schema reference: `docs/ROADMAP_AGENT_MEMORY.md` Appendix A.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

// ───────────────────────── INGEST ─────────────────────────

/// `POST /memory/v1/ingest`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestRequest {
    /// Namespace path (e.g. `tenant:agent:bot:user:luis`).
    pub namespace: String,
    /// Raw conversation turns to ingest. Asynchronous: extraction happens in a
    /// background worker; this call returns 202 immediately after WAL fsync.
    pub turns: Vec<TurnInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnInput {
    /// `user`, `assistant`, `system`, or any caller-defined role string.
    pub role: String,
    /// Free-form text body.
    pub content: String,
    /// Optional timestamp (defaults to server time at write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    /// Optional channel discriminator (e.g. `slack`, `email`, `web`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Optional caller-supplied id. When present, replaces the default
    /// SHA-256 idempotency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    /// Number of turns durably written to WAL (i.e. not dropped as duplicates).
    pub accepted: usize,
    /// Number of turns rejected as duplicates by the idempotency LRU.
    pub skipped_duplicates: usize,
    /// Caller-facing identifier for this batch (ULID).
    pub batch_id: String,
    /// Per-turn ack details, in input order. Length matches `turns`.
    pub acks: Vec<IngestAckResponse>,
}

/// Single-turn ack. Exposes the typed-topic location so callers can later
/// fetch the raw record via `GET /memory/v1/{memory_id}/source`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestAckResponse {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub deduped: bool,
}

// ───────────────────────── REMEMBER ─────────────────────────

/// `POST /memory/v1/remember` — direct typed-memory write, bypasses extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberRequest {
    pub namespace: String,
    pub r#type: String, // "fact" | "event" | "instruction" | "task" | "concept"
    /// Optional caller-supplied key for compaction. If omitted, an envelope-
    /// derived key is used (depends on the body type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Free-form body, typed by the `type` field. The handler parses this
    /// into the matching `chronik_memory::Body` variant.
    pub body: JsonValue,
    /// Caller's confidence in the memory ([0.0, 1.0]).
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_confidence() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberResponse {
    pub memory_id: String,
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
}

// ───────────────────────── FORGET ─────────────────────────

/// `POST /memory/v1/forget` — emit a null-value record (Kafka tombstone).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetRequest {
    pub namespace: String,
    pub r#type: String,
    /// Either `key` or `memory_id` must be set; if both are present, `memory_id`
    /// wins. The handler resolves to a compaction key, then writes a tombstone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetResponse {
    pub tombstoned: bool,
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
}

// ───────────────────────── RECALL ─────────────────────────

/// `POST /memory/v1/recall` — main query API. Multi-channel fan-out + RRF.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallRequest {
    pub namespace: String,
    pub query: String,
    /// Restrict to a subset of memory types. Defaults to all four (fact, event,
    /// instruction, task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
    /// Top-k cap on the merged result set. Default 10.
    #[serde(default = "default_k")]
    pub k: usize,
    /// Channel selection. Defaults to `["bm25", "vector"]`. Valid:
    /// `bm25`, `vector`, `key_match`, `hyde`, `sql`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    /// Per-channel RRF weights. Missing channels default to 1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<HashMap<String, f64>>,
    /// Inline concept-page bodies into the result set (AM-3.7). Default false.
    #[serde(default)]
    pub include_concepts: bool,
    /// If true, the handler runs the synthesizer over the top-k results and
    /// returns a single fused answer in addition to the raw results.
    #[serde(default)]
    pub synthesize: bool,
    /// Drop memories whose confidence is below this threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f32>,
    /// Time-travel query: only consider memories with `valid_from <= as_of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
}

fn default_k() -> usize {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResponse {
    pub results: Vec<RecallResultResponse>,
    /// Present iff `synthesize: true` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<SynthesisResponse>,
    /// Total wall time for the recall fan-out.
    pub latency_ms: u64,
    /// Echo of which channels actually executed (some may be skipped if the
    /// underlying service is unavailable).
    pub channels_executed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResultResponse {
    pub memory_id: String,
    pub r#type: String,
    pub key: Option<String>,
    pub body: JsonValue,
    pub score: f64,
    pub channels_hit: Vec<String>,
    pub confidence: f32,
    pub version: u64,
    pub valid_from: DateTime<Utc>,
    pub source: SourceRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRef {
    pub topic: String,
    pub offsets: Vec<i64>,
    pub extractor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesisResponse {
    pub answer: String,
    pub abstained: bool,
    pub cited_memory_ids: Vec<String>,
}

// ───────────────────────── SOURCE ─────────────────────────

/// `GET /memory/v1/{memory_id}/source` — provenance walk.
///
/// The AM-2.6 MVP returns the raw-turn *pointer* (`source.topic` +
/// `source.offsets`) so callers can fetch the actual turn payloads via
/// their own Kafka client or `/_sql`. `raw_turns` is left empty for now;
/// server-side resolution is a follow-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceResponse {
    pub memory_id: String,
    /// Pointer to the raw-turn topic + offsets. Callers resolve.
    pub source: SourceRef,
    /// Raw turns, when available. MVP always returns empty; kept in the
    /// wire shape so a future server-side resolver can populate without
    /// clients changing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw_turns: Vec<RawTurnView>,
    /// Extractor identifier that produced this memory (e.g. `anthropic-v3`).
    /// Convenience field — the same value is on `source.extractor`.
    pub extractor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTurnView {
    pub topic: String,
    pub offset: i64,
    pub role: String,
    pub content: String,
    pub ts: DateTime<Utc>,
}

// ───────────────────────── FEEDBACK ─────────────────────────

/// `POST /memory/v1/feedback` — agent reports usefulness for AM-3.3 reranker
/// training.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackRequest {
    pub namespace: String,
    pub memory_id: String,
    pub query: String,
    pub useful: bool,
    pub used_in_response: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackResponse {
    pub recorded: bool,
}

// ───────────────────────── ADMIN ─────────────────────────

/// `POST /memory/v1/admin/init-namespace` — provision the topic set for a
/// `(tenant, agent)` pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitNamespaceRequest {
    pub tenant: String,
    pub agent: String,
    /// Optional set of memory types whose topics should be provisioned.
    /// Defaults to the full set (`fact`, `event`, `instruction`, `task`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitNamespaceResponse {
    pub namespace: String,
    pub topics_created: Vec<String>,
}

// ───────────────────────── HEALTH ─────────────────────────

/// `GET /memory/v1/health` — quick liveness + lag readout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHealthResponse {
    pub status: String,
    pub namespaces_cached: usize,
    pub extractor_provider: Option<String>,
    pub extractor_version: Option<String>,
}

// ───────────────────────── ERRORS ─────────────────────────

/// Uniform error envelope. Never returned with an empty body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Stable machine-readable code: `unauthorized`, `forbidden`, `bad_request`,
    /// `not_found`, `rate_limited`, `internal`, `service_unavailable`.
    pub code: String,
    /// Human-readable diagnostic.
    pub message: String,
    /// Echoes the X-Request-Id header when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl ErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            request_id: None,
        }
    }
}

impl ErrorResponse {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody::new(code, message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ingest_request_round_trips() {
        let req = IngestRequest {
            namespace: "tenant:agent:bot:user:luis".into(),
            turns: vec![TurnInput {
                role: "user".into(),
                content: "hello".into(),
                ts: None,
                channel: None,
                external_id: Some("msg-1".into()),
            }],
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: IngestRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.namespace, req.namespace);
        assert_eq!(back.turns.len(), 1);
    }

    #[test]
    fn recall_request_defaults() {
        // Only required fields → defaults fill in the rest.
        let json = r#"{"namespace": "t:a:b:u:l", "query": "what's the budget?"}"#;
        let req: RecallRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.k, 10);
        assert!(req.channels.is_none());
        assert!(!req.synthesize);
        assert!(!req.include_concepts);
    }

    #[test]
    fn error_response_serializes_with_envelope() {
        let resp = ErrorResponse::new("unauthorized", "X-API-Key required");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""code":"unauthorized""#));
        assert!(s.contains(r#""message":"X-API-Key required""#));
    }

    #[test]
    fn remember_request_accepts_arbitrary_body() {
        let req = RememberRequest {
            namespace: "t:a:b:u:l".into(),
            r#type: "fact".into(),
            key: Some("user|budget|max".into()),
            body: json!({"subject": "user", "predicate": "budget_max", "object": 4000}),
            confidence: 0.95,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: RememberRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.r#type, "fact");
        assert_eq!(back.confidence, 0.95);
    }

    #[test]
    fn forget_request_xor_key_or_memory_id() {
        // Schema doesn't enforce the XOR — handler does. This test just confirms
        // both shapes deserialize cleanly.
        let by_key: ForgetRequest = serde_json::from_str(
            r#"{"namespace": "t:a:b:u:l", "type": "fact", "key": "user|budget|max"}"#,
        )
        .unwrap();
        assert!(by_key.key.is_some());
        assert!(by_key.memory_id.is_none());

        let by_id: ForgetRequest = serde_json::from_str(
            r#"{"namespace": "t:a:b:u:l", "type": "fact", "memory_id": "01HX..."}"#,
        )
        .unwrap();
        assert!(by_id.memory_id.is_some());
    }
}
