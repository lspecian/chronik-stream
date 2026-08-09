//! Memory envelope schema — serde types for the four memory types.
//!
//! # Wire format
//!
//! ```json
//! {
//!   "memory_id":   "01HV5XXX...",      // ULID
//!   "tenant_id":   "acme",
//!   "namespace":   "agent:realestate-bot:user:luis",
//!   "type":        "fact",              // tag — drives `body` shape
//!   "key":         "user:luis:occupation",
//!   "version":     3,
//!   "created_at":  "2026-04-25T10:00:00Z",
//!   "valid_from":  "2026-04-25T10:00:00Z",
//!   "valid_to":    null,
//!   "confidence":  0.92,
//!   "source": {
//!     "topic":     "mem.raw.acme.realestate-bot.user-luis",
//!     "offsets":   [142, 143, 144],
//!     "extractor": "extractor-v1@8a3f"
//!   },
//!   "tombstoned":  false,
//!   "body":        { "subject": "user:luis", "predicate": "...", ... }
//! }
//! ```
//!
//! `type` and `body` together encode the [`Body`] tagged enum (serde
//! `tag = "type", content = "body"`). The envelope flattens `Body` so the JSON
//! sees both keys at the top level.
//!
//! # Type-specific bodies
//!
//! - [`FactBody`] — keyed, supersedeable triple-style claim.
//! - [`EventBody`] — time-stamped append-only occurrence.
//! - [`InstructionBody`] — keyed, supersedeable agent rule.
//! - [`TaskBody`] — lifecycle-bearing, state machine via [`TaskState`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Top-level memory envelope.
///
/// `body` is flattened into the outer object via `#[serde(flatten)]`, so the
/// JSON has `type` and `body` at the same level as `memory_id`, `namespace`, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// ULID — time-sortable unique identifier.
    pub memory_id: String,

    /// Tenant identifier. Maps to topic prefix `mem.{type}.{tenant_id}`.
    pub tenant_id: String,

    /// Namespace within tenant — typically `agent:{name}:user:{id}`.
    pub namespace: String,

    /// Stable key for compactable types (fact, instruction, task). `None` for events.
    /// For facts: `{subject}|{predicate}`. For instructions: `{scope}|{trigger}|sha256(rule)`.
    /// For tasks: `task_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Monotonically increasing version per `(tenant_id, namespace, key)`.
    /// For non-keyed events, defaults to 1.
    pub version: u64,

    /// Wall-clock time the memory was minted by the extractor.
    pub created_at: DateTime<Utc>,

    /// Effective-from timestamp. For facts/instructions, when the claim became true.
    /// For events, the event timestamp itself.
    pub valid_from: DateTime<Utc>,

    /// Effective-to timestamp. `None` = currently valid.
    /// Used for time-bounded facts (e.g. "user lived in Lisbon 2023-2025").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<DateTime<Utc>>,

    /// Confidence in [0.0, 1.0]. Calibrated per `(extractor_version, type)`.
    pub confidence: f32,

    /// Provenance — points back to the raw conversation turns this memory was extracted from.
    pub source: Source,

    /// Tombstone marker. When `true`, this record signals deletion and the
    /// underlying Kafka record should be a null-value tombstone for compaction.
    #[serde(default)]
    pub tombstoned: bool,

    /// Type tag + type-specific body. Flattened into the envelope JSON.
    #[serde(flatten)]
    pub body: Body,
}

/// Provenance — which raw turns this memory was extracted from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Source topic, typically `mem.raw.{tenant}.{agent}.{conversation}`.
    pub topic: String,

    /// Kafka offsets in the source topic that this memory cites.
    pub offsets: Vec<i64>,

    /// Extractor identifier — typically `{name}@{git-sha-or-version}`.
    pub extractor: String,

    /// Verbatim excerpt of the source round(s) this memory was extracted
    /// from, as `role: content` lines (WS-0, ROADMAP_MEMORY_QUALITY.md).
    ///
    /// Facts-as-keys, rounds-as-values: the (s, p, o) triple is the
    /// retrieval key; this excerpt is the value handed to synthesis. It
    /// preserves exact wording (assistant-stated names, quantities, quotes)
    /// that atomic extraction loses, and its presence in the indexed JSON
    /// also lets BM25 match the raw phrasing (fact-expansion).
    ///
    /// Capped at ingest (see `client.rs`) so envelopes stay bounded.
    /// `None` on records written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

/// Memory type discriminant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Atomic, supersedeable knowledge claim.
    Fact,
    /// Time-stamped, append-only occurrence.
    Event,
    /// Procedural rule for the agent.
    Instruction,
    /// Lifecycle-bearing action item.
    Task,
    /// Synthesized entity-rollup page (AMS-3.7). See [`ConceptBody`].
    /// Ships in the SDK as Phase 3 work; the type is added here so the
    /// envelope and topic layout know about it from day one — the synthesis
    /// worker / on-demand path lands incrementally.
    Concept,
}

impl MemoryType {
    /// String tag used in topic names (`mem.fact.{tenant}` etc.) and in the JSON `type` field.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::Fact => "fact",
            MemoryType::Event => "event",
            MemoryType::Instruction => "instruction",
            MemoryType::Task => "task",
            MemoryType::Concept => "concept",
        }
    }
}

/// Type-specific memory body. The `type` discriminator is serialized at the
/// envelope level (via `#[serde(flatten)]` on [`MemoryRecord::body`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "lowercase")]
pub enum Body {
    /// Fact body — `(subject, predicate, object)` triple.
    Fact(FactBody),
    /// Event body — `(actor, verb, object)` with a timestamp.
    Event(EventBody),
    /// Instruction body — agent rule with scope and trigger.
    Instruction(InstructionBody),
    /// Task body — state machine.
    Task(TaskBody),
    /// Concept page body — synthesized entity-rollup (AMS-3.7).
    Concept(ConceptBody),
}

impl Body {
    /// Discriminant of this body.
    pub fn kind(&self) -> MemoryType {
        match self {
            Body::Fact(_) => MemoryType::Fact,
            Body::Event(_) => MemoryType::Event,
            Body::Instruction(_) => MemoryType::Instruction,
            Body::Task(_) => MemoryType::Task,
            Body::Concept(_) => MemoryType::Concept,
        }
    }
}

/// Fact body — atomic, supersedeable triple-style claim.
///
/// Topic: `mem.fact.{tenant}` (compacted, key=`{namespace}|{subject}|{predicate}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactBody {
    /// Entity the claim is about (e.g. `user:luis`).
    pub subject: String,
    /// Relation type (e.g. `prefers_neighborhood`).
    pub predicate: String,
    /// Value of the claim. `serde_json::Value` to allow strings, numbers, etc.
    pub object: serde_json::Value,
    /// Polarity — `asserted`, `negated`, `uncertain`, or `conflicting` (Phase 3).
    #[serde(default = "default_polarity")]
    pub polarity: String,
    /// Free-text rendering of the claim. Indexed by Tantivy.
    pub text: String,
    /// Who stated the claim in the source conversation — `"user"` or
    /// `"assistant"` (WS-2, ROADMAP_MEMORY_QUALITY.md). Assistant-stated
    /// facts (recommendations, designations, quotes the user accepted) are
    /// first-class memories; queries like "what did you tell me about X"
    /// boost `speaker == "assistant"` at recall and synthesis commits to
    /// them without requiring the user to have restated them.
    /// Defaults to `"user"` for records written before this field existed.
    #[serde(default = "default_speaker")]
    pub speaker: String,
}

fn default_polarity() -> String {
    "asserted".to_string()
}

fn default_speaker() -> String {
    "user".to_string()
}

/// Event body — time-stamped append-only occurrence.
///
/// Topic: `mem.event.{tenant}` (append-only, key=`memory_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBody {
    /// Who or what performed the action (e.g. `user:luis`, `agent:realestate-bot`).
    pub actor: String,
    /// Action taken (e.g. `viewed_property`, `replied_with_matches`).
    pub verb: String,
    /// Target of the action (e.g. `property:LX-4471`). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// Channel the event came from (e.g. `whatsapp`, `web`, `internal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Free-text context for the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Event timestamp — distinct from envelope `valid_from` only when ingested late.
    pub ts: DateTime<Utc>,
}

/// Instruction body — agent rule.
///
/// Topic: `mem.instruction.{tenant}` (compacted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionBody {
    /// Where this rule applies (e.g. `agent:realestate-bot`).
    pub scope: String,
    /// The rule text.
    pub rule: String,
    /// When the rule fires (e.g. `before_reply`, `on_match`, `always`).
    pub trigger: String,
    /// Higher = applied later / wins ties. Default 0.
    #[serde(default)]
    pub priority: i32,
}

/// Task lifecycle states.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Created, not yet started.
    Open,
    /// Currently being worked on.
    InProgress,
    /// Completed successfully.
    Done,
    /// Cancelled before completion.
    Cancelled,
}

/// Task body — lifecycle-bearing action item.
///
/// State transitions are produced as new records keyed by `task_id`. A compacted
/// view topic `mem.task.current.{tenant}` materializes the latest state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBody {
    /// Stable task identifier (typically a ULID).
    pub task_id: String,
    /// Short human-readable title.
    pub title: String,
    /// Current state.
    pub state: TaskState,
    /// Optional due timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,
    /// Owner identifier (e.g. `agent:realestate-bot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Other task IDs this task depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// Concept page body — synthesized rollup of all atomic memories about
/// a single entity (AMS-3.7).
///
/// Where `FactBody` is one atomic claim, a concept page is a coherent
/// markdown document covering everything we know about an entity. The
/// recall path can inline a top-1 concept page above the atomic memories
/// to give callers a single readable answer instead of N RRF'd fragments.
///
/// **Topic**: `mem.concept.{tenant}` — compacted, keyed by `concept:{entity_id}`
/// so the latest synthesis supersedes older versions of the same page.
///
/// **Synthesis**: a concept worker (binary, separate from the extraction
/// worker — see AMS-3.7) consumes from `mem.fact.*` / `mem.event.*` /
/// `mem.summary.*` and regenerates pages on a per-entity threshold trigger
/// (default: every 50 new memories OR every 24 h). The synthesis prompt
/// takes all atomic memories about an entity plus the previous concept
/// page version (for continuity) and emits markdown.
///
/// **Wikilinks**: the markdown can contain `[[entity_id]]` references; the
/// worker parses them out and stores them in `links_out` so recall can
/// optionally traverse to related concept pages
/// (`RecallBuilder::expand_concepts(max_hops)`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConceptBody {
    /// Identifier of the entity this page is about. Same shape as `subject`
    /// in `FactBody` — `user:luis`, `property:rua_das_flores_42`,
    /// `documentary:our_planet`, `roscioli`.
    pub entity_id: String,

    /// Coarse type tag — used by recall to filter ("show me concept pages
    /// about properties only"). Free-form snake_case: `user`, `property`,
    /// `documentary`, `place`, `org`, etc.
    pub entity_type: String,

    /// Human-readable title shown at the top of the page.
    pub title: String,

    /// Synthesized markdown content. May contain `[[entity_id]]` wikilinks.
    pub markdown: String,

    /// Wikilink targets parsed out of `markdown`. Stored separately so
    /// recall traversal doesn't need to re-parse on every read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links_out: Vec<String>,

    /// Number of atomic memories that fed this synthesis. Used by recall
    /// to surface a staleness warning when the live count diverges from
    /// this snapshot count by a threshold.
    #[serde(default)]
    pub source_memory_count: u32,

    /// Extractor / synthesizer versions that produced the underlying
    /// memories. Used for provenance + cache invalidation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_extractor_versions: Vec<String>,

    /// When this synthesis ran. Different from envelope `valid_from` (which
    /// reflects the entity's *valid time*, not the synthesis time).
    pub last_synthesized_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_source() -> Source {
        Source {
            topic: "mem.raw.acme.bot.user-luis".to_string(),
            offsets: vec![1, 2, 3],
            extractor: "extractor-v1@deadbeef".to_string(), excerpt: None,
        }
    }

    fn dummy_envelope(body: Body) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: "01HV5TESTULID00000000000000".to_string(),
            tenant_id: "acme".to_string(),
            namespace: "agent:bot:user:luis".to_string(),
            key: Some("k".to_string()),
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 0.9,
            source: dummy_source(),
            tombstoned: false,
            body,
        }
    }

    #[test]
    fn fact_round_trips() {
        let m = dummy_envelope(Body::Fact(FactBody {
            subject: "user:luis".to_string(),
            predicate: "prefers_neighborhood".to_string(),
            object: serde_json::json!("Lapa"),
            polarity: "asserted".to_string(),
            text: "Luis prefers Lapa".to_string(),
                speaker: "user".into(),
        }));

        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["type"], "fact");
        assert_eq!(json["body"]["subject"], "user:luis");
        assert_eq!(json["body"]["predicate"], "prefers_neighborhood");

        let parsed: MemoryRecord = serde_json::from_value(json).unwrap();
        match parsed.body {
            Body::Fact(f) => assert_eq!(f.subject, "user:luis"),
            _ => panic!("wrong body variant"),
        }
    }

    #[test]
    fn event_omits_optional_fields() {
        let m = dummy_envelope(Body::Event(EventBody {
            actor: "user:luis".to_string(),
            verb: "viewed_property".to_string(),
            object: Some("property:LX-4471".to_string()),
            channel: None,
            context: None,
            ts: Utc::now(),
        }));
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["type"], "event");
        // `channel` and `context` are skipped when None
        assert!(json["body"].get("channel").is_none());
        assert!(json["body"].get("context").is_none());
        assert_eq!(json["body"]["object"], "property:LX-4471");
    }

    #[test]
    fn task_state_serializes_snake_case() {
        let m = dummy_envelope(Body::Task(TaskBody {
            task_id: "tsk_01".to_string(),
            title: "Schedule viewing".to_string(),
            state: TaskState::InProgress,
            due_at: None,
            owner: None,
            depends_on: vec![],
        }));
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["body"]["state"], "in_progress");
    }

    #[test]
    fn instruction_default_priority() {
        let json = serde_json::json!({
            "memory_id": "01HV5",
            "tenant_id": "acme",
            "namespace": "agent:bot",
            "version": 1,
            "created_at": "2026-04-25T10:00:00Z",
            "valid_from": "2026-04-25T10:00:00Z",
            "confidence": 1.0,
            "source": {
                "topic": "mem.raw.acme",
                "offsets": [1],
                "extractor": "x@1"
            },
            "type": "instruction",
            "body": {
                "scope": "agent:bot",
                "rule": "Always reply in Portuguese",
                "trigger": "before_reply"
            }
        });
        let parsed: MemoryRecord = serde_json::from_value(json).unwrap();
        match parsed.body {
            Body::Instruction(i) => {
                assert_eq!(i.priority, 0);
                assert_eq!(i.rule, "Always reply in Portuguese");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn body_kind_matches_serialized_tag() {
        for (body, expected) in [
            (
                Body::Fact(FactBody {
                    subject: "s".into(),
                    predicate: "p".into(),
                    object: serde_json::json!("o"),
                    polarity: "asserted".into(),
                    text: "t".into(),
                speaker: "user".into(),
                }),
                MemoryType::Fact,
            ),
            (
                Body::Event(EventBody {
                    actor: "a".into(),
                    verb: "v".into(),
                    object: None,
                    channel: None,
                    context: None,
                    ts: Utc::now(),
                }),
                MemoryType::Event,
            ),
        ] {
            assert_eq!(body.kind(), expected);
            assert_eq!(body.kind().as_str(), expected.as_str());
        }
    }
}
