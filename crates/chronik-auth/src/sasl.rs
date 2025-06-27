//! SASL authentication support.

use crate::{AuthError, AuthResult, UserPrincipal, Acl};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;
use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha512, Digest};
use pbkdf2::pbkdf2_hmac;
use base64::{Engine as _, engine::general_purpose};
use ring::rand::{SecureRandom, SystemRandom};

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
    pub auth_message: String,
    pub gs2_header: String,
    pub channel_binding: Option<Vec<u8>>,
}

/// SCRAM exchange phase
#[derive(Debug, Clone, PartialEq)]
pub enum ScramPhase {
    Initial,
    Challenge,
    Verification,
    Complete,
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
    rng: SystemRandom,
    acl: Option<Arc<Acl>>, // Optional ACL for permission loading
}

impl SaslAuthenticator {
    /// Create new SASL authenticator
    pub fn new() -> Self {
        Self {
            users: HashMap::new(),
            scram_users: HashMap::new(),
            scram_sessions: HashMap::new(),
            rng: SystemRandom::new(),
            acl: None,
        }
    }
    
    /// Create new SASL authenticator with ACL
    pub fn with_acl(acl: Arc<Acl>) -> Self {
        Self {
            users: HashMap::new(),
            scram_users: HashMap::new(),
            scram_sessions: HashMap::new(),
            rng: SystemRandom::new(),
            acl: Some(acl),
        }
    }
    
    /// Add user with support for all mechanisms
    pub fn add_user(&mut self, username: String, password: String) -> AuthResult<()> {
        use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
        
        // Generate salt using ring's random
        let mut salt_bytes = [0u8; 16];
        self.rng.fill(&mut salt_bytes)
            .map_err(|_| AuthError::Internal("Failed to generate salt".to_string()))?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| AuthError::Internal(format!("Failed to encode salt: {}", e)))?;
        
        // Hash password with argon2 for PLAIN auth
        let argon2 = Argon2::default();
        let hash = argon2.hash_password(password.as_bytes(), &salt)
            .map_err(|e| AuthError::Internal(format!("Failed to hash password: {}", e)))?
            .to_string();
        
        self.users.insert(username.clone(), hash);
        
        // Generate SCRAM-SHA-256 data
        let scram_data = self.generate_scram_data(&password, &salt_bytes)?;
        self.scram_users.insert(username, scram_data);
        
        Ok(())
    }
    
    /// Add user with specific SCRAM mechanism
    pub fn add_user_with_mechanism(&mut self, username: String, password: String, mechanism: &SaslMechanism) -> AuthResult<()> {
        // Generate salt
        let mut salt_bytes = [0u8; 16];
        self.rng.fill(&mut salt_bytes)
            .map_err(|_| AuthError::Internal("Failed to generate salt".to_string()))?;
        
        match mechanism {
            SaslMechanism::Plain => {
                use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
                let salt = SaltString::encode_b64(&salt_bytes)
                    .map_err(|e| AuthError::Internal(format!("Failed to encode salt: {}", e)))?;
                let argon2 = Argon2::default();
                let hash = argon2.hash_password(password.as_bytes(), &salt)
                    .map_err(|e| AuthError::Internal(format!("Failed to hash password: {}", e)))?
                    .to_string();
                self.users.insert(username, hash);
            }
            SaslMechanism::ScramSha256 => {
                let scram_data = self.generate_scram_data(&password, &salt_bytes)?;
                self.scram_users.insert(username, scram_data);
            }
            SaslMechanism::ScramSha512 => {
                let scram_data = self.generate_scram_data_sha512(&password, &salt_bytes)?;
                self.scram_users.insert(username, scram_data);
            }
            SaslMechanism::GssApi => {
                return Err(AuthError::SaslError("GSSAPI not implemented yet".to_string()));
            }
        }
        
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
                // SCRAM requires multi-step process, this is just for initial compatibility
                // Real implementation should use process_scram_* methods
                Err(AuthError::SaslError("SCRAM requires multi-step authentication. Use process_scram_* methods.".to_string()))
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
                let permissions = if let Some(acl) = &self.acl {
                    acl.get_user_permissions(&username).unwrap_or_default()
                } else {
                    vec![]
                };
                
                Ok(UserPrincipal {
                    username,
                    authenticated: true,
                    mechanism: Some("PLAIN".to_string()),
                    permissions,
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
    
    /// Generate SCRAM data for a user (SHA-256)
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
    
    /// Generate SCRAM data for a user (SHA-512)
    fn generate_scram_data_sha512(&self, password: &str, salt: &[u8]) -> AuthResult<ScramUserData> {
        let iteration_count = 4096; // Standard iteration count
        
        // Generate salted password using PBKDF2
        let mut salted_password = [0u8; 64];
        pbkdf2_hmac::<Sha512>(password.as_bytes(), salt, iteration_count, &mut salted_password);
        
        // Generate client key: HMAC(salted_password, "Client Key")
        let mut client_key_hmac = Hmac::<Sha512>::new_from_slice(&salted_password)
            .map_err(|_| AuthError::Internal("Failed to create HMAC".to_string()))?;
        client_key_hmac.update(b"Client Key");
        let client_key = client_key_hmac.finalize().into_bytes();
        
        // Generate stored key: SHA512(client_key)
        let stored_key = Sha512::digest(&client_key).to_vec();
        
        // Generate server key: HMAC(salted_password, "Server Key")
        let mut server_key_hmac = Hmac::<Sha512>::new_from_slice(&salted_password)
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
    
    /// Process SCRAM-SHA-256 client-first message
    pub fn process_scram_sha256_first(&mut self, session_id: &str, auth_bytes: &[u8]) -> AuthResult<Vec<u8>> {
        self.process_scram_first(session_id, auth_bytes, false)
    }
    
    /// Process SCRAM-SHA-512 client-first message
    pub fn process_scram_sha512_first(&mut self, session_id: &str, auth_bytes: &[u8]) -> AuthResult<Vec<u8>> {
        self.process_scram_first(session_id, auth_bytes, true)
    }
    
    /// Process SCRAM client-first message (generic for both SHA-256 and SHA-512)
    fn process_scram_first(&mut self, session_id: &str, auth_bytes: &[u8], _use_sha512: bool) -> AuthResult<Vec<u8>> {
        let message = String::from_utf8(auth_bytes.to_vec())
            .map_err(|_| AuthError::SaslError("Invalid UTF-8 in client-first message".to_string()))?;
        
        debug!("SCRAM client-first message: {}", message);
        
        // Parse client-first-message
        let (gs2_header, client_first_bare) = self.parse_client_first_message(&message)?;
        
        // Extract username and client nonce
        let (username, client_nonce) = self.parse_client_first_bare(&client_first_bare)?;
        
        // Check if user exists
        let user_data = self.scram_users.get(&username)
            .ok_or(AuthError::InvalidCredentials)?;
        
        // Generate server nonce
        let mut server_nonce_bytes = [0u8; 16];
        self.rng.fill(&mut server_nonce_bytes)
            .map_err(|_| AuthError::Internal("Failed to generate server nonce".to_string()))?;
        let server_nonce = general_purpose::STANDARD.encode(&server_nonce_bytes);
        let combined_nonce = format!("{}{}", client_nonce, server_nonce);
        
        // Create server-first-message
        let server_first = format!(
            "r={},s={},i={}",
            combined_nonce,
            general_purpose::STANDARD.encode(&user_data.salt),
            user_data.iteration_count
        );
        
        // Store session state
        let state = ScramState {
            username: username.clone(),
            client_nonce: client_nonce.clone(),
            server_nonce: server_nonce.clone(),
            salt: user_data.salt.clone(),
            iteration_count: user_data.iteration_count,
            stored_key: user_data.stored_key.clone(),
            server_key: user_data.server_key.clone(),
            auth_message: format!("{},{}", client_first_bare, server_first),
            gs2_header,
            channel_binding: None,
        };
        
        self.scram_sessions.insert(session_id.to_string(), state);
        
        Ok(server_first.into_bytes())
    }
    
    /// Process SCRAM-SHA-256 client-final message
    pub fn process_scram_sha256_final(&mut self, session_id: &str, auth_bytes: &[u8]) -> AuthResult<(UserPrincipal, Vec<u8>)> {
        self.process_scram_final(session_id, auth_bytes, false)
    }
    
    /// Process SCRAM-SHA-512 client-final message
    pub fn process_scram_sha512_final(&mut self, session_id: &str, auth_bytes: &[u8]) -> AuthResult<(UserPrincipal, Vec<u8>)> {
        self.process_scram_final(session_id, auth_bytes, true)
    }
    
    /// Process SCRAM client-final message (generic for both SHA-256 and SHA-512)
    fn process_scram_final(&mut self, session_id: &str, auth_bytes: &[u8], use_sha512: bool) -> AuthResult<(UserPrincipal, Vec<u8>)> {
        let message = String::from_utf8(auth_bytes.to_vec())
            .map_err(|_| AuthError::SaslError("Invalid UTF-8 in client-final message".to_string()))?;
        
        debug!("SCRAM client-final message: {}", message);
        
        // Parse client-final-message first before getting mutable state
        let (channel_binding, nonce, proof) = self.parse_client_final_message(&message)?;
        
        // Get session state and verify
        let state = self.scram_sessions.get(session_id)
            .ok_or_else(|| AuthError::SaslError("No SCRAM session found".to_string()))?;
        
        // Verify nonce
        let expected_nonce = format!("{}{}", state.client_nonce, state.server_nonce);
        if nonce != expected_nonce {
            return Err(AuthError::SaslError("Nonce mismatch".to_string()));
        }
        
        // Complete auth message
        let client_final_without_proof = format!("c={},r={}", channel_binding, nonce);
        let auth_message = format!("{},{}", state.auth_message, client_final_without_proof);
        
        // Verify client proof
        let client_proof = general_purpose::STANDARD.decode(&proof)
            .map_err(|_| AuthError::SaslError("Invalid base64 in client proof".to_string()))?;
        
        // Clone what we need from state before dropping the immutable borrow
        let stored_key = state.stored_key.clone();
        let server_key = state.server_key.clone();
        let username = state.username.clone();
        
        let server_signature = if use_sha512 {
            self.verify_client_proof_sha512_with_data(&stored_key, &server_key, &auth_message, &client_proof)?
        } else {
            self.verify_client_proof_sha256_with_data(&stored_key, &server_key, &auth_message, &client_proof)?
        };
        
        // Create server-final-message
        let server_final = format!("v={}", general_purpose::STANDARD.encode(&server_signature));
        
        // Load permissions from ACL
        let permissions = if let Some(acl) = &self.acl {
            acl.get_user_permissions(&username).unwrap_or_default()
        } else {
            vec![]
        };
        
        // Create user principal
        let principal = UserPrincipal {
            username,
            authenticated: true,
            mechanism: Some(if use_sha512 { "SCRAM-SHA-512" } else { "SCRAM-SHA-256" }.to_string()),
            permissions,
        };
        
        // Clean up session
        self.scram_sessions.remove(session_id);
        
        Ok((principal, server_final.into_bytes()))
    }
    
    /// Verify client proof for SCRAM-SHA-256 with data
    fn verify_client_proof_sha256_with_data(&self, stored_key: &[u8], server_key: &[u8], auth_message: &str, client_proof: &[u8]) -> AuthResult<Vec<u8>> {
        // Calculate ClientSignature = HMAC(StoredKey, AuthMessage)
        let mut client_sig_hmac = Hmac::<Sha256>::new_from_slice(stored_key)
            .map_err(|_| AuthError::Internal("Failed to create HMAC".to_string()))?;
        client_sig_hmac.update(auth_message.as_bytes());
        let client_signature = client_sig_hmac.finalize().into_bytes();
        
        // Calculate ClientKey = ClientProof XOR ClientSignature
        if client_proof.len() != client_signature.len() {
            return Err(AuthError::SaslError("Invalid proof length".to_string()));
        }
        
        let mut client_key = vec![0u8; client_proof.len()];
        for i in 0..client_proof.len() {
            client_key[i] = client_proof[i] ^ client_signature[i];
        }
        
        // Verify StoredKey = SHA256(ClientKey)
        let computed_stored_key = Sha256::digest(&client_key);
        if computed_stored_key.as_slice() != stored_key {
            return Err(AuthError::InvalidCredentials);
        }
        
        // Calculate ServerSignature = HMAC(ServerKey, AuthMessage)
        let mut server_sig_hmac = Hmac::<Sha256>::new_from_slice(server_key)
            .map_err(|_| AuthError::Internal("Failed to create HMAC".to_string()))?;
        server_sig_hmac.update(auth_message.as_bytes());
        let server_signature = server_sig_hmac.finalize().into_bytes();
        
        Ok(server_signature.to_vec())
    }
    
    /// Verify client proof for SCRAM-SHA-512 with data
    fn verify_client_proof_sha512_with_data(&self, stored_key: &[u8], server_key: &[u8], auth_message: &str, client_proof: &[u8]) -> AuthResult<Vec<u8>> {
        // Calculate ClientSignature = HMAC(StoredKey, AuthMessage)
        let mut client_sig_hmac = Hmac::<Sha512>::new_from_slice(stored_key)
            .map_err(|_| AuthError::Internal("Failed to create HMAC".to_string()))?;
        client_sig_hmac.update(auth_message.as_bytes());
        let client_signature = client_sig_hmac.finalize().into_bytes();
        
        // Calculate ClientKey = ClientProof XOR ClientSignature
        if client_proof.len() != client_signature.len() {
            return Err(AuthError::SaslError("Invalid proof length".to_string()));
        }
        
        let mut client_key = vec![0u8; client_proof.len()];
        for i in 0..client_proof.len() {
            client_key[i] = client_proof[i] ^ client_signature[i];
        }
        
        // Verify StoredKey = SHA512(ClientKey)
        let computed_stored_key = Sha512::digest(&client_key);
        if computed_stored_key.as_slice() != stored_key {
            return Err(AuthError::InvalidCredentials);
        }
        
        // Calculate ServerSignature = HMAC(ServerKey, AuthMessage)
        let mut server_sig_hmac = Hmac::<Sha512>::new_from_slice(server_key)
            .map_err(|_| AuthError::Internal("Failed to create HMAC".to_string()))?;
        server_sig_hmac.update(auth_message.as_bytes());
        let server_signature = server_sig_hmac.finalize().into_bytes();
        
        Ok(server_signature.to_vec())
    }
    
    /// Parse client-first-message
    fn parse_client_first_message(&self, message: &str) -> AuthResult<(String, String)> {
        // Format: gs2-header client-first-bare
        // gs2-header = gs2-bind-flag "," [ authzid ] ","
        // gs2-bind-flag = "n" / "y" / "p"
        
        let parts: Vec<&str> = message.splitn(2, ',').collect();
        if parts.len() < 2 {
            return Err(AuthError::SaslError("Invalid client-first message format".to_string()));
        }
        
        let gs2_bind_flag = parts[0];
        if !["n", "y", "p"].contains(&gs2_bind_flag) {
            return Err(AuthError::SaslError("Invalid GS2 bind flag".to_string()));
        }
        
        // Find the second comma to extract authzid
        let remaining = parts[1];
        let comma_pos = remaining.find(',')
            .ok_or_else(|| AuthError::SaslError("Missing comma after authzid".to_string()))?;
        
        let authzid = &remaining[..comma_pos];
        let client_first_bare = &remaining[comma_pos + 1..];
        
        let gs2_header = format!("{},{},", gs2_bind_flag, authzid);
        
        Ok((gs2_header, client_first_bare.to_string()))
    }
    
    /// Parse client-first-bare message
    fn parse_client_first_bare(&self, message: &str) -> AuthResult<(String, String)> {
        // Format: [reserved-mext ","] username "," nonce ["," extensions]
        let mut username = None;
        let mut nonce = None;
        
        for part in message.split(',') {
            if let Some(value) = part.strip_prefix("n=") {
                username = Some(self.saslprep(value)?);
            } else if let Some(value) = part.strip_prefix("r=") {
                nonce = Some(value.to_string());
            }
            // Ignore other attributes like extensions
        }
        
        let username = username.ok_or_else(|| AuthError::SaslError("Missing username".to_string()))?;
        let nonce = nonce.ok_or_else(|| AuthError::SaslError("Missing nonce".to_string()))?;
        
        Ok((username, nonce))
    }
    
    /// Parse client-final-message
    fn parse_client_final_message(&self, message: &str) -> AuthResult<(String, String, String)> {
        // Format: channel-binding "," nonce "," proof
        let mut channel_binding = None;
        let mut nonce = None;
        let mut proof = None;
        
        for part in message.split(',') {
            if let Some(value) = part.strip_prefix("c=") {
                channel_binding = Some(value.to_string());
            } else if let Some(value) = part.strip_prefix("r=") {
                nonce = Some(value.to_string());
            } else if let Some(value) = part.strip_prefix("p=") {
                proof = Some(value.to_string());
            }
        }
        
        let channel_binding = channel_binding
            .ok_or_else(|| AuthError::SaslError("Missing channel binding".to_string()))?;
        let nonce = nonce
            .ok_or_else(|| AuthError::SaslError("Missing nonce".to_string()))?;
        let proof = proof
            .ok_or_else(|| AuthError::SaslError("Missing proof".to_string()))?;
        
        Ok((channel_binding, nonce, proof))
    }
    
    /// Simple SASLprep implementation (RFC 4013)
    /// In production, use a proper implementation
    fn saslprep(&self, input: &str) -> AuthResult<String> {
        // For now, just normalize and check for prohibited characters
        // A full implementation would handle Unicode normalization, etc.
        if input.contains('\0') || input.contains('\n') || input.contains('\r') {
            return Err(AuthError::SaslError("Invalid characters in username".to_string()));
        }
        Ok(input.to_string())
    }
}

impl Default for SaslAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_sasl_plain_auth() {
        let mut auth = SaslAuthenticator::new();
        auth.add_user("testuser".to_string(), "testpass".to_string()).unwrap();
        
        // Test successful authentication
        let auth_bytes = b"\0testuser\0testpass";
        let result = auth.authenticate(&SaslMechanism::Plain, auth_bytes);
        assert!(result.is_ok());
        let principal = result.unwrap();
        assert_eq!(principal.username, "testuser");
        assert!(principal.authenticated);
        assert_eq!(principal.mechanism.as_deref(), Some("PLAIN"));
        
        // Test failed authentication
        let auth_bytes = b"\0testuser\0wrongpass";
        let result = auth.authenticate(&SaslMechanism::Plain, auth_bytes);
        assert!(result.is_err());
    }
    
    #[test]
    fn test_scram_sha256_flow() {
        let mut auth = SaslAuthenticator::new();
        auth.add_user("user".to_string(), "pencil".to_string()).unwrap();
        
        // Simulate client-first message
        let client_nonce = "rOprNGfwEbeRWgbNEkqO";
        let client_first = format!("n,,n=user,r={}", client_nonce);
        
        // Process client-first message
        let session_id = "test-session";
        let server_first_bytes = auth.process_scram_sha256_first(session_id, client_first.as_bytes()).unwrap();
        let server_first = String::from_utf8(server_first_bytes).unwrap();
        
        // Parse server-first message
        assert!(server_first.starts_with(&format!("r={}", client_nonce)));
        assert!(server_first.contains(",s="));
        assert!(server_first.contains(",i="));
        
        // For a complete test, we would need to implement the client side
        // to calculate the correct proof. This test verifies the server-side flow.
    }
    
    #[test]
    fn test_scram_sha256_full_flow() {
        use hmac::Mac;
        
        let mut auth = SaslAuthenticator::new();
        
        // Create a user with known salt and iteration count for predictable testing
        let username = "user";
        let password = "pencil";
        let salt = b"QSXCR+Q6sek8bf92";
        let iteration_count = 4096u32;
        
        // Generate salted password
        let mut salted_password = [0u8; 32];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iteration_count, &mut salted_password);
        
        // Generate keys
        let mut client_key_hmac = Hmac::<Sha256>::new_from_slice(&salted_password).unwrap();
        client_key_hmac.update(b"Client Key");
        let client_key = client_key_hmac.finalize().into_bytes();
        let stored_key = Sha256::digest(&client_key).to_vec();
        
        let mut server_key_hmac = Hmac::<Sha256>::new_from_slice(&salted_password).unwrap();
        server_key_hmac.update(b"Server Key");
        let server_key = server_key_hmac.finalize().into_bytes().to_vec();
        
        // Add user with pre-calculated data
        auth.scram_users.insert(username.to_string(), ScramUserData {
            salt: salt.to_vec(),
            stored_key: stored_key.clone(),
            server_key: server_key.clone(),
            iteration_count,
        });
        
        // Step 1: Client-first message
        let client_nonce = "rOprNGfwEbeRWgbNEkqO";
        let client_first_bare = format!("n=user,r={}", client_nonce);
        let client_first = format!("n,,{}", client_first_bare);
        
        let session_id = "test-session";
        let server_first_bytes = auth.process_scram_sha256_first(session_id, client_first.as_bytes()).unwrap();
        let server_first = String::from_utf8(server_first_bytes).unwrap();
        
        // Extract server nonce from server-first message
        let server_nonce_start = server_first.find("r=").unwrap() + 2;
        let server_nonce_end = server_first[server_nonce_start..].find(',').unwrap() + server_nonce_start;
        let combined_nonce = &server_first[server_nonce_start..server_nonce_end];
        
        // Step 2: Calculate client proof
        let channel_binding = "biws"; // base64("n,,")
        let client_final_without_proof = format!("c={},r={}", channel_binding, combined_nonce);
        let auth_message = format!("{},{},{}", client_first_bare, server_first, client_final_without_proof);
        
        // Calculate client signature
        let mut client_sig_hmac = Hmac::<Sha256>::new_from_slice(&stored_key).unwrap();
        client_sig_hmac.update(auth_message.as_bytes());
        let client_signature = client_sig_hmac.finalize().into_bytes();
        
        // Calculate client proof = client_key XOR client_signature
        let mut client_proof = vec![0u8; client_key.len()];
        for i in 0..client_key.len() {
            client_proof[i] = client_key[i] ^ client_signature[i];
        }
        
        // Step 3: Client-final message
        let client_final = format!("{},p={}", client_final_without_proof, general_purpose::STANDARD.encode(&client_proof));
        
        let result = auth.process_scram_sha256_final(session_id, client_final.as_bytes());
        assert!(result.is_ok());
        
        let (principal, server_final_bytes) = result.unwrap();
        assert_eq!(principal.username, "user");
        assert!(principal.authenticated);
        assert_eq!(principal.mechanism.as_deref(), Some("SCRAM-SHA-256"));
        
        // Verify server signature
        let server_final = String::from_utf8(server_final_bytes).unwrap();
        assert!(server_final.starts_with("v="));
    }
    
    #[test]
    fn test_scram_sha512_flow() {
        let mut auth = SaslAuthenticator::new();
        auth.add_user_with_mechanism("user".to_string(), "pencil".to_string(), &SaslMechanism::ScramSha512).unwrap();
        
        // Simulate client-first message
        let client_nonce = "rOprNGfwEbeRWgbNEkqO";
        let client_first = format!("n,,n=user,r={}", client_nonce);
        
        // Process client-first message
        let session_id = "test-session";
        let server_first_bytes = auth.process_scram_sha512_first(session_id, client_first.as_bytes()).unwrap();
        let server_first = String::from_utf8(server_first_bytes).unwrap();
        
        // Parse server-first message
        assert!(server_first.starts_with(&format!("r={}", client_nonce)));
        assert!(server_first.contains(",s="));
        assert!(server_first.contains(",i="));
    }
    
    #[test]
    fn test_scram_message_parsing() {
        let auth = SaslAuthenticator::new();
        
        // Test client-first message parsing
        let client_first = "n,,n=user,r=clientnonce";
        let (gs2_header, client_first_bare) = auth.parse_client_first_message(client_first).unwrap();
        assert_eq!(gs2_header, "n,,");
        assert_eq!(client_first_bare, "n=user,r=clientnonce");
        
        // Test client-first-bare parsing
        let (username, nonce) = auth.parse_client_first_bare(&client_first_bare).unwrap();
        assert_eq!(username, "user");
        assert_eq!(nonce, "clientnonce");
        
        // Test client-final message parsing
        let client_final = "c=biws,r=clientnonceservernonce,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=";
        let (channel_binding, nonce, proof) = auth.parse_client_final_message(client_final).unwrap();
        assert_eq!(channel_binding, "biws");
        assert_eq!(nonce, "clientnonceservernonce");
        assert_eq!(proof, "v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=");
    }
    
    #[test]
    fn test_saslprep() {
        let auth = SaslAuthenticator::new();
        
        // Valid username
        assert_eq!(auth.saslprep("validuser").unwrap(), "validuser");
        
        // Invalid usernames
        assert!(auth.saslprep("user\0name").is_err());
        assert!(auth.saslprep("user\nname").is_err());
        assert!(auth.saslprep("user\rname").is_err());
    }
    
    #[test]
    fn test_handshake() {
        let auth = SaslAuthenticator::new();
        
        // Test with no requested mechanisms (returns all supported)
        let mechanisms = auth.handshake(&[]);
        assert_eq!(mechanisms.len(), 3);
        assert!(mechanisms.contains(&"PLAIN".to_string()));
        assert!(mechanisms.contains(&"SCRAM-SHA-256".to_string()));
        assert!(mechanisms.contains(&"SCRAM-SHA-512".to_string()));
        
        // Test with specific requested mechanisms
        let requested = vec!["PLAIN".to_string(), "GSSAPI".to_string()];
        let mechanisms = auth.handshake(&requested);
        assert_eq!(mechanisms.len(), 1);
        assert_eq!(mechanisms[0], "PLAIN");
    }
}