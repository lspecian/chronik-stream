//! TLS configuration and utilities.

use crate::{AuthError, AuthResult};
use rustls::{
    Certificate, PrivateKey, ServerConfig, ClientConfig,
};
use rustls_pemfile::{certs as read_certs, pkcs8_private_keys};
use std::{
    fs::File,
    io::BufReader,
    sync::Arc,
};
use tokio_rustls::{TlsAcceptor as TokioTlsAcceptor, TlsConnector as TokioTlsConnector};

/// TLS configuration
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Certificate file path
    pub cert_file: String,
    
    /// Private key file path
    pub key_file: String,
    
    /// CA certificate file path (optional)
    pub ca_file: Option<String>,
    
    /// Whether to verify client certificates
    pub verify_client: bool,
    
    /// Minimum TLS version (e.g., "1.2", "1.3")
    pub min_version: String,
}

/// TLS acceptor for servers
#[derive(Clone)]
pub struct TlsAcceptor {
    inner: TokioTlsAcceptor,
}

impl TlsAcceptor {
    /// Create new TLS acceptor from config
    pub fn new(config: &TlsConfig) -> AuthResult<Self> {
        let certs = load_certs(&config.cert_file)?;
        let key = load_private_key(&config.key_file)?;
        
        let server_config = ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| AuthError::TlsError(e.to_string()))?;
        
        // Note: In rustls 0.21, version configuration is done via with_safe_defaults()
        // which includes TLS 1.2 and 1.3 by default
        
        // TODO: Add client certificate verification if needed
        
        Ok(Self {
            inner: TokioTlsAcceptor::from(Arc::new(server_config)),
        })
    }
    
    /// Get the inner acceptor
    pub fn inner(&self) -> &TokioTlsAcceptor {
        &self.inner
    }
}

/// TLS connector for clients
#[derive(Clone)]
pub struct TlsConnector {
    inner: TokioTlsConnector,
}

impl TlsConnector {
    /// Create new TLS connector from config
    pub fn new(config: &TlsConfig) -> AuthResult<Self> {
        let mut root_cert_store = rustls::RootCertStore::empty();
        
        // Add CA certificates if provided
        if let Some(ca_file) = &config.ca_file {
            let ca_certs = load_certs(ca_file)?;
            for cert in ca_certs {
                root_cert_store.add(&cert)
                    .map_err(|e| AuthError::TlsError(e.to_string()))?;
            }
        } else {
            // Use webpki roots
            let anchors = webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
                rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                    ta.subject,
                    ta.spki,
                    ta.name_constraints,
                )
            });
            root_cert_store.add_trust_anchors(anchors);
        }
        
        let client_config = ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();
        
        Ok(Self {
            inner: TokioTlsConnector::from(Arc::new(client_config)),
        })
    }
    
    /// Get the inner connector
    pub fn inner(&self) -> &TokioTlsConnector {
        &self.inner
    }
}

/// Load certificates from file
fn load_certs(path: &str) -> AuthResult<Vec<Certificate>> {
    let file = File::open(path)
        .map_err(|e| AuthError::TlsError(format!("Failed to open cert file: {}", e)))?;
    let mut reader = BufReader::new(file);
    
    match read_certs(&mut reader) {
        Ok(certs) => {
            let cert_vec: Vec<Certificate> = certs.into_iter()
                .map(Certificate)
                .collect();
            
            if cert_vec.is_empty() {
                Err(AuthError::TlsError("No certificates found".to_string()))
            } else {
                Ok(cert_vec)
            }
        }
        Err(_) => Err(AuthError::TlsError("Failed to parse certificates".to_string())),
    }
}

/// Load private key from file
fn load_private_key(path: &str) -> AuthResult<PrivateKey> {
    let file = File::open(path)
        .map_err(|e| AuthError::TlsError(format!("Failed to open key file: {}", e)))?;
    let mut reader = BufReader::new(file);
    
    match pkcs8_private_keys(&mut reader) {
        Ok(keys) => {
            if keys.is_empty() {
                Err(AuthError::TlsError("No private key found".to_string()))
            } else {
                Ok(PrivateKey(keys[0].clone()))
            }
        }
        Err(_) => Err(AuthError::TlsError("Failed to parse private key".to_string())),
    }
}