//! Admin API configuration.

use serde::{Deserialize, Serialize};

/// Admin API configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    /// HTTP server configuration
    pub server: ServerConfig,
    
    /// Database configuration
    pub database: DatabaseConfig,
    
    /// Controller configuration
    pub controller: ControllerConfig,
    
    /// Authentication configuration
    pub auth: AuthConfig,
}

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Listen address
    pub address: String,
    
    /// Listen port
    pub port: u16,
    
    /// TLS configuration
    pub tls: Option<TlsConfig>,
}

/// TLS configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Certificate file
    pub cert_file: String,
    
    /// Key file
    pub key_file: String,
}

/// Database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Database URL
    pub url: String,
    
    /// Maximum connections
    pub max_connections: u32,
}

/// Controller configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerConfig {
    /// Controller endpoints
    pub endpoints: Vec<String>,
    
    /// Connection timeout in seconds
    pub timeout_secs: u64,
}

/// Authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Enable authentication
    pub enabled: bool,
    
    /// JWT secret
    pub jwt_secret: String,
    
    /// Token expiration in seconds
    pub token_expiration_secs: i64,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                address: "0.0.0.0".to_string(),
                port: 8080,
                tls: None,
            },
            database: DatabaseConfig {
                url: "postgres://localhost/chronik".to_string(),
                max_connections: 10,
            },
            controller: ControllerConfig {
                endpoints: vec!["localhost:9090".to_string()],
                timeout_secs: 30,
            },
            auth: AuthConfig {
                enabled: true,
                jwt_secret: "change-me-in-production".to_string(),
                token_expiration_secs: 3600,
            },
        }
    }
}