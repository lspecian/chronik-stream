//! LLM-pass extractor providers.
//!
//! - **Phase 1 (AMS-1.3)** — [`anthropic::AnthropicExtractor`]
//! - **Phase 2 (AMS-2.3)** — [`openai::OpenAIExtractor`] (also covers vLLM,
//!   LM Studio, llama.cpp HTTP server, and any other OpenAI-compatible
//!   endpoint via [`openai::OpenAIExtractor::with_base_url`]),
//!   plus [`ollama::OllamaExtractor`] for Ollama's native `/api/chat` with
//!   tool calling — distinct from the OpenAI-compat facade so we get
//!   first-class tool-use semantics on local-only deployments.

pub mod anthropic;
pub mod common;
pub mod ollama;
pub mod openai;
