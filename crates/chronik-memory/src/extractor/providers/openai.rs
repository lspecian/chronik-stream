//! OpenAI Chat Completions extractor — calls any OpenAI-compatible endpoint
//! with `tool_choice` forcing a single `record_memories` invocation.
//!
//! Covers:
//! - **OpenAI** itself (default base URL `https://api.openai.com`)
//! - **vLLM** OpenAI-compatible server (any URL via [`OpenAIExtractor::with_base_url`])
//! - **LM Studio**, **llama.cpp HTTP server**, **OpenRouter**, anything else
//!   that speaks the OpenAI Chat Completions API + tool calling
//!
//! Sharing-by-design: parsing of the tool-call output reuses the provider-
//! agnostic [`crate::extractor::providers::common`] pipeline, so calibration,
//! source-cite verification, and the [`RawToolInput`] schema are identical to
//! the Anthropic extractor.

use crate::embeddings::TextGenerator;
use crate::error::{MemoryError, Result};
use crate::extractor::prompts::{v1, v2, v3, v4, v5};
use crate::extractor::providers::common::{filter_and_convert, RawToolInput};
use crate::extractor::{Extracted, Extractor, Turn};
use async_trait::async_trait;
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Stable extractor-version identifiers for provenance. Distinct from the
/// Anthropic ids so memories carry a marker of which provider produced them.
const ID_V1: &str = "openai-v1";
const ID_V2: &str = "openai-v2";
const ID_V3: &str = "openai-v3";
const ID_V4: &str = "openai-v4";
const ID_V5: &str = "openai-v5";

/// Which prompt revision the extractor sends. Same shape as Anthropic — only
/// the wire-format wrapping differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAIPromptVersion {
    /// Phase 1 prompt — fact + event only.
    V1,
    /// Phase 2 prompt — fact + event + instruction + task.
    V2,
    /// Phase 2 calibration prompt — same schema as V2 with predicate
    /// canonicalization + type-boundary tie-breakers.
    V3,
    /// Phase 2 actor-symmetry prompt — regressed LongMemEval-S in May
    /// 2026 by diluting user-fact density. Opt-in.
    V4,
    /// Phase 2/3 follow-up: v3 calibration + ADDITIVE concrete-noun
    /// extraction (named places, foods, brands, quantities, physical
    /// descriptions) without trading user-fact density. Opt-in for
    /// measurement.
    V5,
}

impl OpenAIPromptVersion {
    fn extractor_version(self) -> &'static str {
        match self {
            OpenAIPromptVersion::V1 => ID_V1,
            OpenAIPromptVersion::V2 => ID_V2,
            OpenAIPromptVersion::V3 => ID_V3,
            OpenAIPromptVersion::V4 => ID_V4,
            OpenAIPromptVersion::V5 => ID_V5,
        }
    }
    fn system_prompt(self) -> &'static str {
        match self {
            OpenAIPromptVersion::V1 => v1::SYSTEM_PROMPT,
            OpenAIPromptVersion::V2 => v2::SYSTEM_PROMPT,
            OpenAIPromptVersion::V3 => v3::SYSTEM_PROMPT,
            OpenAIPromptVersion::V4 => v4::SYSTEM_PROMPT,
            OpenAIPromptVersion::V5 => v5::SYSTEM_PROMPT,
        }
    }
    fn tool_input_schema(self) -> serde_json::Value {
        match self {
            OpenAIPromptVersion::V1 => v1::tool_input_schema(),
            OpenAIPromptVersion::V2 => v2::tool_input_schema(),
            OpenAIPromptVersion::V3 => v3::tool_input_schema(),
            OpenAIPromptVersion::V4 => v4::tool_input_schema(),
            OpenAIPromptVersion::V5 => v5::tool_input_schema(),
        }
    }
    fn render_user_message(self, turns: &[Turn]) -> String {
        match self {
            OpenAIPromptVersion::V1 => v1::render_user_message(turns),
            OpenAIPromptVersion::V2 => v2::render_user_message(turns),
            OpenAIPromptVersion::V3 => v3::render_user_message(turns),
            OpenAIPromptVersion::V4 => v4::render_user_message(turns),
            OpenAIPromptVersion::V5 => v5::render_user_message(turns),
        }
    }
    fn tool_name(self) -> &'static str {
        v1::TOOL_NAME // shared across versions and providers
    }
}

/// Calls an OpenAI-compatible Chat Completions endpoint with forced tool use.
///
/// Cheap to clone — wraps an `Arc`'d HTTP client.
#[derive(Debug, Clone)]
pub struct OpenAIExtractor {
    api_key: String,
    model: String,
    base_url: String,
    http: reqwest::Client,
    max_tokens: u32,
    prompt_version: OpenAIPromptVersion,
}

impl OpenAIExtractor {
    /// Build with an OpenAI API key. Default model `gpt-4o-mini`, default base
    /// URL `https://api.openai.com`.
    ///
    /// For vLLM / LM Studio / llama.cpp endpoints, override via
    /// [`with_base_url`](Self::with_base_url) and supply whatever model id
    /// that server expects (typically the loaded model's HF id).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: "gpt-4o-mini".to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: reqwest::Client::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            prompt_version: OpenAIPromptVersion::V3,
        }
    }

    /// Override the model identifier (e.g. `gpt-4o`, `gpt-4.1-mini`,
    /// `Qwen/Qwen3-7B-Instruct` for a vLLM endpoint).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Override the base URL — point at vLLM, LM Studio, OpenRouter, etc.
    /// Path `/v1/chat/completions` is appended automatically.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Override `max_tokens` for the response (default 4096).
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Override the prompt version. Default is V2.
    pub fn with_prompt_version(mut self, v: OpenAIPromptVersion) -> Self {
        self.prompt_version = v;
        self
    }

    /// Inject a custom HTTP client (e.g. with custom timeouts or proxies).
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http = client;
        self
    }

    /// **Convenience for self-hosted OpenAI-compatible servers** (vLLM, LM
    /// Studio, etc.) where the API key is irrelevant or set to a placeholder.
    pub fn for_local_server(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new("local")
            .with_base_url(base_url)
            .with_model(model)
    }
}

#[async_trait]
impl TextGenerator for OpenAIExtractor {
    fn id(&self) -> &str {
        self.prompt_version.extractor_version()
    }

    /// Free-form completion for HyDE. No tool use — sends the prompt as a
    /// single user message and reads back `choices[0].message.content`.
    async fn complete(&self, prompt: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": prompt}]
        });
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let s = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "openai completion {s}: {txt}"
            )));
        }
        let parsed: ChatCompletionsResponse = resp.json().await?;
        Ok(parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default())
    }
}

#[async_trait]
impl Extractor for OpenAIExtractor {
    fn id(&self) -> &str {
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
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "openai-compatible endpoint returned {status}: {txt}"
            )));
        }

        let parsed: ChatCompletionsResponse = resp.json().await?;
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
    pv: OpenAIPromptVersion,
) -> serde_json::Value {
    // `temperature: 0` is critical for extractor determinism — at the API
    // default (1.0) the same conversation yields different predicate names
    // across runs, breaking key stability and downstream recall calibration.
    serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "temperature": 0,
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
        ],
        // `"required"` (string) works on real OpenAI *and* on self-hosted
        // OpenAI-compat servers (LM Studio, some vLLM builds) that reject the
        // object form `{"type": "function", "function": {"name": ...}}`.
        // Since we only expose one tool in `tools`, "required" is equivalent
        // to forcing that specific tool.
        "tool_choice": "required"
    })
}

// ---- response parsing ----

#[derive(Debug, Deserialize)]
struct ChatCompletionsResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
    #[allow(dead_code)]
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    #[allow(dead_code)]
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionCall>,
}

#[derive(Debug, Deserialize)]
struct FunctionCall {
    #[serde(default)]
    name: Option<String>,
    /// OpenAI returns `arguments` as a JSON-encoded string.
    #[serde(default)]
    arguments: Option<String>,
}

fn parse_tool_output(
    resp: &ChatCompletionsResponse,
    pv: OpenAIPromptVersion,
) -> Result<RawToolInput> {
    let want_name = pv.tool_name();
    for choice in &resp.choices {
        for tc in &choice.message.tool_calls {
            let func = match &tc.function {
                Some(f) => f,
                None => continue,
            };
            if func.name.as_deref() != Some(want_name) {
                continue;
            }
            let args = match &func.arguments {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            return serde_json::from_str(args).map_err(|e| {
                MemoryError::Provider(format!(
                    "openai tool-call arguments failed schema: {e}"
                ))
            });
        }
    }
    // No matching tool call — treat as empty (model refused or returned text only).
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

    fn fake_response(arguments: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": v1::TOOL_NAME,
                            "arguments": arguments.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    }

    #[test]
    fn build_request_uses_required_string_tool_choice() {
        let body =
            build_request_body("gpt-4o-mini", 1024, &[turn("user", "hi")], OpenAIPromptVersion::V2);
        assert_eq!(body["model"], "gpt-4o-mini");
        // "required" (string) is portable across real OpenAI + LM Studio +
        // vLLM. Since we only expose one tool, it's equivalent to forcing that
        // specific tool.
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["tools"][0]["function"]["name"], v1::TOOL_NAME);
    }

    #[test]
    fn build_request_v1_only_facts_events() {
        let body =
            build_request_body("gpt-4o-mini", 1024, &[turn("user", "hi")], OpenAIPromptVersion::V1);
        let req = body["tools"][0]["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        let names: Vec<_> = req.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"facts"));
        assert!(names.contains(&"events"));
        assert!(!names.contains(&"instructions"));
        assert!(!names.contains(&"tasks"));
    }

    #[test]
    fn parse_tool_output_extracts_arguments() {
        let resp_json = fake_response(serde_json::json!({
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
        let resp: ChatCompletionsResponse = serde_json::from_value(resp_json).unwrap();
        let raw = parse_tool_output(&resp, OpenAIPromptVersion::V2).unwrap();
        assert_eq!(raw.facts.len(), 1);
        assert_eq!(raw.facts[0].predicate.as_deref(), Some("prefers"));
    }

    #[test]
    fn parse_tool_output_empty_when_no_tool_call() {
        let resp: ChatCompletionsResponse = serde_json::from_value(serde_json::json!({
            "id": "x",
            "model": "x",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "I cannot extract.", "tool_calls": []},
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        let raw = parse_tool_output(&resp, OpenAIPromptVersion::V2).unwrap();
        assert!(raw.facts.is_empty());
        assert!(raw.events.is_empty());
    }

    #[tokio::test]
    async fn happy_path_against_wiremock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_response(
                serde_json::json!({
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
                }),
            )))
            .mount(&server)
            .await;

        let extractor = OpenAIExtractor::new("test-key").with_base_url(server.uri());
        let out = extractor
            .extract(&[turn("user", "max 800k euros")])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(<OpenAIExtractor as Extractor>::id(&extractor), "openai-v3");
    }

    #[tokio::test]
    async fn http_4xx_becomes_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{\"error\":\"unauthorized\"}"))
            .mount(&server)
            .await;
        let extractor = OpenAIExtractor::new("bad-key").with_base_url(server.uri());
        let err = extractor.extract(&[turn("user", "hi")]).await.unwrap_err();
        assert!(matches!(err, MemoryError::Provider(_)), "got: {:?}", err);
    }

    #[tokio::test]
    async fn empty_batch_skips_http_call() {
        let extractor =
            OpenAIExtractor::new("test-key").with_base_url("http://127.0.0.1:1");
        assert!(extractor.extract(&[]).await.unwrap().is_empty());
    }

    #[test]
    fn id_changes_with_prompt_version() {
        let default = OpenAIExtractor::new("k");
        // V3 is the empirical baseline default — V4 regressed LongMemEval-S
        // judge_rate by -43 % on 18-item balanced pilot. V4 still available
        // via `with_prompt_version`.
        assert_eq!(<OpenAIExtractor as Extractor>::id(&default), ID_V3);
        let v4 = OpenAIExtractor::new("k").with_prompt_version(OpenAIPromptVersion::V4);
        assert_eq!(<OpenAIExtractor as Extractor>::id(&v4), ID_V4);
        let v2 = OpenAIExtractor::new("k").with_prompt_version(OpenAIPromptVersion::V2);
        assert_eq!(<OpenAIExtractor as Extractor>::id(&v2), ID_V2);
        let v1 = OpenAIExtractor::new("k").with_prompt_version(OpenAIPromptVersion::V1);
        assert_eq!(<OpenAIExtractor as Extractor>::id(&v1), ID_V1);
    }

    #[test]
    fn for_local_server_constructs_offline_capable_extractor() {
        let e = OpenAIExtractor::for_local_server("http://localhost:8000", "Qwen/Qwen3-7B-Instruct");
        assert_eq!(e.base_url, "http://localhost:8000");
        assert_eq!(e.model, "Qwen/Qwen3-7B-Instruct");
    }
}
