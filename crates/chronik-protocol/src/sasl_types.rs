//! SASL authentication types for Kafka protocol

use serde::{Deserialize, Serialize};

/// SASL handshake request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaslHandshakeRequest {
    /// Requested SASL mechanism
    pub mechanism: String,
}

/// SASL handshake response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaslHandshakeResponse {
    /// Error code
    pub error_code: i16,
    /// Enabled mechanisms
    pub mechanisms: Vec<String>,
}

/// SASL authenticate request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaslAuthenticateRequest {
    /// Authentication bytes
    pub auth_bytes: Vec<u8>,
}

/// SASL authenticate response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaslAuthenticateResponse {
    /// Error code
    pub error_code: i16,
    /// Error message
    pub error_message: Option<String>,
    /// Authentication bytes
    pub auth_bytes: Vec<u8>,
    /// Session lifetime in milliseconds
    pub session_lifetime_ms: i64,
}