//! Ollama native extractor — calls `/api/chat` with tool-use, distinct from
//! the OpenAI-compatible facade.
//!
//! Why a dedicated provider? Ollama exposes two HTTP surfaces:
//!
//! 1. **OpenAI-compatible** (`/v1/chat/completions`) — works with
//!    [`super::openai::OpenAIExtractor::for_local_server`], but historically
//!    has had spotty tool-call support across model and Ollama versions.
//! 2. **Native** (`/api/chat`) — first-class tool calling that follows the
//!    pattern documented at <https://ollama.com/blog/tool-support>; this is
//!    what we target here.
//!
//! Wire differences vs OpenAI worth knowing:
//! - Endpoint path is `/api/chat`, not `/v1/chat/completions`.
//! - No `Authorization` header in the default install (Ollama is local-only
//!   by default). [`OllamaExtractor::with_api_key`] is provided for proxies
//!   that front Ollama with auth (e.g. nginx + bearer).
//! - Response is a single `{message: ..., done: true}` envelope — no
//!   `choices[]` wrapper.
//! - Tool-call `arguments` is a JSON **object** by default (not a JSON-
//!   encoded string the way OpenAI returns it). We accept both shapes so the
//!   parser tolerates older builds.
//! - There is no `tool_choice` parameter — the model decides whether to call
//!   a tool. Phase-2 prompt v2 nudges it strongly enough in our testing, but
//!   the parser treats "no tool call" as "no extractions" rather than an
//!   error (same behaviour as OpenAI).

use crate::embeddings::TextGenerator;
use crate::error::{MemoryError, Result};
use crate::extractor::prompts::{v1, v2, v3, v4, v5};
use crate::extractor::providers::common::{filter_and_convert, RawToolInput};
use crate::extractor::{Extracted, Extractor, Turn};
use async_trait::async_trait;
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Stable extractor-version identifiers for provenance.
const ID_V1: &str = "ollama-v1";
const ID_V2: &str = "ollama-v2";
const ID_V3: &str = "ollama-v3";
const ID_V4: &str = "ollama-v4";
const ID_V5: &str = "ollama-v5";

/// Which prompt revision the extractor sends. Mirrors the OpenAI / Anthropic
/// extractors so a deployment can swap providers without touching downstream
/// fixtures or calibration tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaPromptVersion {
    /// Phase 1 prompt — fact + event only.
    V1,
    /// Phase 2 prompt — fact + event + instruction + task.
    V2,
    /// Phase 2 calibration prompt — predicate canonicalization +
    /// type-boundary tie-breakers.
    V3,
    /// Phase 2 actor-symmetry prompt — regressed LongMemEval-S by
    /// diluting user-fact density. Opt-in.
    V4,
    /// Phase 2/3 follow-up: v3 calibration + ADDITIVE concrete-noun
    /// extraction without trading user-fact density. Opt-in for
    /// measurement.
    V5,
}

impl OllamaPromptVersion {
    fn extractor_version(self) -> &'static str {
        match self {
            OllamaPromptVersion::V1 => ID_V1,
            OllamaPromptVersion::V2 => ID_V2,
            OllamaPromptVersion::V3 => ID_V3,
            OllamaPromptVersion::V4 => ID_V4,
            OllamaPromptVersion::V5 => ID_V5,
        }
    }
    fn system_prompt(self) -> &'static str {
        match self {
            OllamaPromptVersion::V1 => v1::SYSTEM_PROMPT,
            OllamaPromptVersion::V2 => v2::SYSTEM_PROMPT,
            OllamaPromptVersion::V3 => v3::SYSTEM_PROMPT,
            OllamaPromptVersion::V4 => v4::SYSTEM_PROMPT,
            OllamaPromptVersion::V5 => v5::SYSTEM_PROMPT,
        }
    }
    fn tool_input_schema(self) -> serde_json::Value {
        match self {
            OllamaPromptVersion::V1 => v1::tool_input_schema(),
            OllamaPromptVersion::V2 => v2::tool_input_schema(),
            OllamaPromptVersion::V3 => v3::tool_input_schema(),
            OllamaPromptVersion::V4 => v4::tool_input_schema(),
            OllamaPromptVersion::V5 => v5::tool_input_schema(),
        }
    }
    fn render_user_message(self, turns: &[Turn]) -> String {
        match self {
            OllamaPromptVersion::V1 => v1::render_user_message(turns),
            OllamaPromptVersion::V2 => v2::render_user_message(turns),
            OllamaPromptVersion::V3 => v3::render_user_message(turns),
            OllamaPromptVersion::V4 => v4::render_user_message(turns),
            OllamaPromptVersion::V5 => v5::render_user_message(turns),
        }
    }
    fn tool_name(self) -> &'static str {
        v1::TOOL_NAME
    }
}

/// Native-Ollama extractor. Cheap to clone — wraps an `Arc`'d HTTP client.
#[derive(Debug, Clone)]
pub struct OllamaExtractor {
    model: String,
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
    prompt_version: OllamaPromptVersion,
}

impl OllamaExtractor {
    /// Build with the model id. Default base URL `http://localhost:11434`,
    /// default prompt version V2.
    ///
    /// `model` is whatever Ollama tag the server has pulled, e.g. `llama3.1`,
    /// `qwen2.5:14b`, `llama3.1:70b-instruct-q4_K_M`. The model **must**
    /// support tool calling on the Ollama side — see
    /// <https://ollama.com/search?c=tools>.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: None,
            http: reqwest::Client::new(),
            prompt_version: OllamaPromptVersion::V3,
        }
    }

    /// Override the base URL (e.g. `http://ollama-1.internal:11434` or
    /// `https://ollama.example.com` if fronted by a TLS proxy). Path
    /// `/api/chat` is appended automatically.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Optional bearer token — only needed if Ollama is fronted by a proxy
    /// that enforces auth. Leave unset for the default local install.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override the prompt version. Default is V2.
    pub fn with_prompt_version(mut self, v: OllamaPromptVersion) -> Self {
        self.prompt_version = v;
        self
    }

    /// Inject a custom HTTP client (e.g. for custom timeouts or proxies).
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http = client;
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }
}

#[async_trait]
impl TextGenerator for OllamaExtractor {
    fn id(&self) -> &str {
        self.prompt_version.extractor_version()
    }

    /// Free-form completion for HyDE. No tools — sends a single user message
    /// and reads back `message.content`.
    async fn complete(&self, prompt: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "stream": false,
            "messages": [{"role": "user", "content": prompt}]
        });
        let resp = self
            .apply_auth(self.http.post(self.chat_url()))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let s = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "ollama completion {s}: {txt}"
            )));
        }
        let parsed: ChatResponse = resp.json().await?;
        Ok(parsed.message.content.unwrap_or_default())
    }
}

#[async_trait]
impl Extractor for OllamaExtractor {
    fn id(&self) -> &str {
        self.prompt_version.extractor_version()
    }

    async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>> {
        if turns.is_empty() {
            return Ok(vec![]);
        }
        let body = build_request_body(&self.model, turns, self.prompt_version);

        let resp = self
            .apply_auth(self.http.post(self.chat_url()))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "ollama /api/chat returned {status}: {txt}"
            )));
        }

        let parsed: ChatResponse = resp.json().await?;
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
    turns: &[Turn],
    pv: OllamaPromptVersion,
) -> serde_json::Value {
    // `temperature: 0` (under `options`) is critical for extractor
    // determinism. Ollama accepts model parameters under the `options`
    // object; setting `temperature: 0` matches the determinism contract
    // we apply to Anthropic and OpenAI.
    serde_json::json!({
        "model": model,
        // `stream: false` — we want a single response object, not NDJSON.
        "stream": false,
        "options": {"temperature": 0},
        "messages": [
            {"role": "system", "content": pv.system_prompt()},
            {"role": "user", "content": pv.render_user_message(turns)}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": pv.tool_name(),
                    "description": "Record extracted memories from the conversation. Call exactly once.",
                    "parameters": pv.tool_input_schema()
                }
            }
        ]
    })
}

// ---- response parsing ----

#[derive(Debug, Deserialize)]
struct ChatResponse {
    message: AssistantMessage,
    #[allow(dead_code)]
    #[serde(default)]
    done: bool,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    #[serde(default)]
    function: Option<FunctionCall>,
}

#[derive(Debug, Deserialize)]
struct FunctionCall {
    #[serde(default)]
    name: Option<String>,
    /// Ollama returns `arguments` as a JSON **object** by default, but some
    /// versions / OpenAI-compat-shimming setups return a string. We tolerate
    /// both via `serde_json::Value`.
    #[serde(default)]
    arguments: serde_json::Value,
}

fn parse_tool_output(
    resp: &ChatResponse,
    pv: OllamaPromptVersion,
) -> Result<RawToolInput> {
    let want_name = pv.tool_name();
    for tc in &resp.message.tool_calls {
        let func = match &tc.function {
            Some(f) => f,
            None => continue,
        };
        if func.name.as_deref() != Some(want_name) {
            continue;
        }
        return parse_arguments_value(&func.arguments);
    }
    // No matching tool call — treat as empty (model refused or returned text only).
    Ok(RawToolInput::default())
}

fn parse_arguments_value(v: &serde_json::Value) -> Result<RawToolInput> {
    match v {
        // Object — happy path.
        serde_json::Value::Object(_) => serde_json::from_value(v.clone()).map_err(|e| {
            MemoryError::Provider(format!(
                "ollama tool-call arguments failed schema: {e}"
            ))
        }),
        // String — older builds / OpenAI-compat shims; parse the inner JSON.
        serde_json::Value::String(s) if !s.is_empty() => {
            serde_json::from_str(s).map_err(|e| {
                MemoryError::Provider(format!(
                    "ollama tool-call arguments (string-shape) failed schema: {e}"
                ))
            })
        }
        // Null / empty — model didn't fill the tool — treat as no extractions.
        _ => Ok(RawToolInput::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
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

    /// Ollama-native shape: `arguments` is a JSON object.
    fn fake_response_object_args(arguments: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "model": "llama3.1",
            "created_at": "2026-04-25T12:00:00Z",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": v1::TOOL_NAME,
                        "arguments": arguments
                    }
                }]
            },
            "done": true
        })
    }

    /// Older / OpenAI-compat shim shape: `arguments` is a JSON-encoded string.
    fn fake_response_string_args(arguments: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "model": "llama3.1",
            "created_at": "2026-04-25T12:00:00Z",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": v1::TOOL_NAME,
                        "arguments": arguments.to_string()
                    }
                }]
            },
            "done": true
        })
    }

    #[test]
    fn build_request_includes_record_memories_tool() {
        let body = build_request_body(
            "llama3.1",
            &[turn("user", "hi")],
            OllamaPromptVersion::V2,
        );
        assert_eq!(body["model"], "llama3.1");
        assert_eq!(body["stream"], false);
        assert_eq!(body["tools"][0]["function"]["name"], v1::TOOL_NAME);
        assert!(body["tools"][0]["function"]["parameters"].is_object());
        // Ollama has no tool_choice — confirm we don't add one.
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn parse_tool_output_accepts_object_arguments() {
        let resp_json = fake_response_object_args(serde_json::json!({
            "facts": [{
                "subject": "user:luis",
                "predicate": "prefers",
                "object": "Lapa",
                "text": "Luis prefers Lapa",
                "source_indexes": [0],
                "confidence": 0.9
            }],
            "events": [],
            "instructions": [],
            "tasks": []
        }));
        let resp: ChatResponse = serde_json::from_value(resp_json).unwrap();
        let raw = parse_tool_output(&resp, OllamaPromptVersion::V2).unwrap();
        assert_eq!(raw.facts.len(), 1);
        assert_eq!(raw.facts[0].predicate.as_deref(), Some("prefers"));
    }

    #[test]
    fn parse_tool_output_accepts_string_arguments() {
        let resp_json = fake_response_string_args(serde_json::json!({
            "facts": [{
                "subject": "user",
                "predicate": "language",
                "object": "pt-BR",
                "text": "user speaks Brazilian Portuguese",
                "source_indexes": [0],
                "confidence": 0.85
            }],
            "events": [],
            "instructions": [],
            "tasks": []
        }));
        let resp: ChatResponse = serde_json::from_value(resp_json).unwrap();
        let raw = parse_tool_output(&resp, OllamaPromptVersion::V2).unwrap();
        assert_eq!(raw.facts.len(), 1);
        assert_eq!(raw.facts[0].predicate.as_deref(), Some("language"));
    }

    #[test]
    fn parse_tool_output_empty_when_no_tool_call() {
        let resp: ChatResponse = serde_json::from_value(serde_json::json!({
            "model": "llama3.1",
            "message": {"role": "assistant", "content": "I cannot extract."},
            "done": true
        }))
        .unwrap();
        let raw = parse_tool_output(&resp, OllamaPromptVersion::V2).unwrap();
        assert!(raw.facts.is_empty());
        assert!(raw.events.is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_null() {
        let raw = parse_arguments_value(&serde_json::Value::Null).unwrap();
        assert!(raw.facts.is_empty());
    }

    #[tokio::test]
    async fn happy_path_against_wiremock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                fake_response_object_args(serde_json::json!({
                    "facts": [{
                        "subject": "user",
                        "predicate": "max_budget_eur",
                        "object": 800000,
                        "text": "User has max budget 800k EUR",
                        "source_indexes": [0],
                        "confidence": 0.9
                    }],
                    "events": [],
                    "instructions": [],
                    "tasks": []
                })),
            ))
            .mount(&server)
            .await;

        let extractor = OllamaExtractor::new("llama3.1").with_base_url(server.uri());
        let out = extractor
            .extract(&[turn("user", "max 800k euros")])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(<OllamaExtractor as Extractor>::id(&extractor), "ollama-v3");
    }

    #[tokio::test]
    async fn http_4xx_becomes_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(404).set_body_string("model not found"),
            )
            .mount(&server)
            .await;
        let extractor = OllamaExtractor::new("does-not-exist").with_base_url(server.uri());
        let err = extractor.extract(&[turn("user", "hi")]).await.unwrap_err();
        assert!(matches!(err, MemoryError::Provider(_)), "got: {:?}", err);
    }

    #[tokio::test]
    async fn empty_batch_skips_http_call() {
        let extractor =
            OllamaExtractor::new("llama3.1").with_base_url("http://127.0.0.1:1");
        assert!(extractor.extract(&[]).await.unwrap().is_empty());
    }

    #[test]
    fn id_changes_with_prompt_version() {
        let default = OllamaExtractor::new("llama3.1");
        // V3 is the empirical baseline default. V4 (actor-symmetry) opt-in.
        assert_eq!(<OllamaExtractor as Extractor>::id(&default), ID_V3);
        let v4 = OllamaExtractor::new("llama3.1").with_prompt_version(OllamaPromptVersion::V4);
        assert_eq!(<OllamaExtractor as Extractor>::id(&v4), ID_V4);
        let v2 = OllamaExtractor::new("llama3.1").with_prompt_version(OllamaPromptVersion::V2);
        assert_eq!(<OllamaExtractor as Extractor>::id(&v2), ID_V2);
        let v1 = OllamaExtractor::new("llama3.1").with_prompt_version(OllamaPromptVersion::V1);
        assert_eq!(<OllamaExtractor as Extractor>::id(&v1), ID_V1);
    }

    #[test]
    fn with_api_key_attaches_bearer_when_set() {
        let e = OllamaExtractor::new("m").with_api_key("secret");
        assert_eq!(e.api_key.as_deref(), Some("secret"));
    }

    #[tokio::test]
    async fn with_api_key_sends_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(wiremock::matchers::header("authorization", "Bearer s3cr3t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_response_object_args(
                serde_json::json!({
                    "facts": [],
                    "events": [],
                    "instructions": [],
                    "tasks": []
                }),
            )))
            .mount(&server)
            .await;
        let extractor = OllamaExtractor::new("m")
            .with_base_url(server.uri())
            .with_api_key("s3cr3t");
        let _ = extractor.extract(&[turn("user", "hi")]).await.unwrap();
    }
}
