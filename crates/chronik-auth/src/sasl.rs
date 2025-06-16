//! SASL authentication support.

use crate::{AuthError, AuthResult, UserPrincipal};
use std::collections::HashMap;
use tracing::debug;

/// SASL mechanism
#[derive(Debug, Clone, PartialEq)]
pub enum SaslMechanism {
    Plain,
    ScramSha256,
    ScramSha512,
    GssApi,
}

impl SaslMechanism {
    /// Parse mechanism from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "PLAIN" => Some(Self::Plain),
            "SCRAM-SHA-256" => Some(Self::ScramSha256),
            "SCRAM-SHA-512" => Some(Self::ScramSha512),
            "GSSAPI" => Some(Self::GssApi),
            _ => None,
        }
    }
    
    /// Get mechanism as string
    pub fn as_str(&self) -> &str {
        match self {
            Self::Plain => "PLAIN",
            Self::ScramSha256 => "SCRAM-SHA-256",
            Self::ScramSha512 => "SCRAM-SHA-512",
            Self::GssApi => "GSSAPI",
        }
    }
}

/// SASL credentials
#[derive(Debug, Clone)]
pub struct SaslCredentials {
    pub username: String,
    pub password: String,
}

/// SASL authenticator
pub struct SaslAuthenticator {
    users: HashMap<String, String>, // username -> password hash
}

impl SaslAuthenticator {
    /// Create new SASL authenticator
    pub fn new() -> Self {
        Self {
            users: HashMap::new(),
        }
    }
    
    /// Add user
    pub fn add_user(&mut self, username: String, password: String) -> AuthResult<()> {
        use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
        
        // Generate salt using ring's random
        let rng = ring::rand::SystemRandom::new();
        let mut salt_bytes = [0u8; 16];
        ring::rand::SecureRandom::fill(&rng, &mut salt_bytes)
            .map_err(|_| AuthError::Internal("Failed to generate salt".to_string()))?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| AuthError::Internal(format!("Failed to encode salt: {}", e)))?;
        
        // Hash password with argon2
        let argon2 = Argon2::default();
        let hash = argon2.hash_password(password.as_bytes(), &salt)
            .map_err(|e| AuthError::Internal(format!("Failed to hash password: {}", e)))?
            .to_string();
        
        self.users.insert(username, hash);
        Ok(())
    }
    
    /// Authenticate using SASL
    pub fn authenticate(
        &self,
        mechanism: &SaslMechanism,
        auth_bytes: &[u8],
    ) -> AuthResult<UserPrincipal> {
        match mechanism {
            SaslMechanism::Plain => self.authenticate_plain(auth_bytes),
            SaslMechanism::ScramSha256 | SaslMechanism::ScramSha512 => {
                Err(AuthError::SaslError("SCRAM not implemented yet".to_string()))
            }
            SaslMechanism::GssApi => {
                Err(AuthError::SaslError("GSSAPI not implemented yet".to_string()))
            }
        }
    }
    
    /// Authenticate using PLAIN mechanism
    fn authenticate_plain(&self, auth_bytes: &[u8]) -> AuthResult<UserPrincipal> {
        // PLAIN mechanism format: [authzid]\0authcid\0passwd
        let parts: Vec<&[u8]> = auth_bytes.split(|&b| b == 0).collect();
        
        if parts.len() != 3 {
            return Err(AuthError::InvalidCredentials);
        }
        
        let username = String::from_utf8_lossy(parts[1]).to_string();
        let password = String::from_utf8_lossy(parts[2]).to_string();
        
        debug!("PLAIN auth attempt for user: {}", username);
        
        // Verify credentials
        if let Some(hash) = self.users.get(&username) {
            use argon2::{Argon2, PasswordVerifier, PasswordHash};
            
            let parsed_hash = PasswordHash::new(hash)
                .map_err(|_| AuthError::Internal("Invalid password hash".to_string()))?;
            
            let argon2 = Argon2::default();
            let valid = argon2.verify_password(password.as_bytes(), &parsed_hash).is_ok();
            
            if valid {
                Ok(UserPrincipal {
                    username,
                    authenticated: true,
                    mechanism: Some("PLAIN".to_string()),
                    permissions: vec![], // TODO: Load from ACL
                })
            } else {
                Err(AuthError::InvalidCredentials)
            }
        } else {
            Err(AuthError::InvalidCredentials)
        }
    }
    
    /// Handle SASL handshake request
    pub fn handshake(&self, mechanisms: &[String]) -> Vec<String> {
        let supported = vec![
            SaslMechanism::Plain,
            SaslMechanism::ScramSha256,
            SaslMechanism::ScramSha512,
        ];
        
        supported
            .into_iter()
            .map(|m| m.as_str().to_string())
            .filter(|m| mechanisms.is_empty() || mechanisms.contains(m))
            .collect()
    }
}

impl Default for SaslAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}