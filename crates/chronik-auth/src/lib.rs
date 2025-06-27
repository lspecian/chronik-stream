//! Security and authentication for Chronik Stream.

pub mod tls;
pub mod sasl;
pub mod jwt;
pub mod acl;
pub mod middleware;

pub use tls::{TlsConfig, TlsAcceptor, TlsConnector};
pub use sasl::{SaslMechanism, SaslAuthenticator, SaslCredentials};
pub use jwt::{JwtConfig, JwtManager, Claims};
pub use acl::{Acl, Permission, Resource, Operation};
pub use middleware::{AuthMiddleware, AuthContext, KafkaApiKey};

use thiserror::Error;

/// Authentication error
#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Invalid credentials")]
    InvalidCredentials,
    
    #[error("Token expired")]
    TokenExpired,
    
    #[error("Token invalid: {0}")]
    TokenInvalid(String),
    
    #[error("Permission denied")]
    PermissionDenied,
    
    #[error("TLS error: {0}")]
    TlsError(String),
    
    #[error("SASL error: {0}")]
    SaslError(String),
    
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Authentication result
pub type AuthResult<T> = Result<T, AuthError>;

/// User principal
#[derive(Debug, Clone)]
pub struct UserPrincipal {
    pub username: String,
    pub authenticated: bool,
    pub mechanism: Option<String>,
    pub permissions: Vec<Permission>,
}