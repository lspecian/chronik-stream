//! JWT token management.

use crate::{AuthError, AuthResult};
use chrono::{Duration, Utc};
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// JWT configuration
#[derive(Debug, Clone)]
pub struct JwtConfig {
    /// Secret key for signing tokens
    pub secret: String,
    
    /// Token expiration duration in seconds
    pub expiration_secs: i64,
    
    /// Token issuer
    pub issuer: String,
    
    /// Token audience
    pub audience: String,
}

/// JWT claims
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (user ID)
    pub sub: String,
    
    /// Issued at
    pub iat: i64,
    
    /// Expiration
    pub exp: i64,
    
    /// Not before
    pub nbf: i64,
    
    /// Issuer
    pub iss: String,
    
    /// Audience
    pub aud: String,
    
    /// JWT ID
    pub jti: String,
    
    /// Username
    pub username: String,
    
    /// Permissions
    pub permissions: Vec<String>,
}

/// JWT manager
pub struct JwtManager {
    config: JwtConfig,
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    validation: Validation,
}

impl JwtManager {
    /// Create new JWT manager
    pub fn new(config: JwtConfig) -> Self {
        let encoding_key = EncodingKey::from_secret(config.secret.as_bytes());
        let decoding_key = DecodingKey::from_secret(config.secret.as_bytes());
        
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[config.issuer.clone()]);
        validation.set_audience(&[config.audience.clone()]);
        
        Self {
            config,
            encoding_key,
            decoding_key,
            validation,
        }
    }
    
    /// Generate token for user
    pub fn generate_token(
        &self,
        username: &str,
        permissions: Vec<String>,
    ) -> AuthResult<String> {
        let now = Utc::now();
        let exp = now + Duration::seconds(self.config.expiration_secs);
        
        let claims = Claims {
            sub: username.to_string(),
            iat: now.timestamp(),
            exp: exp.timestamp(),
            nbf: now.timestamp(),
            iss: self.config.issuer.clone(),
            aud: self.config.audience.clone(),
            jti: Uuid::new_v4().to_string(),
            username: username.to_string(),
            permissions,
        };
        
        let header = Header::new(Algorithm::HS256);
        
        encode(&header, &claims, &self.encoding_key)
            .map_err(|e| AuthError::TokenInvalid(e.to_string()))
    }
    
    /// Verify and decode token
    pub fn verify_token(&self, token: &str) -> AuthResult<Claims> {
        decode::<Claims>(token, &self.decoding_key, &self.validation)
            .map(|data| data.claims)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                _ => AuthError::TokenInvalid(e.to_string()),
            })
    }
    
    /// Refresh token
    pub fn refresh_token(&self, token: &str) -> AuthResult<String> {
        let claims = self.verify_token(token)?;
        self.generate_token(&claims.username, claims.permissions)
    }
}