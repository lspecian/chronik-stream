//! SASL Authentication Support for Chronik Stream
//!
//! Implements SASL handshake and authentication mechanisms
//! to support secure client connections.

use std::collections::HashMap;
use bytes::{Bytes, BytesMut, BufMut};
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn, error};
use thiserror::Error;

use crate::scram::{self, ScramCredential, ScramExchange};

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

/// A user's credentials.
///
/// The plaintext password is kept only because SASL/PLAIN requires comparing
/// against it. SCRAM credentials hold no password-equivalent material and are
/// derived once, when the user is added.
#[derive(Debug, Clone)]
pub struct SaslUser {
    password: String,
    scram_sha256: ScramCredential,
    scram_sha512: ScramCredential,
}

impl SaslUser {
    /// Derive a user's credentials from a plaintext password.
    ///
    /// Both SCRAM variants are derived up front: which one a client selects is
    /// not known until the handshake, and deriving lazily would put a PBKDF2 run
    /// on the authentication path.
    pub fn from_password(password: &str, iterations: u32) -> Result<Self, SaslError> {
        Ok(Self {
            password: password.to_string(),
            scram_sha256: ScramCredential::derive(
                password,
                SaslMechanism::ScramSha256,
                iterations,
            )?,
            scram_sha512: ScramCredential::derive(
                password,
                SaslMechanism::ScramSha512,
                iterations,
            )?,
        })
    }

    fn scram_credential(&self, mechanism: SaslMechanism) -> Option<ScramCredential> {
        match mechanism {
            SaslMechanism::ScramSha256 => Some(self.scram_sha256.clone()),
            SaslMechanism::ScramSha512 => Some(self.scram_sha512.clone()),
            _ => None,
        }
    }
}

/// SASL authenticator
pub struct SaslAuthenticator {
    /// Supported mechanisms
    supported_mechanisms: Vec<SaslMechanism>,
    /// User credential store
    users: HashMap<String, SaslUser>,
    /// Current authentication state
    state: SaslState,
    /// In-flight SCRAM exchange, if the negotiated mechanism is SCRAM
    scram_exchange: Option<ScramExchange>,
    /// Mechanism chosen by the handshake, retained past the handshake state
    /// so the authenticated principal can be attributed to a mechanism.
    negotiated: Option<SaslMechanism>,
}

/// Mechanisms this server will advertise and accept.
///
/// SCRAM-SHA-256/512 were once advertised while their verification step was a
/// stub that accepted *any* client-final message (hardcoded salt, fabricated
/// server signature, client proof never checked), so any password
/// authenticated. They were withdrawn, and are back here only now that
/// [`crate::scram`] performs real RFC 5802 proof verification.
///
/// Order matters: librdkafka and the Java client pick the first mutually
/// supported mechanism, so the stronger ones are listed first and PLAIN — which
/// sends the password in the clear and is only safe under TLS — is last.
const ENABLED_MECHANISMS: &[SaslMechanism] = &[
    SaslMechanism::ScramSha512,
    SaslMechanism::ScramSha256,
    SaslMechanism::Plain,
];

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
            scram_exchange: None,
            negotiated: None,
        }
    }

    /// Create a new SASL authenticator from config string
    /// Format: "user1:pass1,user2:pass2"
    pub fn new_from_config(config: &str) -> Self {
        let mut authenticator = Self::new_empty();
        for pair in config.split(',') {
            let parts: Vec<&str> = pair.trim().splitn(2, ':').collect();
            if parts.len() == 2 && !parts[0].is_empty() {
                authenticator.add_user(parts[0].to_string(), parts[1].to_string());
                info!("SASL user configured: {}", parts[0]);
            }
        }
        if authenticator.users.is_empty() {
            warn!(
                "CHRONIK_SASL_USERS provided but no valid users parsed - \
                 every authentication attempt will be rejected"
            );
        }
        authenticator
    }

    /// Add a user, deriving its SCRAM credentials from the password.
    pub fn add_user(&mut self, username: String, password: String) {
        match SaslUser::from_password(&password, scram::DEFAULT_ITERATIONS) {
            Ok(user) => {
                self.users.insert(username, user);
            }
            Err(e) => {
                // Refusing to add the user is the safe failure: the alternative
                // is a user that exists for PLAIN but not for SCRAM.
                warn!("Failed to derive credentials for user '{}': {}", username, e);
            }
        }
    }

    /// Add a user with pre-derived SCRAM credentials (credential store import).
    pub fn add_user_with_credentials(&mut self, username: String, user: SaslUser) {
        self.users.insert(username, user);
    }

    /// Number of configured users.
    pub fn user_count(&self) -> usize {
        self.users.len()
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
                self.negotiated = Some(mechanism);

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
    ///
    /// PLAIN completes in one round trip; SCRAM takes two, so this is a state
    /// machine: `HandshakeComplete` consumes `client-first` and answers
    /// `server-first`, and the following call in `Authenticating` consumes
    /// `client-final` and answers `server-final`.
    pub fn handle_authenticate(&mut self, auth_bytes: &[u8]) -> Result<SaslAuthenticateResponse, SaslError> {
        match self.state.clone() {
            SaslState::HandshakeComplete(mechanism) => match mechanism {
                SaslMechanism::Plain => self.handle_plain_auth(auth_bytes),
                SaslMechanism::ScramSha256 | SaslMechanism::ScramSha512 => {
                    self.begin_scram_auth(auth_bytes, mechanism)
                }
                // Unreachable via the handshake, which only completes for a
                // mechanism in ENABLED_MECHANISMS. Kept as a hard refusal so
                // that re-adding a mechanism to that list cannot silently
                // reintroduce an unverified authentication path.
                other => Err(SaslError::UnsupportedMechanism(other.as_str().to_string())),
            },
            SaslState::Authenticating => self.finish_scram_auth(auth_bytes),
            _ => Err(SaslError::ProtocolError("Invalid state for authenticate".to_string())),
        }
    }

    /// SCRAM step 1: consume `client-first`, answer `server-first`.
    fn begin_scram_auth(
        &mut self,
        auth_bytes: &[u8],
        mechanism: SaslMechanism,
    ) -> Result<SaslAuthenticateResponse, SaslError> {
        let users = &self.users;
        let (exchange, server_first) =
            ScramExchange::start(mechanism, auth_bytes, |username| {
                users.get(username).and_then(|u| u.scram_credential(mechanism))
            })?;

        self.scram_exchange = Some(exchange);
        self.state = SaslState::Authenticating;

        Ok(SaslAuthenticateResponse {
            error_code: 0,
            error_message: None,
            auth_bytes: Some(server_first),
            // The session lifetime is only meaningful once the exchange
            // completes; sending it here would be premature.
            session_lifetime_ms: None,
        })
    }

    /// SCRAM step 2: verify `client-final`, answer `server-final`.
    fn finish_scram_auth(&mut self, auth_bytes: &[u8]) -> Result<SaslAuthenticateResponse, SaslError> {
        let exchange = self
            .scram_exchange
            .take()
            .ok_or_else(|| SaslError::ProtocolError("No SCRAM exchange in progress".into()))?;

        match exchange.finish(auth_bytes) {
            Ok(server_final) => {
                let username = exchange.username().to_string();
                info!("SCRAM authentication successful for user: {}", username);
                self.state = SaslState::Authenticated(username);

                Ok(SaslAuthenticateResponse {
                    error_code: 0,
                    error_message: None,
                    auth_bytes: Some(server_final),
                    session_lifetime_ms: Some(3600000), // 1 hour
                })
            }
            Err(e) => {
                warn!(
                    "SCRAM authentication failed for user '{}': {}",
                    exchange.username(),
                    e
                );
                self.state = SaslState::Failed(e.to_string());
                Err(e)
            }
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

        // Constant-time comparison: a byte-by-byte `==` on the password leaks
        // its length and prefix through timing.
        let authenticated = match self.users.get(username) {
            Some(user) => {
                let stored = user.password.as_bytes();
                let offered = password.as_bytes();
                stored.len() == offered.len() && stored.ct_eq(offered).unwrap_u8() == 1
            }
            None => false,
        };

        match authenticated {
            true => {
                info!("PLAIN authentication successful for user: {}", username);
                self.state = SaslState::Authenticated(username.to_string());

                Ok(SaslAuthenticateResponse {
                    error_code: 0,
                    error_message: None,
                    auth_bytes: None,
                    session_lifetime_ms: Some(3600000), // 1 hour
                })
            }
            false => {
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

    /// Get authenticated username.
    ///
    /// `None` until the exchange completes — importantly, that includes the
    /// intermediate step of a SCRAM exchange, so a caller that keys "is this
    /// connection authenticated?" off this method cannot be fooled by a
    /// half-finished SCRAM handshake.
    pub fn username(&self) -> Option<&str> {
        match &self.state {
            SaslState::Authenticated(username) => Some(username),
            _ => None,
        }
    }

    /// The mechanism negotiated by the handshake, if any.
    pub fn negotiated_mechanism(&self) -> Option<SaslMechanism> {
        match &self.state {
            SaslState::HandshakeComplete(m) => Some(*m),
            // Once authenticating or authenticated the handshake state is gone;
            // the in-flight exchange (SCRAM) or PLAIN is implied.
            _ => self.negotiated,
        }
    }
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
/// Encode a SaslAuthenticate response body for the given request version.
///
/// The version matters. `session_lifetime_ms` is a **mandatory int64 from v1
/// onwards** and absent in v0. This function previously took no version and
/// wrote the field only when the value happened to be `Some`, so a v1 response
/// carrying no lifetime was eight bytes short: librdkafka rejected it with
/// `Protocol read buffer underflow for SaslAuthenticate v1 ... expected 8 bytes
/// > 0 remaining bytes`. That is exactly the intermediate step of a SCRAM
/// exchange, which has no session lifetime to report yet — so SCRAM could not
/// complete against any real client.
///
/// v2+ is flexible (compact strings/bytes plus tagged fields).
pub fn encode_sasl_authenticate_response(
    response: &SaslAuthenticateResponse,
    version: i16,
) -> Bytes {
    let mut buf = BytesMut::new();
    let flexible = version >= 2;

    // Error code
    buf.put_i16(response.error_code);

    // Error message (nullable string)
    match &response.error_message {
        Some(msg) if flexible => {
            put_unsigned_varint(&mut buf, msg.len() as u32 + 1);
            buf.put_slice(msg.as_bytes());
        }
        Some(msg) => {
            buf.put_i16(msg.len() as i16);
            buf.put_slice(msg.as_bytes());
        }
        None if flexible => put_unsigned_varint(&mut buf, 0), // null
        None => buf.put_i16(-1),                              // null
    }

    // Auth bytes (nullable bytes)
    match &response.auth_bytes {
        Some(bytes) if flexible => {
            put_unsigned_varint(&mut buf, bytes.len() as u32 + 1);
            buf.put_slice(bytes);
        }
        Some(bytes) => {
            buf.put_i32(bytes.len() as i32);
            buf.put_slice(bytes);
        }
        None if flexible => put_unsigned_varint(&mut buf, 0), // null
        None => buf.put_i32(-1),                              // null
    }

    // SessionLifetimeMs: present from v1, and NOT optional. Zero means "no
    // expiry communicated", which is what an in-progress exchange reports.
    if version >= 1 {
        buf.put_i64(response.session_lifetime_ms.unwrap_or(0));
    }

    // Tagged fields (empty) for flexible versions.
    if flexible {
        put_unsigned_varint(&mut buf, 0);
    }

    buf.freeze()
}

/// Kafka unsigned varint (used by flexible versions for lengths and tag counts).
fn put_unsigned_varint(buf: &mut BytesMut, mut value: u32) {
    loop {
        if value < 0x80 {
            buf.put_u8(value as u8);
            return;
        }
        buf.put_u8(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
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

    /// SCRAM is advertised only now that its proof is genuinely verified.
    ///
    /// This test replaces one asserting the opposite. SCRAM was withdrawn while
    /// its verification was a stub that accepted any client-final message; it is
    /// back because [`crate::scram`] implements RFC 5802 properly. The
    /// *behavioural* guarantee is the second half: a wrong password must fail.
    #[test]
    fn test_scram_is_advertised_and_verified() {
        let auth = SaslAuthenticator::new_from_config("admin:admin123");
        assert!(auth
            .supported_mechanisms()
            .contains(&SaslMechanism::ScramSha256));
        assert!(auth
            .supported_mechanisms()
            .contains(&SaslMechanism::ScramSha512));

        // Stronger mechanisms are offered ahead of PLAIN, which clients select
        // by first match.
        assert_eq!(
            auth.supported_mechanisms().first(),
            Some(&SaslMechanism::ScramSha512)
        );

        // A SCRAM exchange with the WRONG password must fail at client-final.
        let mut auth = SaslAuthenticator::new_from_config("admin:admin123");
        auth.handle_handshake(1, &["SCRAM-SHA-256".to_string()])
            .unwrap();
        let server_first = auth
            .handle_authenticate(b"n,,n=admin,r=clientnonce")
            .expect("server-first must be produced");
        assert_eq!(server_first.error_code, 0);
        assert!(server_first.auth_bytes.is_some());

        // A forged client-final (proof of the right length, wrong content).
        let nonce = {
            let sf = String::from_utf8(server_first.auth_bytes.unwrap()).unwrap();
            sf.split(',')
                .find(|p| p.starts_with("r="))
                .unwrap()
                .trim_start_matches("r=")
                .to_string()
        };
        let forged = format!(
            "c=biws,r={},p={}",
            nonce,
            base64::engine::general_purpose::STANDARD.encode([0u8; 32])
        );
        assert!(
            auth.handle_authenticate(forged.as_bytes()).is_err(),
            "a forged SCRAM proof must not authenticate"
        );
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