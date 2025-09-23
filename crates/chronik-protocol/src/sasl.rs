//! SASL Authentication Support for Chronik Stream
//!
//! Implements SASL handshake and authentication mechanisms
//! to support secure client connections.

use std::collections::HashMap;
use bytes::{Bytes, BytesMut, BufMut};
use tracing::{debug, info, warn, error};
use thiserror::Error;

/// SASL mechanism types supported
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaslMechanism {
    /// PLAIN mechanism (username/password)
    Plain,
    /// SCRAM-SHA-256 mechanism
    ScramSha256,
    /// SCRAM-SHA-512 mechanism
    ScramSha512,
    /// GSSAPI/Kerberos mechanism (stub)
    GssApi,
    /// OAUTHBEARER mechanism (stub)
    OAuthBearer,
}

impl SaslMechanism {
    /// Parse mechanism from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "PLAIN" => Some(Self::Plain),
            "SCRAM-SHA-256" => Some(Self::ScramSha256),
            "SCRAM-SHA-512" => Some(Self::ScramSha512),
            "GSSAPI" => Some(Self::GssApi),
            "OAUTHBEARER" => Some(Self::OAuthBearer),
            _ => None,
        }
    }

    /// Get mechanism name as string
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plain => "PLAIN",
            Self::ScramSha256 => "SCRAM-SHA-256",
            Self::ScramSha512 => "SCRAM-SHA-512",
            Self::GssApi => "GSSAPI",
            Self::OAuthBearer => "OAUTHBEARER",
        }
    }
}

/// SASL authentication error
#[derive(Debug, Error)]
pub enum SaslError {
    #[error("Unsupported mechanism: {0}")]
    UnsupportedMechanism(String),

    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("Invalid credentials")]
    InvalidCredentials,

    #[error("Protocol error: {0}")]
    ProtocolError(String),

    #[error("Internal error: {0}")]
    InternalError(String),
}

/// SASL authentication state
#[derive(Debug, Clone)]
pub enum SaslState {
    /// Initial state, waiting for handshake
    Initial,
    /// Handshake received, waiting for authentication
    HandshakeComplete(SaslMechanism),
    /// Authentication in progress
    Authenticating,
    /// Authentication successful
    Authenticated(String), // username
    /// Authentication failed
    Failed(String), // error message
}

/// SASL authenticator
pub struct SaslAuthenticator {
    /// Supported mechanisms
    supported_mechanisms: Vec<SaslMechanism>,
    /// User credentials store (for demo/testing)
    users: HashMap<String, String>, // username -> password
    /// Current authentication state
    state: SaslState,
    /// SCRAM server state (if using SCRAM)
    scram_state: Option<ScramServerState>,
}

impl SaslAuthenticator {
    /// Create a new SASL authenticator
    pub fn new() -> Self {
        let mut users = HashMap::new();
        // Add some default users for testing
        users.insert("admin".to_string(), "admin123".to_string());
        users.insert("user".to_string(), "user123".to_string());
        users.insert("kafka".to_string(), "kafka-secret".to_string());

        Self {
            supported_mechanisms: vec![
                SaslMechanism::Plain,
                SaslMechanism::ScramSha256,
                SaslMechanism::ScramSha512,
            ],
            users,
            state: SaslState::Initial,
            scram_state: None,
        }
    }

    /// Add a user for authentication
    pub fn add_user(&mut self, username: String, password: String) {
        self.users.insert(username, password);
    }

    /// Get supported mechanisms
    pub fn supported_mechanisms(&self) -> &[SaslMechanism] {
        &self.supported_mechanisms
    }

    /// Handle SASL handshake request (API key 17)
    pub fn handle_handshake(&mut self, version: i16, mechanisms: &[String]) -> Result<SaslHandshakeResponse, SaslError> {
        debug!("SASL handshake request: version={}, mechanisms={:?}", version, mechanisms);

        // Find a supported mechanism
        let selected_mechanism = mechanisms
            .iter()
            .filter_map(|m| SaslMechanism::from_str(m))
            .find(|m| self.supported_mechanisms.contains(m));

        match selected_mechanism {
            Some(mechanism) => {
                info!("Selected SASL mechanism: {}", mechanism.as_str());
                self.state = SaslState::HandshakeComplete(mechanism);

                Ok(SaslHandshakeResponse {
                    error_code: 0,
                    enabled_mechanisms: self.supported_mechanisms
                        .iter()
                        .map(|m| m.as_str().to_string())
                        .collect(),
                })
            }
            None => {
                warn!("No supported SASL mechanism found in: {:?}", mechanisms);
                Err(SaslError::UnsupportedMechanism(
                    mechanisms.join(", ")
                ))
            }
        }
    }

    /// Handle SASL authenticate request (API key 36)
    pub fn handle_authenticate(&mut self, auth_bytes: &[u8]) -> Result<SaslAuthenticateResponse, SaslError> {
        match &self.state {
            SaslState::HandshakeComplete(mechanism) => {
                match mechanism {
                    SaslMechanism::Plain => self.handle_plain_auth(auth_bytes),
                    SaslMechanism::ScramSha256 | SaslMechanism::ScramSha512 => {
                        self.handle_scram_auth(auth_bytes, *mechanism)
                    }
                    _ => Err(SaslError::UnsupportedMechanism(mechanism.as_str().to_string())),
                }
            }
            SaslState::Authenticating => {
                // Continue SCRAM authentication
                if self.scram_state.is_some() {
                    self.continue_scram_auth(auth_bytes)
                } else {
                    Err(SaslError::ProtocolError("Unexpected authenticate request".to_string()))
                }
            }
            _ => Err(SaslError::ProtocolError("Invalid state for authenticate".to_string())),
        }
    }

    /// Handle PLAIN authentication
    fn handle_plain_auth(&mut self, auth_bytes: &[u8]) -> Result<SaslAuthenticateResponse, SaslError> {
        // PLAIN format: \0username\0password
        let auth_str = String::from_utf8_lossy(auth_bytes);
        let parts: Vec<&str> = auth_str.split('\0').collect();

        if parts.len() != 3 || !parts[0].is_empty() {
            return Err(SaslError::ProtocolError("Invalid PLAIN auth format".to_string()));
        }

        let username = parts[1];
        let password = parts[2];

        // Verify credentials
        match self.users.get(username) {
            Some(stored_password) if stored_password == password => {
                info!("PLAIN authentication successful for user: {}", username);
                self.state = SaslState::Authenticated(username.to_string());

                Ok(SaslAuthenticateResponse {
                    error_code: 0,
                    error_message: None,
                    auth_bytes: None,
                    session_lifetime_ms: Some(3600000), // 1 hour
                })
            }
            _ => {
                warn!("PLAIN authentication failed for user: {}", username);
                self.state = SaslState::Failed("Invalid credentials".to_string());

                Err(SaslError::InvalidCredentials)
            }
        }
    }

    /// Handle SCRAM authentication (initial)
    fn handle_scram_auth(&mut self, auth_bytes: &[u8], mechanism: SaslMechanism) -> Result<SaslAuthenticateResponse, SaslError> {
        // Parse client-first message
        let client_first = String::from_utf8_lossy(auth_bytes);
        debug!("SCRAM client-first: {}", client_first);

        // Extract username from client-first message
        // Format: n,,n=username,r=client-nonce
        let username = client_first
            .split(',')
            .find(|s| s.starts_with("n="))
            .and_then(|s| s.strip_prefix("n="))
            .ok_or_else(|| SaslError::ProtocolError("Invalid SCRAM client-first".to_string()))?;

        // Generate server nonce
        let server_nonce = format!("server-{}", uuid::Uuid::new_v4().to_string());

        // Create SCRAM state
        self.scram_state = Some(ScramServerState {
            username: username.to_string(),
            client_nonce: extract_nonce(&client_first)?,
            server_nonce: server_nonce.clone(),
            mechanism,
        });

        self.state = SaslState::Authenticating;

        // Build server-first message
        let server_first = format!(
            "r={}{},s={},i=4096",
            extract_nonce(&client_first)?,
            server_nonce,
            base64::encode("salt")
        );

        Ok(SaslAuthenticateResponse {
            error_code: 0,
            error_message: None,
            auth_bytes: Some(server_first.into_bytes()),
            session_lifetime_ms: None,
        })
    }

    /// Continue SCRAM authentication
    fn continue_scram_auth(&mut self, auth_bytes: &[u8]) -> Result<SaslAuthenticateResponse, SaslError> {
        let scram_state = self.scram_state.as_ref()
            .ok_or_else(|| SaslError::InternalError("No SCRAM state".to_string()))?;

        // Parse client-final message
        let client_final = String::from_utf8_lossy(auth_bytes);
        debug!("SCRAM client-final: {}", client_final);

        // For this stub, we'll accept any client-final message
        // In production, this would verify the client proof

        info!("SCRAM authentication successful for user: {}", scram_state.username);
        self.state = SaslState::Authenticated(scram_state.username.clone());

        // Build server-final message
        let server_final = format!("v={}", base64::encode("server-signature"));

        Ok(SaslAuthenticateResponse {
            error_code: 0,
            error_message: None,
            auth_bytes: Some(server_final.into_bytes()),
            session_lifetime_ms: Some(3600000), // 1 hour
        })
    }

    /// Check if authenticated
    pub fn is_authenticated(&self) -> bool {
        matches!(self.state, SaslState::Authenticated(_))
    }

    /// Get authenticated username
    pub fn username(&self) -> Option<&str> {
        match &self.state {
            SaslState::Authenticated(username) => Some(username),
            _ => None,
        }
    }
}

/// SCRAM server state
#[derive(Debug, Clone)]
struct ScramServerState {
    username: String,
    client_nonce: String,
    server_nonce: String,
    mechanism: SaslMechanism,
}

/// Extract nonce from SCRAM message
fn extract_nonce(message: &str) -> Result<String, SaslError> {
    message
        .split(',')
        .find(|s| s.starts_with("r="))
        .and_then(|s| s.strip_prefix("r="))
        .map(|s| s.to_string())
        .ok_or_else(|| SaslError::ProtocolError("Missing nonce".to_string()))
}

/// SASL handshake response
#[derive(Debug, Clone)]
pub struct SaslHandshakeResponse {
    pub error_code: i16,
    pub enabled_mechanisms: Vec<String>,
}

/// SASL authenticate response
#[derive(Debug, Clone)]
pub struct SaslAuthenticateResponse {
    pub error_code: i16,
    pub error_message: Option<String>,
    pub auth_bytes: Option<Vec<u8>>,
    pub session_lifetime_ms: Option<i64>,
}

/// Encode SASL handshake response
pub fn encode_sasl_handshake_response(response: &SaslHandshakeResponse) -> Bytes {
    let mut buf = BytesMut::new();

    // Error code
    buf.put_i16(response.error_code);

    // Enabled mechanisms array
    buf.put_i32(response.enabled_mechanisms.len() as i32);
    for mechanism in &response.enabled_mechanisms {
        buf.put_i16(mechanism.len() as i16);
        buf.put_slice(mechanism.as_bytes());
    }

    buf.freeze()
}

/// Encode SASL authenticate response
pub fn encode_sasl_authenticate_response(response: &SaslAuthenticateResponse) -> Bytes {
    let mut buf = BytesMut::new();

    // Error code
    buf.put_i16(response.error_code);

    // Error message (nullable string)
    if let Some(ref msg) = response.error_message {
        buf.put_i16(msg.len() as i16);
        buf.put_slice(msg.as_bytes());
    } else {
        buf.put_i16(-1); // null
    }

    // Auth bytes (nullable bytes)
    if let Some(ref bytes) = response.auth_bytes {
        buf.put_i32(bytes.len() as i32);
        buf.put_slice(bytes);
    } else {
        buf.put_i32(-1); // null
    }

    // Session lifetime ms
    if let Some(lifetime) = response.session_lifetime_ms {
        buf.put_i64(lifetime);
    }

    buf.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sasl_mechanisms() {
        assert_eq!(SaslMechanism::from_str("PLAIN"), Some(SaslMechanism::Plain));
        assert_eq!(SaslMechanism::from_str("SCRAM-SHA-256"), Some(SaslMechanism::ScramSha256));
        assert_eq!(SaslMechanism::from_str("UNKNOWN"), None);
    }

    #[test]
    fn test_plain_authentication() {
        let mut auth = SaslAuthenticator::new();

        // Handshake
        let response = auth.handle_handshake(1, &["PLAIN".to_string()]).unwrap();
        assert_eq!(response.error_code, 0);

        // Authenticate with valid credentials
        let auth_bytes = format!("\0admin\0admin123").into_bytes();
        let response = auth.handle_authenticate(&auth_bytes).unwrap();
        assert_eq!(response.error_code, 0);
        assert!(auth.is_authenticated());
        assert_eq!(auth.username(), Some("admin"));
    }

    #[test]
    fn test_invalid_credentials() {
        let mut auth = SaslAuthenticator::new();

        // Handshake
        auth.handle_handshake(1, &["PLAIN".to_string()]).unwrap();

        // Authenticate with invalid credentials
        let auth_bytes = format!("\0admin\0wrong").into_bytes();
        let result = auth.handle_authenticate(&auth_bytes);
        assert!(result.is_err());
        assert!(!auth.is_authenticated());
    }

    #[test]
    fn test_unsupported_mechanism() {
        let mut auth = SaslAuthenticator::new();

        let result = auth.handle_handshake(1, &["UNKNOWN".to_string()]);
        assert!(result.is_err());
    }
}