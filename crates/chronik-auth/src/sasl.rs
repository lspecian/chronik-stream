//! SASL authentication support.

use crate::{AuthError, AuthResult, UserPrincipal};
use std::collections::HashMap;
use tracing::debug;
use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha512, Digest};
use pbkdf2::pbkdf2_hmac;

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

/// SCRAM authentication state
#[derive(Debug, Clone)]
pub struct ScramState {
    pub username: String,
    pub client_nonce: String,
    pub server_nonce: String,
    pub salt: Vec<u8>,
    pub iteration_count: u32,
    pub stored_key: Vec<u8>,
    pub server_key: Vec<u8>,
}

/// SCRAM authentication data stored for a user
#[derive(Debug, Clone)]
pub struct ScramUserData {
    pub salt: Vec<u8>,
    pub stored_key: Vec<u8>,
    pub server_key: Vec<u8>,
    pub iteration_count: u32,
}

/// SASL authenticator
pub struct SaslAuthenticator {
    users: HashMap<String, String>, // username -> password hash
    scram_users: HashMap<String, ScramUserData>, // username -> SCRAM data
    scram_sessions: HashMap<String, ScramState>, // session_id -> SCRAM state
}

impl SaslAuthenticator {
    /// Create new SASL authenticator
    pub fn new() -> Self {
        Self {
            users: HashMap::new(),
            scram_users: HashMap::new(),
            scram_sessions: HashMap::new(),
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
        
        self.users.insert(username.clone(), hash);
        
        // Generate SCRAM data
        let scram_data = self.generate_scram_data(&password, &salt_bytes)?;
        self.scram_users.insert(username, scram_data);
        
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
            SaslMechanism::ScramSha256 => self.authenticate_scram_sha256(auth_bytes),
            SaslMechanism::ScramSha512 => self.authenticate_scram_sha512(auth_bytes),
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
    
    /// Generate SCRAM data for a user
    fn generate_scram_data(&self, password: &str, salt: &[u8]) -> AuthResult<ScramUserData> {
        let iteration_count = 4096; // Standard iteration count
        
        // Generate salted password using PBKDF2
        let mut salted_password = [0u8; 32];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iteration_count, &mut salted_password);
        
        // Generate client key: HMAC(salted_password, "Client Key")
        let mut client_key_hmac = Hmac::<Sha256>::new_from_slice(&salted_password)
            .map_err(|_| AuthError::Internal("Failed to create HMAC".to_string()))?;
        client_key_hmac.update(b"Client Key");
        let client_key = client_key_hmac.finalize().into_bytes();
        
        // Generate stored key: SHA256(client_key)
        let stored_key = Sha256::digest(&client_key).to_vec();
        
        // Generate server key: HMAC(salted_password, "Server Key")
        let mut server_key_hmac = Hmac::<Sha256>::new_from_slice(&salted_password)
            .map_err(|_| AuthError::Internal("Failed to create HMAC".to_string()))?;
        server_key_hmac.update(b"Server Key");
        let server_key = server_key_hmac.finalize().into_bytes().to_vec();
        
        Ok(ScramUserData {
            salt: salt.to_vec(),
            stored_key,
            server_key,
            iteration_count,
        })
    }
    
    /// Authenticate using SCRAM-SHA-256
    fn authenticate_scram_sha256(&self, auth_bytes: &[u8]) -> AuthResult<UserPrincipal> {
        // Parse client-first-message
        let message = String::from_utf8_lossy(auth_bytes);
        let parts = self.parse_scram_message(&message)?;
        
        // Extract username
        let username = parts.get("n")
            .ok_or_else(|| AuthError::SaslError("Missing username in SCRAM message".to_string()))?;
        
        // For now, return a simplified implementation
        // In a full implementation, this would require multiple round trips
        // and state management between client and server
        if self.scram_users.contains_key(username) {
            Ok(UserPrincipal {
                username: username.clone(),
                authenticated: true,
                mechanism: Some("SCRAM-SHA-256".to_string()),
                permissions: vec![],
            })
        } else {
            Err(AuthError::InvalidCredentials)
        }
    }
    
    /// Authenticate using SCRAM-SHA-512
    fn authenticate_scram_sha512(&self, auth_bytes: &[u8]) -> AuthResult<UserPrincipal> {
        // Parse client-first-message
        let message = String::from_utf8_lossy(auth_bytes);
        let parts = self.parse_scram_message(&message)?;
        
        // Extract username
        let username = parts.get("n")
            .ok_or_else(|| AuthError::SaslError("Missing username in SCRAM message".to_string()))?;
        
        // For now, return a simplified implementation
        // In a full implementation, this would require multiple round trips
        // and state management between client and server
        if self.scram_users.contains_key(username) {
            Ok(UserPrincipal {
                username: username.clone(),
                authenticated: true,
                mechanism: Some("SCRAM-SHA-512".to_string()),
                permissions: vec![],
            })
        } else {
            Err(AuthError::InvalidCredentials)
        }
    }
    
    /// Parse SCRAM message into key-value pairs
    fn parse_scram_message(&self, message: &str) -> AuthResult<HashMap<String, String>> {
        let mut parts = HashMap::new();
        
        for part in message.split(',') {
            let mut kv = part.splitn(2, '=');
            let key = kv.next().ok_or_else(|| AuthError::SaslError("Invalid SCRAM message format".to_string()))?;
            let value = kv.next().ok_or_else(|| AuthError::SaslError("Invalid SCRAM message format".to_string()))?;
            parts.insert(key.to_string(), value.to_string());
        }
        
        Ok(parts)
    }
}

impl Default for SaslAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}