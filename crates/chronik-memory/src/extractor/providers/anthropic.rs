//! Anthropic Messages API extractor — calls Claude with `tool_choice` forcing
//! a single `record_memories` invocation, parses the structured output, and
//! verifies that all `source_indexes` actually point at valid input turns.
//!
//! Hallucination guard: extractions whose cited indexes are out-of-range are
//! dropped with a warning. This is the SDK's primary defence against the model
//! making up sources.

use crate::embeddings::TextGenerator;
use crate::error::{MemoryError, Result};
use crate::extractor::prompts::{v1, v2, v3, v4, v5};
use crate::extractor::providers::common::{filter_and_convert, RawToolInput};
use crate::extractor::{Extracted, Extractor, Turn};
use async_trait::async_trait;
use serde::Deserialize;

const ANTHROPIC_VERSION_HEADER: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Which prompt revision the extractor will send.
///
/// v1 emits fact + event only (Phase 1 baseline). v2 emits all four memory
/// types (Phase 2, AMS-2.1). v3 keeps v2's schema and adds predicate
/// canonicalization + type-boundary tie-breakers (AMS-2.8 calibration).
/// v4 — **the new default** — keeps everything v3 fixed and adds
/// actor-symmetry: facts can be about any entity, not just the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptVersion {
    /// Phase 1 prompt — fact + event only.
    V1,
    /// Phase 2 prompt — fact + event + instruction + task.
    V2,
    /// Phase 2 calibration prompt — same schema as V2 with predicate
    /// canonicalization + type-boundary tie-breakers.
    V3,
    /// Phase 2 actor-symmetry prompt — same schema as V3 with explicit
    /// rules that facts may be about any entity. Regressed LongMemEval-S
    /// in May 2026 (judge_rate 0.389 → 0.222) by diluting user-facts.
    /// Opt-in only.
    V4,
    /// Phase 2 / 3 follow-up: v3 calibration + **additive** concrete-noun
    /// extraction for third-party entities (named places, foods, brands,
    /// quantities, physical descriptions). Designed to fix v4's
    /// regression by NOT trading user-fact density for third-party
    /// coverage. Opt-in until measured against v3 baseline.
    V5,
}

impl PromptVersion {
    fn extractor_version(self) -> &'static str {
        match self {
            PromptVersion::V1 => v1::EXTRACTOR_VERSION,
            PromptVersion::V2 => v2::EXTRACTOR_VERSION,
            PromptVersion::V3 => v3::EXTRACTOR_VERSION,
            PromptVersion::V4 => v4::EXTRACTOR_VERSION,
            PromptVersion::V5 => v5::EXTRACTOR_VERSION,
        }
    }

    fn system_prompt(self) -> &'static str {
        match self {
            PromptVersion::V1 => v1::SYSTEM_PROMPT,
            PromptVersion::V2 => v2::SYSTEM_PROMPT,
            PromptVersion::V3 => v3::SYSTEM_PROMPT,
            PromptVersion::V4 => v4::SYSTEM_PROMPT,
            PromptVersion::V5 => v5::SYSTEM_PROMPT,
        }
    }

    fn tool_input_schema(self) -> serde_json::Value {
        match self {
            PromptVersion::V1 => v1::tool_input_schema(),
            PromptVersion::V2 => v2::tool_input_schema(),
            PromptVersion::V3 => v3::tool_input_schema(),
            PromptVersion::V4 => v4::tool_input_schema(),
            PromptVersion::V5 => v5::tool_input_schema(),
        }
    }

    fn render_user_message(self, turns: &[Turn]) -> String {
        match self {
            PromptVersion::V1 => v1::render_user_message(turns),
            PromptVersion::V2 => v2::render_user_message(turns),
            PromptVersion::V3 => v3::render_user_message(turns),
            PromptVersion::V4 => v4::render_user_message(turns),
            PromptVersion::V5 => v5::render_user_message(turns),
        }
    }

    fn tool_name(self) -> &'static str {
        // All versions use the same tool name.
        v1::TOOL_NAME
    }
}

/// Calls Claude (default Haiku 4.5) via the Messages API with forced tool-use.
///
/// Cheap to clone — wraps an `Arc`'d HTTP client.
#[derive(Debug, Clone)]
pub struct AnthropicExtractor {
    api_key: String,
    model: String,
    base_url: String,
    http: reqwest::Client,
    max_tokens: u32,
    prompt_version: PromptVersion,
}

impl AnthropicExtractor {
    /// Build a new extractor.
    ///
    /// Defaults: model `claude-haiku-4-5`, base URL `https://api.anthropic.com`,
    /// max_tokens 4096. Override via [`with_model`](Self::with_model) /
    /// [`with_base_url`](Self::with_base_url) / [`with_max_tokens`](Self::with_max_tokens).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: "claude-haiku-4-5".to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: reqwest::Client::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            prompt_version: PromptVersion::V3,
        }
    }

    /// Override the prompt version. Default is [`PromptVersion::V3`] —
    /// the calibration baseline. V4 (actor-symmetry) is opt-in: it lifts
    /// the `single-session-assistant` LongMemEval-S category from 0/3 to
    /// 1/3 but regresses `single-session-user` (-2) and
    /// `temporal-reasoning` (-1) on the 18-item balanced pilot
    /// (judge_rate 0.389 → 0.222). Use V4 when third-party / agent /
    /// assistant utterance recall matters more than user-fact density;
    /// V3 is the empirically better default. V2 is pre-calibration parity;
    /// V1 is the Phase 1 fact+event-only prompt.
    pub fn with_prompt_version(mut self, v: PromptVersion) -> Self {
        self.prompt_version = v;
        self
    }

    /// Override the model identifier.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Override the API base URL — used for testing with a mock server.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Override `max_tokens` for the response (default 4096).
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Inject a custom HTTP client (e.g. one with custom timeouts).
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http = client;
        self
    }
}

#[async_trait]
impl TextGenerator for AnthropicExtractor {
    fn id(&self) -> &str {
        self.prompt_version.extractor_version()
    }

    /// Free-form completion for HyDE. Bypasses the tool-use machinery — sends
    /// the prompt as a normal user message and reads back the assistant's
    /// concatenated text content blocks.
    async fn complete(&self, prompt: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": prompt}]
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
        if !resp.status().is_success() {
            let s = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "anthropic completion {s}: {txt}"
            )));
        }
        let parsed: MessagesResponse = resp.json().await?;
        let mut out = String::new();
        for block in &parsed.content {
            if let ContentBlock::Text { text } = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Extractor for AnthropicExtractor {
    fn id(&self) -> &str {
        // Stable identifier for provenance — driven by prompt version, not the
        // model id (so re-running with a different Claude model under the same
        // prompt doesn't invalidate existing memories).
        self.prompt_version.extractor_version()
    }

    async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>> {
        if turns.is_empty() {
            return Ok(vec![]);
        }
        let body = build_request_body(
            &self.model,
            self.max_tokens,
            turns,
            self.prompt_version,
        );
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
                "anthropic returned {status}: {txt}"
            )));
        }

        let parsed: MessagesResponse = resp.json().await?;
        let raw = parse_tool_output(&parsed, self.prompt_version)?;
        Ok(filter_and_convert(
            raw,
            turns.len(),
            self.prompt_version.extractor_version(),
        ))
    }
}

// ---- request body ----

fn build_request_body(
    model: &str,
    max_tokens: u32,
    turns: &[Turn],
    pv: PromptVersion,
) -> serde_json::Value {
    // `temperature: 0` is critical for extractor determinism — at the API
    // default (1.0) the same conversation can yield different predicate
    // names across runs (`orientation_preference` vs
    // `apartment_orientation_preference`), which destroys downstream key
    // stability and recall calibration. Anthropic accepts 0..=1; we pin to 0.
    serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "temperature": 0,
        "system": pv.system_prompt(),
        "messages": [
            {"role": "user", "content": pv.render_user_message(turns)}
        ],
        "tools": [
            {
                "name": pv.tool_name(),
                "description": "Record extracted memories from the conversation. Call exactly once.",
                "input_schema": pv.tool_input_schema()
            }
        ],
        "tool_choice": {"type": "tool", "name": pv.tool_name()}
    })
}

// ---- response parsing ----

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    /// Anthropic returns content as an array of blocks; we look for `tool_use`.
    content: Vec<ContentBlock>,
    #[allow(dead_code)]
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "tool_use")]
    ToolUse {
        #[allow(dead_code)]
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "text")]
    Text {
        #[allow(dead_code)]
        text: String,
    },
    #[serde(other)]
    Other,
}

/// Pull the `tool_use` block out of the response; return its `input`.
/// Raw struct types and `filter_and_convert` come from
/// [`crate::extractor::providers::common`] so this provider only owns the
/// Anthropic-specific tool_use unwrapping.
fn parse_tool_output(
    resp: &MessagesResponse,
    pv: PromptVersion,
) -> Result<RawToolInput> {
    let want_name = pv.tool_name();
    for block in &resp.content {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            if name == want_name {
                return serde_json::from_value(input.clone()).map_err(|e| {
                    MemoryError::Provider(format!(
                        "anthropic tool_use input failed schema: {e}"
                    ))
                });
            }
        }
    }
    // No tool_use block — model refused or hallucinated. Treat as empty.
    Ok(RawToolInput::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn turn(role: &str, content: &str) -> Turn {
        Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: None,
        }
    }

    fn fake_response_with_tool_use(input: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "msg_abc",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-4-5",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": v1::TOOL_NAME,
                    "input": input
                }
            ],
            "stop_reason": "tool_use"
        })
    }

    #[test]
    fn request_body_uses_tool_choice_force_v2() {
        let body = build_request_body(
            "claude-haiku-4-5",
            1024,
            &[turn("user", "hi")],
            PromptVersion::V2,
        );
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], v1::TOOL_NAME);
        assert_eq!(body["tools"][0]["name"], v1::TOOL_NAME);
        // v2 schema includes all four arrays.
        let req = body["tools"][0]["input_schema"]["required"]
            .as_array()
            .unwrap();
        let names: Vec<_> = req.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"facts"));
        assert!(names.contains(&"events"));
        assert!(names.contains(&"instructions"));
        assert!(names.contains(&"tasks"));
    }

    #[test]
    fn request_body_v1_only_facts_events() {
        let body = build_request_body(
            "claude-haiku-4-5",
            1024,
            &[turn("user", "hi")],
            PromptVersion::V1,
        );
        let req = body["tools"][0]["input_schema"]["required"]
            .as_array()
            .unwrap();
        let names: Vec<_> = req.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"facts"));
        assert!(names.contains(&"events"));
        assert!(!names.contains(&"instructions"));
        assert!(!names.contains(&"tasks"));
    }

    // The shared `filter_and_convert` / raw-struct conversions are tested in
    // `super::common::tests`. This module only tests the Anthropic-specific
    // wire format: tool-choice forcing, content-block tool_use parsing, refusal
    // handling, and the wiremock end-to-end happy path.

    #[test]
    fn parse_tool_output_picks_tool_use_block() {
        let resp_json = fake_response_with_tool_use(serde_json::json!({
            "facts": [{
                "subject": "user:luis",
                "predicate": "prefers",
                "object": "Lapa",
                "text": "Luis prefers Lapa",
                "source_indexes": [0],
                "confidence": 0.9
            }],
            "events": []
        }));
        let resp: MessagesResponse = serde_json::from_value(resp_json).unwrap();
        let raw = parse_tool_output(&resp, PromptVersion::V2).unwrap();
        assert_eq!(raw.facts.len(), 1);
        assert_eq!(raw.facts[0].predicate.as_deref(), Some("prefers"));
    }

    #[test]
    fn parse_tool_output_returns_empty_when_no_tool_use() {
        // Model refused or sent only text.
        let resp: MessagesResponse = serde_json::from_value(serde_json::json!({
            "id": "msg_x",
            "type": "message",
            "role": "assistant",
            "model": "x",
            "content": [{"type": "text", "text": "I cannot extract anything."}]
        }))
        .unwrap();
        let raw = parse_tool_output(&resp, PromptVersion::V2).unwrap();
        assert!(raw.facts.is_empty());
        assert!(raw.events.is_empty());
        assert!(raw.instructions.is_empty());
        assert!(raw.tasks.is_empty());
    }

    #[tokio::test]
    async fn happy_path_against_wiremock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_VERSION_HEADER))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                fake_response_with_tool_use(serde_json::json!({
                    "facts": [{
                        "subject": "user:luis",
                        "predicate": "seeks_property_type",
                        "object": "T3",
                        "text": "Luis seeks a T3",
                        "source_indexes": [0],
                        "confidence": 0.95
                    }],
                    "events": [{
                        "actor": "user:luis",
                        "verb": "stated_budget",
                        "object": "800000",
                        "context": "max budget in EUR",
                        "ts": "2026-04-25T10:00:00Z",
                        "source_indexes": [0],
                        "confidence": 0.9
                    }]
                })),
            ))
            .mount(&server)
            .await;

        let extractor = AnthropicExtractor::new("test-key").with_base_url(server.uri());
        let out = extractor
            .extract(&[turn(
                "user",
                "I want a T3 in Lapa, max 800k euros.",
            )])
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        // Default prompt version is v3; id reflects that.
        assert_eq!(<AnthropicExtractor as Extractor>::id(&extractor), v3::EXTRACTOR_VERSION);
    }

    #[tokio::test]
    async fn id_changes_with_prompt_version() {
        let default = AnthropicExtractor::new("k");
        // V3 is the empirical baseline default — V4 (actor-symmetry)
        // regressed LongMemEval-S judge_rate 0.389 → 0.222 on the 18-item
        // balanced pilot. V4 still available via `with_prompt_version`.
        assert_eq!(<AnthropicExtractor as Extractor>::id(&default), v3::EXTRACTOR_VERSION);
        let v4 = AnthropicExtractor::new("k").with_prompt_version(PromptVersion::V4);
        assert_eq!(<AnthropicExtractor as Extractor>::id(&v4), v4::EXTRACTOR_VERSION);
        let v2 = AnthropicExtractor::new("k").with_prompt_version(PromptVersion::V2);
        assert_eq!(<AnthropicExtractor as Extractor>::id(&v2), v2::EXTRACTOR_VERSION);
        let v1 = AnthropicExtractor::new("k").with_prompt_version(PromptVersion::V1);
        assert_eq!(<AnthropicExtractor as Extractor>::id(&v1), v1::EXTRACTOR_VERSION);
    }

    #[tokio::test]
    async fn http_4xx_becomes_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("{\"error\":\"unauthorized\"}"),
            )
            .mount(&server)
            .await;

        let extractor = AnthropicExtractor::new("bad-key").with_base_url(server.uri());
        let err = extractor
            .extract(&[turn("user", "hi")])
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::Provider(_)), "got: {:?}", err);
    }

    #[tokio::test]
    async fn empty_batch_skips_http_call() {
        // No mock registered — if extract makes a call, it'll fail.
        let extractor =
            AnthropicExtractor::new("test-key").with_base_url("http://127.0.0.1:1");
        let out = extractor.extract(&[]).await.unwrap();
        assert!(out.is_empty());
    }
}
