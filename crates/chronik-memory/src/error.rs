//! Error type for the Chronik Memory SDK.
//!
//! All public APIs return [`Result<T>`] = `std::result::Result<T, MemoryError>`.
//! `MemoryError` is intentionally distinct from `chronik_common::Error` so the SDK
//! can be published independently of the platform crate tree.

use thiserror::Error;

/// SDK result alias.
pub type Result<T> = std::result::Result<T, MemoryError>;

/// Top-level SDK error.
///
/// Variants are coarse-grained on purpose — callers shouldn't need to match
/// on every underlying library's error. Use `.to_string()` for detail.
#[derive(Error, Debug)]
pub enum MemoryError {
    /// Kafka producer/consumer or admin error (rdkafka).
    #[error("kafka error: {0}")]
    Kafka(String),

    /// HTTP error talking to the Chronik unified API on port 6092.
    #[error("http error: {0}")]
    Http(String),

    /// Schema validation, serde, or Schema Registry error.
    #[error("schema error: {0}")]
    Schema(String),

    /// LLM provider error (Anthropic / OpenAI / Ollama / vLLM).
    #[error("provider error: {0}")]
    Provider(String),

    /// Authentication / authorization error.
    #[error("auth error: {0}")]
    Auth(String),

    /// Configuration error — e.g. missing required builder field.
    #[error("config error: {0}")]
    Config(String),

    /// Caller passed an invalid argument (e.g. bad namespace, empty query).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// I/O error not otherwise classified.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<serde_json::Error> for MemoryError {
    fn from(e: serde_json::Error) -> Self {
        MemoryError::Schema(e.to_string())
    }
}

impl From<reqwest::Error> for MemoryError {
    fn from(e: reqwest::Error) -> Self {
        MemoryError::Http(e.to_string())
    }
}

impl From<rdkafka::error::KafkaError> for MemoryError {
    fn from(e: rdkafka::error::KafkaError) -> Self {
        MemoryError::Kafka(e.to_string())
    }
}
