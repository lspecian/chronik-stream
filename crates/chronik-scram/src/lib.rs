//! SASL mechanisms and SCRAM for Chronik.
//!
//! The broker's exchange, the client the replica fetcher and the operator log
//! in with, and credential derivation (including the salted password
//! `AlterUserScramCredentials` carries) live here, in one implementation, with
//! no dependency on the protocol or metadata crates. `chronik-protocol`
//! re-exports all of it under its old paths.

use thiserror::Error;

pub mod scram;

/// The identity brokers use to authenticate to each other (replica fetches).
///
/// Its only credential is derived from `CHRONIK_INTER_BROKER_SECRET`; it can
/// never be configured in `CHRONIK_SASL_USERS` or created through
/// `AlterUserScramCredentials`, and it bypasses ACLs. Hardcoding the name is
/// safe, hardcoding a password would not be: it would be the same in every
/// installation and public in the source.
pub const INTER_BROKER_USER: &str = "__chronik_broker";

/// Names reserved for Chronik's own principals. A client-managed user with one
/// of these names could otherwise shadow, or be confused with, the broker.
pub fn is_reserved_username(name: &str) -> bool {
    name.starts_with("__chronik")
}

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
    #[allow(clippy::should_implement_trait)]
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
