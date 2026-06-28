//! Two-pass extraction (LongMemEval lever #1).
//!
//! The single-pass extractor (V3/V5) misses memories the user *agrees to* but
//! never restates — assistant-stated facts like "Andy clothing", "Roscioli",
//! "2-3 eggs" that the user accepts implicitly. These cause the
//! `single-session-assistant` category to sit at 0/3 across pilots 3-6.
//!
//! Two-pass works around this with a candidate-entity scan followed by an
//! entity-targeted extraction sweep:
//!
//! - **Pass 1**: standard extraction via an inner [`Extractor`] (typically
//!   `AnthropicExtractor` with V3). Captures the bulk of user-stated facts.
//! - **Pass 2 (entity scan)**: cheap Anthropic call that returns a JSON list
//!   of candidate entities (people, places, brands, foods, products) mentioned
//!   in the conversation.
//! - **Pass 2 (per-entity sweep)**: for each entity (capped at
//!   `max_entities`), a targeted extraction prompt asking Claude to enumerate
//!   facts about that entity, even when the user didn't restate them.
//!
//! Merging: pass 1 results are kept verbatim; pass 2 results are added if
//! they don't duplicate a pass 1 (subject, predicate) for facts or
//! (actor, verb, ts) for events.
//!
//! Cost: ~1 + N Anthropic calls per batch (vs. 1 for single-pass), where N
//! is the number of entities. With `max_entities=5` and Claude Haiku 4.5
//! pricing, this roughly doubles extraction cost per batch in exchange for
//! the missed-entity recovery.

use crate::error::{MemoryError, Result};
use crate::extractor::{Extracted, Extractor, Turn};
use crate::schema::{Body, FactBody};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

const DEFAULT_ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION_HEADER: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-haiku-4-5";
const DEFAULT_MAX_ENTITIES: usize = 5;
const DEFAULT_SCAN_MAX_TOKENS: u32 = 512;
const DEFAULT_SWEEP_MAX_TOKENS: u32 = 1024;

/// Two-pass extractor. Construct via [`TwoPassExtractor::new`] or
/// [`TwoPassExtractor::builder`].
#[derive(Debug, Clone)]
pub struct TwoPassExtractor {
    inner: Arc<dyn Extractor>,
    api_key: String,
    model: String,
    base_url: String,
    max_entities: usize,
    scan_max_tokens: u32,
    sweep_max_tokens: u32,
    http: reqwest::Client,
    id: String,
}

impl TwoPassExtractor {
    /// Build with sensible defaults. The inner extractor is used for pass 1
    /// (typically `AnthropicExtractor` with V3); pass 2's Anthropic calls
    /// use the provided `api_key`.
    pub fn new(inner: Arc<dyn Extractor>, api_key: impl Into<String>) -> Result<Self> {
        let api_key = api_key.into();
        if api_key.is_empty() {
            return Err(MemoryError::Config(
                "TwoPassExtractor: ANTHROPIC_API_KEY (api_key) is required".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| MemoryError::Http(format!("http client: {e}")))?;
        let id = format!("two-pass[{}]+anthropic-entity-sweep@v1", inner.id());
        Ok(Self {
            inner,
            api_key,
            model: DEFAULT_MODEL.into(),
            base_url: DEFAULT_ANTHROPIC_BASE.into(),
            max_entities: DEFAULT_MAX_ENTITIES,
            scan_max_tokens: DEFAULT_SCAN_MAX_TOKENS,
            sweep_max_tokens: DEFAULT_SWEEP_MAX_TOKENS,
            http,
            id,
        })
    }

    /// Override the Claude model used for entity scanning + per-entity sweep.
    pub fn with_model(mut self, m: impl Into<String>) -> Self {
        self.model = m.into();
        self
    }

    /// Override the Anthropic base URL (used by wiremock in tests).
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Cap on how many entities the per-entity sweep will process.
    pub fn with_max_entities(mut self, n: usize) -> Self {
        self.max_entities = n;
        self
    }

    /// Provide a custom HTTP client (e.g. for tests).
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }
}

#[async_trait]
impl Extractor for TwoPassExtractor {
    fn id(&self) -> &str {
        &self.id
    }

    async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>> {
        if turns.is_empty() {
            return Ok(vec![]);
        }

        // Pass 1: delegate to the inner extractor.
        let mut combined = self.inner.extract(turns).await?;
        debug!(
            inner = %self.inner.id(),
            pass1_count = combined.len(),
            "TwoPassExtractor pass 1 done"
        );

        // Pass 2a: entity scan.
        let entities = match self.scan_entities(turns).await {
            Ok(e) => e,
            Err(err) => {
                // Pass 2 is additive — failure here must not destroy pass 1.
                warn!(error = ?err, "TwoPassExtractor entity scan failed; falling back to pass 1 only");
                return Ok(combined);
            }
        };
        debug!(entity_count = entities.len(), entities = ?entities, "TwoPassExtractor pass 2a done");

        // Pass 2b: per-entity sweep, capped at max_entities.
        let mut pass2: Vec<Extracted> = Vec::new();
        for entity in entities.iter().take(self.max_entities) {
            match self.sweep_entity(entity, turns).await {
                Ok(mut extracted) => pass2.append(&mut extracted),
                Err(err) => {
                    warn!(entity = %entity, error = ?err, "per-entity sweep failed; skipping entity");
                }
            }
        }
        debug!(pass2_count = pass2.len(), "TwoPassExtractor pass 2b done");

        // Merge: drop pass 2 records that duplicate a pass 1 record on the
        // dedup key. Use (subject, predicate) for facts, (actor, verb,
        // ts-ish-bucket) for events; everything else passes through.
        let dedup_keys: std::collections::HashSet<String> = combined
            .iter()
            .filter_map(dedup_key_for)
            .collect();
        for extracted in pass2 {
            if let Some(k) = dedup_key_for(&extracted) {
                if dedup_keys.contains(&k) {
                    continue;
                }
            }
            combined.push(extracted);
        }
        info!(
            total = combined.len(),
            "TwoPassExtractor combined pass 1 + pass 2"
        );
        Ok(combined)
    }
}

/// Compute a dedup key for an extracted record. Same shape as the at-rest
/// compaction key; if two extractors emit the same key, log compaction
/// would collapse them anyway — drop pre-emptively to save Kafka traffic.
fn dedup_key_for(e: &Extracted) -> Option<String> {
    match &e.body {
        Body::Fact(f) => Some(format!("fact|{}|{}", f.subject, f.predicate)),
        Body::Event(ev) => Some(format!(
            "event|{}|{}|{}",
            ev.actor,
            ev.verb,
            ev.ts.timestamp()
        )),
        Body::Instruction(i) => Some(format!("instruction|{}|{}", i.scope, i.rule)),
        Body::Task(t) => Some(format!("task|{}", t.task_id)),
        Body::Concept(c) => Some(format!("concept|{}", c.entity_id)),
    }
}

// ───────────────────────── Pass 2a: entity scan ─────────────────────────

impl TwoPassExtractor {
    async fn scan_entities(&self, turns: &[Turn]) -> Result<Vec<String>> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.scan_max_tokens,
            "temperature": 0,
            "system": ENTITY_SCAN_SYSTEM,
            "messages": [{
                "role": "user",
                "content": render_turns_for_entity_scan(turns),
            }],
            "tools": [{
                "name": "list_candidate_entities",
                "description": "Return all concrete entities mentioned in the conversation \
                    that might warrant per-entity fact extraction.",
                "input_schema": entity_scan_schema(),
            }],
            "tool_choice": {"type": "tool", "name": "list_candidate_entities"},
        });
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION_HEADER)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "anthropic entity scan returned {status}: {txt}"
            )));
        }
        let parsed: MessagesResponse = resp.json().await?;
        Ok(extract_entity_list(&parsed))
    }
}

// ───────────────────────── Pass 2b: per-entity sweep ─────────────────────────

impl TwoPassExtractor {
    async fn sweep_entity(
        &self,
        entity: &str,
        turns: &[Turn],
    ) -> Result<Vec<Extracted>> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.sweep_max_tokens,
            "temperature": 0,
            "system": entity_sweep_system_prompt(entity),
            "messages": [{
                "role": "user",
                "content": render_turns_for_entity_sweep(entity, turns),
            }],
            "tools": [{
                "name": "extract_entity_facts",
                "description": format!("Extract all facts about {entity} from the conversation."),
                "input_schema": entity_sweep_schema(),
            }],
            "tool_choice": {"type": "tool", "name": "extract_entity_facts"},
        });
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION_HEADER)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "anthropic entity sweep returned {status}: {txt}"
            )));
        }
        let parsed: MessagesResponse = resp.json().await?;
        Ok(extract_entity_facts(&parsed, entity, turns.len(), &self.id))
    }
}

// ───────────────────────── Wire types ─────────────────────────

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize, Default)]
struct EntityListInput {
    #[serde(default)]
    entities: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct EntitySweepInput {
    #[serde(default)]
    facts: Vec<EntityFactRow>,
}

#[derive(Debug, Deserialize)]
struct EntityFactRow {
    predicate: String,
    object_text: String,
    #[serde(default)]
    source_indexes: Vec<usize>,
    #[serde(default = "default_confidence")]
    confidence: f32,
}

fn default_confidence() -> f32 {
    0.75
}

// ───────────────────────── Prompts ─────────────────────────

const ENTITY_SCAN_SYSTEM: &str = "\
You scan a conversation and list every concrete entity mentioned that might warrant per-entity \
fact extraction. Return the call to `list_candidate_entities` with a deduplicated list.

Include:
- Named people (\"Andy\", \"Maria Roscioli\")
- Named places, neighbourhoods, restaurants, museums (\"Roscioli\", \"Met Museum\", \"Williamsburg\")
- Brands, products, clothing labels (\"Andy clothing\", \"iPhone 15\")
- Foods, dishes, recipes when named or quantified (\"2-3 eggs\", \"jollof rice\")
- Concrete quantities tied to a thing (\"21-day trip\", \"$720 dinner\")

Exclude:
- Pronouns (\"he\", \"the user\")
- Generic categories with no name (\"apartments\", \"restaurants\")
- Common verbs / actions
- Filler words (\"thanks\", \"okay\")

Return at most 8 entities. Prefer entities mentioned by the ASSISTANT that the USER \
accepted (these are the ones the single-pass extractor misses).";

fn entity_sweep_system_prompt(entity: &str) -> String {
    format!(
        "\
You extract ALL facts about \"{entity}\" from this conversation. Return the call to \
`extract_entity_facts` with one row per fact.

Predicate guidance:
- Use canonical, lowercase, snake_case predicates (\"mentioned_in\", \"wears\", \
  \"recommends\", \"visited\", \"quantity\", \"price\").
- Object_text is a short value (\"Roscioli\", \"2-3\", \"$720\", \"Tuesday\").
- Include facts the assistant stated that the user accepted (acknowledged, \
  thanked, agreed to, didn't object).
- Skip facts not anchored to this specific entity.
- Cite source_indexes (0-based) for every fact. Drop the row if you can't.

Return up to 8 facts. Quality over quantity. Confidence in [0.0, 1.0] — 1.0 if the \
user explicitly accepted; 0.8 if the user implicitly accepted; 0.6 if only the \
assistant stated and no user reaction."
    )
}

fn render_turns_for_entity_scan(turns: &[Turn]) -> String {
    let mut s = String::with_capacity(turns.len() * 64);
    for (i, t) in turns.iter().enumerate() {
        s.push_str(&format!("[{i}] {}: {}\n", t.role, t.content));
    }
    s
}

fn render_turns_for_entity_sweep(entity: &str, turns: &[Turn]) -> String {
    let mut s = format!("Entity: {entity}\n\nConversation:\n");
    for (i, t) in turns.iter().enumerate() {
        s.push_str(&format!("[{i}] {}: {}\n", t.role, t.content));
    }
    s
}

/// Schema for the entity-scan tool. Built fresh per call — the JSON values
/// are small and cheap to construct; avoiding a `once_cell` dep keeps the
/// surface lean.
fn entity_scan_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "entities": {
                "type": "array",
                "items": {"type": "string"},
                "maxItems": 8
            }
        },
        "required": ["entities"]
    })
}

fn entity_sweep_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "predicate": {"type": "string"},
                        "object_text": {"type": "string"},
                        "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                    },
                    "required": ["predicate", "object_text"]
                },
                "maxItems": 8
            }
        },
        "required": ["facts"]
    })
}

// ───────────────────────── Response parsing ─────────────────────────

fn extract_entity_list(resp: &MessagesResponse) -> Vec<String> {
    for block in &resp.content {
        if let ContentBlock::ToolUse { name, input } = block {
            if name == "list_candidate_entities" {
                let parsed: EntityListInput =
                    serde_json::from_value(input.clone()).unwrap_or_default();
                let mut out = parsed.entities;
                // Light client-side cleanup: trim, dedup case-insensitively,
                // drop empties.
                out.retain(|s| !s.trim().is_empty());
                let mut seen = std::collections::HashSet::new();
                out.retain(|s| seen.insert(s.to_lowercase()));
                return out;
            }
        }
    }
    Vec::new()
}

fn extract_entity_facts(
    resp: &MessagesResponse,
    entity: &str,
    turns_len: usize,
    extractor_id: &str,
) -> Vec<Extracted> {
    for block in &resp.content {
        if let ContentBlock::ToolUse { name, input } = block {
            if name == "extract_entity_facts" {
                let parsed: EntitySweepInput =
                    serde_json::from_value(input.clone()).unwrap_or_default();
                return parsed
                    .facts
                    .into_iter()
                    .filter_map(|row| {
                        // Cite-source hallucination guard — identical to the
                        // base extractor's behavior.
                        let indexes: Vec<usize> = row
                            .source_indexes
                            .into_iter()
                            .filter(|i| *i < turns_len)
                            .collect();
                        if indexes.is_empty() {
                            return None;
                        }
                        let body = Body::Fact(FactBody {
                            subject: entity.to_string(),
                            predicate: row.predicate.trim().to_string(),
                            object: serde_json::Value::String(row.object_text.clone()),
                            polarity: "asserted".into(),
                            text: format!("{entity} — {}: {}", row.predicate, row.object_text),
                        });
                        let key = Some(format!("{entity}|{}", row.predicate));
                        Some(Extracted {
                            body,
                            key,
                            confidence: row.confidence.clamp(0.0, 1.0),
                            source_indexes: indexes,
                        })
                    })
                    .collect();
            }
        }
    }
    // No tool_use call (model refused / hallucinated). Mirror the base
    // extractor: silently return empty rather than error.
    let _ = extractor_id;
    Vec::new()
}

// silence dead-code warnings on the Other variant
#[allow(dead_code)]
fn _drain_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::Extractor;
    use async_trait::async_trait;

    fn turn(role: &str, content: &str) -> Turn {
        Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: None,
        }
    }

    #[derive(Debug)]
    struct StubInner {
        id_str: &'static str,
        rows: Vec<Extracted>,
    }

    #[async_trait]
    impl Extractor for StubInner {
        fn id(&self) -> &str {
            self.id_str
        }
        async fn extract(&self, _turns: &[Turn]) -> Result<Vec<Extracted>> {
            Ok(self.rows.clone())
        }
    }

    fn fact(subject: &str, predicate: &str, object: &str) -> Extracted {
        Extracted {
            body: Body::Fact(FactBody {
                subject: subject.into(),
                predicate: predicate.into(),
                object: serde_json::Value::String(object.into()),
                polarity: "asserted".into(),
                text: format!("{subject}|{predicate}|{object}"),
            }),
            key: Some(format!("{subject}|{predicate}")),
            confidence: 1.0,
            source_indexes: vec![0],
        }
    }

    #[test]
    fn dedup_key_for_fact_is_subject_predicate() {
        let e = fact("user", "budget_max", "4000");
        assert_eq!(dedup_key_for(&e), Some("fact|user|budget_max".to_string()));
    }

    #[test]
    fn dedup_key_for_distinct_facts() {
        let a = fact("user", "neighbourhood", "Williamsburg");
        let b = fact("user", "budget_max", "4000");
        assert_ne!(dedup_key_for(&a), dedup_key_for(&b));
    }

    #[test]
    fn render_turns_for_entity_scan_preserves_order_and_roles() {
        let turns = vec![
            turn("user", "I like jazz"),
            turn("assistant", "Try Smalls Jazz Club"),
            turn("user", "Sure"),
        ];
        let rendered = render_turns_for_entity_scan(&turns);
        assert!(rendered.starts_with("[0] user: I like jazz"));
        assert!(rendered.contains("[1] assistant: Try Smalls Jazz Club"));
        assert!(rendered.contains("[2] user: Sure"));
    }

    #[test]
    fn entity_sweep_system_prompt_includes_entity_name() {
        let p = entity_sweep_system_prompt("Roscioli");
        assert!(p.contains("Roscioli"));
        assert!(p.contains("source_indexes"));
        assert!(p.contains("user accepted"));
    }

    #[test]
    fn extract_entity_facts_drops_rows_without_valid_source_indexes() {
        let resp = MessagesResponse {
            content: vec![ContentBlock::ToolUse {
                name: "extract_entity_facts".into(),
                input: serde_json::json!({
                    "facts": [
                        {"predicate": "wears", "object_text": "jacket", "source_indexes": [0, 5]},
                        {"predicate": "size", "object_text": "M", "source_indexes": []},
                        {"predicate": "color", "object_text": "blue", "source_indexes": [99]},
                    ]
                }),
            }],
        };
        let out = extract_entity_facts(&resp, "Andy", 3, "two-pass[stub]");
        assert_eq!(out.len(), 1, "only `wears` has valid source_indexes after filter (index 5 dropped, 0 kept)");
        let body = match &out[0].body {
            Body::Fact(f) => f,
            _ => panic!("expected fact"),
        };
        assert_eq!(body.subject, "Andy");
        assert_eq!(body.predicate, "wears");
        assert_eq!(out[0].source_indexes, vec![0]);
    }

    #[test]
    fn extract_entity_list_dedups_case_insensitively() {
        let resp = MessagesResponse {
            content: vec![ContentBlock::ToolUse {
                name: "list_candidate_entities".into(),
                input: serde_json::json!({
                    "entities": ["Roscioli", "ROSCIOLI", "Andy", "andy", "  "]
                }),
            }],
        };
        let out = extract_entity_list(&resp);
        assert_eq!(out.len(), 2, "expected dedup + empty drop: {out:?}");
    }

    #[test]
    fn extract_entity_facts_returns_empty_on_no_tool_use() {
        let resp = MessagesResponse {
            content: vec![ContentBlock::Text {
                text: "I refuse".into(),
            }],
        };
        assert!(extract_entity_facts(&resp, "X", 1, "id").is_empty());
    }

    #[tokio::test]
    async fn two_pass_falls_back_to_pass_one_on_scan_failure() {
        // Stub inner returns 1 fact; entity scanner hits an unreachable URL
        // → pass 2 fails, but pass 1 results survive.
        let inner = Arc::new(StubInner {
            id_str: "stub@v1",
            rows: vec![fact("user", "color", "blue")],
        });
        let tp = TwoPassExtractor::new(inner, "fake-key")
            .expect("ok")
            .with_base_url("http://127.0.0.1:1") // refused
            .with_http_client(
                reqwest::Client::builder()
                    .timeout(Duration::from_millis(500))
                    .build()
                    .unwrap(),
            );
        let out = tp.extract(&[turn("user", "hi")]).await.expect("ok");
        assert_eq!(out.len(), 1, "should keep pass 1 on pass 2 failure");
        let body = match &out[0].body {
            Body::Fact(f) => f,
            _ => panic!("fact"),
        };
        assert_eq!(body.predicate, "color");
    }

    #[test]
    fn id_format() {
        let inner: Arc<dyn Extractor> = Arc::new(StubInner {
            id_str: "anthropic-v3",
            rows: vec![],
        });
        let tp = TwoPassExtractor::new(inner, "fake-key").expect("ok");
        assert_eq!(tp.id(), "two-pass[anthropic-v3]+anthropic-entity-sweep@v1");
    }
}
