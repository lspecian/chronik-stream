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

/// Mechanisms this server will advertise and accept.
///
/// PLAIN only. SCRAM-SHA-256/512 were advertised, until this change, while their
/// verification step was a stub that accepted *any* client-final message
/// (hardcoded salt, fabricated server signature, client proof never checked) —
/// so any password authenticated. Advertising a mechanism whose proof is not
/// verified is worse than not offering it, because clients select it in
/// preference to PLAIN. They return here in Phase 1 with real proof
/// verification and a salted credential store; see docs/ROADMAP_SECURITY.md.
const ENABLED_MECHANISMS: &[SaslMechanism] = &[SaslMechanism::Plain];

impl SaslAuthenticator {
    /// Create a new SASL authenticator, taking users from the environment.
    ///
    /// Users come from `CHRONIK_SASL_USERS` (`"user1:pass1,user2:pass2"`). If it
    /// is unset the authenticator has no users and every authentication attempt
    /// fails — that is the intended default. Previously this silently
    /// installed `admin/admin123`, `user/user123` and `kafka/kafka-secret`.
    pub fn new() -> Self {
        match std::env::var("CHRONIK_SASL_USERS") {
            Ok(users_config) => Self::new_from_config(&users_config),
            Err(_) => {
                warn!(
                    "SASL enabled but CHRONIK_SASL_USERS is not set - no users are configured, \
                     so every authentication attempt will be rejected. \
                     Set CHRONIK_SASL_USERS='user1:pass1,user2:pass2'."
                );
                Self::new_empty()
            }
        }
    }

    /// Create a new SASL authenticator with no users.
    pub fn new_empty() -> Self {
        Self {
            supported_mechanisms: ENABLED_MECHANISMS.to_vec(),
            users: HashMap::new(),
            state: SaslState::Initial,
            scram_state: None,
        }
    }

    /// Create a new SASL authenticator from config string
    /// Format: "user1:pass1,user2:pass2"
    pub fn new_from_config(config: &str) -> Self {
        let mut users = HashMap::new();
        for pair in config.split(',') {
            let parts: Vec<&str> = pair.trim().splitn(2, ':').collect();
            if parts.len() == 2 && !parts[0].is_empty() {
                users.insert(parts[0].to_string(), parts[1].to_string());
                info!("SASL user configured: {}", parts[0]);
            }
        }
        if users.is_empty() {
            warn!(
                "CHRONIK_SASL_USERS provided but no valid users parsed - \
                 every authentication attempt will be rejected"
            );
        }
        Self {
            supported_mechanisms: ENABLED_MECHANISMS.to_vec(),
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
                    // Unreachable via the handshake, which only completes for a
                    // mechanism in ENABLED_MECHANISMS. Kept as a hard refusal so
                    // that re-adding a mechanism to that list cannot silently
                    // reintroduce an unverified authentication path.
                    other => Err(SaslError::UnsupportedMechanism(other.as_str().to_string())),
                }
            }
            SaslState::Authenticating => Err(SaslError::ProtocolError(
                "Multi-step SASL exchange is not supported by any enabled mechanism".to_string(),
            )),
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

/// SCRAM server state.
///
/// Retained for the Phase 1 SCRAM implementation (real per-user salt, stored
/// key, client-proof verification). Nothing sets it today — the exchange that
/// populated it was removed because it never verified the proof.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ScramServerState {
    username: String,
    client_nonce: String,
    server_nonce: String,
    mechanism: SaslMechanism,
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
        // Users must be provisioned explicitly - there are no built-in defaults.
        let mut auth = SaslAuthenticator::new_from_config("admin:admin123");

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
        let mut auth = SaslAuthenticator::new_from_config("admin:admin123");

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
        let mut auth = SaslAuthenticator::new_empty();

        let result = auth.handle_handshake(1, &["UNKNOWN".to_string()]);
        assert!(result.is_err());
    }

    /// SCRAM must NOT be advertised or accepted while its proof verification is
    /// unimplemented. Previously it was advertised and the client proof was
    /// never checked, so any password authenticated.
    #[test]
    fn test_scram_is_not_advertised_or_accepted() {
        let auth = SaslAuthenticator::new_from_config("admin:admin123");

        assert!(
            !auth.supported_mechanisms().contains(&SaslMechanism::ScramSha256),
            "SCRAM-SHA-256 must not be advertised while unimplemented"
        );
        assert!(
            !auth.supported_mechanisms().contains(&SaslMechanism::ScramSha512),
            "SCRAM-SHA-512 must not be advertised while unimplemented"
        );

        // A client asking only for SCRAM must be refused, not silently accepted.
        let mut auth = SaslAuthenticator::new_from_config("admin:admin123");
        assert!(auth
            .handle_handshake(1, &["SCRAM-SHA-256".to_string()])
            .is_err());
        assert!(!auth.is_authenticated());
    }

    /// No default users: a fresh authenticator rejects the credentials that used
    /// to be hardcoded.
    #[test]
    fn test_no_default_users() {
        for (user, pass) in [
            ("admin", "admin123"),
            ("user", "user123"),
            ("kafka", "kafka-secret"),
        ] {
            let mut auth = SaslAuthenticator::new_empty();
            auth.handle_handshake(1, &["PLAIN".to_string()]).unwrap();
            let bytes = format!("\0{}\0{}", user, pass).into_bytes();
            assert!(
                auth.handle_authenticate(&bytes).is_err(),
                "{} must not authenticate against an unconfigured server",
                user
            );
            assert!(!auth.is_authenticated());
        }
    }

    #[test]
    fn test_custom_users_from_config() {
        let mut auth = SaslAuthenticator::new_from_config("myuser:mypass,other:secret");

        // Handshake
        auth.handle_handshake(1, &["PLAIN".to_string()]).unwrap();

        // Authenticate with custom user
        let auth_bytes = format!("\0myuser\0mypass").into_bytes();
        let response = auth.handle_authenticate(&auth_bytes).unwrap();
        assert_eq!(response.error_code, 0);
        assert!(auth.is_authenticated());
        assert_eq!(auth.username(), Some("myuser"));
    }

    #[test]
    fn test_empty_authenticator_rejects_all() {
        let mut auth = SaslAuthenticator::new_empty();

        // Handshake should work
        auth.handle_handshake(1, &["PLAIN".to_string()]).unwrap();

        // But authentication should fail (no users configured)
        let auth_bytes = format!("\0admin\0admin123").into_bytes();
        let result = auth.handle_authenticate(&auth_bytes);
        assert!(result.is_err());
        assert!(!auth.is_authenticated());
    }
}