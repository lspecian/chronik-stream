//! TLS/SSL support for Chronik Stream
//!
//! Provides TLS encryption for Kafka protocol connections using rustls.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use anyhow::{Result, Context};
use rustls::{ServerConfig, RootCertStore};
use rustls_pemfile::{certs, private_key};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn, debug};

/// TLS configuration for the server
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Path to the certificate file (PEM format)
    pub cert_path: String,
    /// Path to the private key file (PEM format)
    pub key_path: String,
    /// Optional path to CA certificate for client authentication
    pub ca_cert_path: Option<String>,
    /// Whether to require client certificates
    pub require_client_cert: bool,
}

impl TlsConfig {
    /// Create a new TLS configuration
    pub fn new(cert_path: String, key_path: String) -> Self {
        Self {
            cert_path,
            key_path,
            ca_cert_path: None,
            require_client_cert: false,
        }
    }

    /// Enable mutual TLS (mTLS) with client certificate verification
    pub fn with_client_auth(mut self, ca_cert_path: String, required: bool) -> Self {
        self.ca_cert_path = Some(ca_cert_path);
        self.require_client_cert = required;
        self
    }
}

/// TLS acceptor for incoming connections
pub struct TlsManager {
    acceptor: TlsAcceptor,
    config: TlsConfig,
}

impl TlsManager {
    /// Create a new TLS manager from configuration
    pub fn new(config: TlsConfig) -> Result<Self> {
        let acceptor = create_tls_acceptor(&config)?;
        Ok(Self { acceptor, config })
    }

    /// Get the TLS acceptor for accepting connections
    pub fn acceptor(&self) -> &TlsAcceptor {
        &self.acceptor
    }

    /// Reload certificates from disk (useful for certificate rotation)
    pub fn reload_certificates(&mut self) -> Result<()> {
        info!("Reloading TLS certificates");
        self.acceptor = create_tls_acceptor(&self.config)?;
        info!("TLS certificates reloaded successfully");
        Ok(())
    }
}

/// Create a TLS acceptor from configuration
fn create_tls_acceptor(config: &TlsConfig) -> Result<TlsAcceptor> {
    // Load server certificate
    let cert_file = File::open(&config.cert_path)
        .with_context(|| format!("Failed to open certificate file: {}", config.cert_path))?;
    let mut cert_reader = BufReader::new(cert_file);
    let cert_chain = certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse certificate chain")?;

    if cert_chain.is_empty() {
        anyhow::bail!("No certificates found in {}", config.cert_path);
    }

    // Load private key
    let key_file = File::open(&config.key_path)
        .with_context(|| format!("Failed to open key file: {}", config.key_path))?;
    let mut key_reader = BufReader::new(key_file);
    let key = private_key(&mut key_reader)
        .context("Failed to parse private key")?
        .ok_or_else(|| anyhow::anyhow!("No private key found in {}", config.key_path))?;

    // Create server config
    let mut server_config = ServerConfig::builder();

    // Configure client authentication if enabled
    let server_config = if let Some(ref ca_path) = config.ca_cert_path {
        info!("Configuring mutual TLS with CA certificate: {}", ca_path);

        // Load CA certificate for client verification
        let ca_file = File::open(ca_path)
            .with_context(|| format!("Failed to open CA certificate file: {}", ca_path))?;
        let mut ca_reader = BufReader::new(ca_file);
        let ca_certs = certs(&mut ca_reader)
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to parse CA certificates")?;

        let mut root_store = RootCertStore::empty();
        for cert in ca_certs {
            root_store.add(cert)
                .context("Failed to add CA certificate to root store")?;
        }

        let verifier = if config.require_client_cert {
            rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
                .build()
                .context("Failed to create client certificate verifier")?
        } else {
            rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
                .allow_unauthenticated()
                .build()
                .context("Failed to create optional client certificate verifier")?
        };

        server_config.with_client_cert_verifier(verifier)
    } else {
        server_config.with_no_client_auth()
    };

    let server_config = server_config
        .with_single_cert(cert_chain, key.into())
        .context("Failed to create TLS server configuration")?;

    info!("TLS configuration created successfully");
    if config.require_client_cert {
        info!("Client certificate verification: REQUIRED");
    } else if config.ca_cert_path.is_some() {
        info!("Client certificate verification: OPTIONAL");
    } else {
        info!("Client certificate verification: DISABLED");
    }

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Check if TLS certificates exist and are readable
pub fn validate_tls_paths(cert_path: &str, key_path: &str) -> Result<()> {
    if !Path::new(cert_path).exists() {
        anyhow::bail!("Certificate file not found: {}", cert_path);
    }
    if !Path::new(key_path).exists() {
        anyhow::bail!("Private key file not found: {}", key_path);
    }

    // Try to open files to check permissions
    File::open(cert_path)
        .with_context(|| format!("Cannot read certificate file: {}", cert_path))?;
    File::open(key_path)
        .with_context(|| format!("Cannot read private key file: {}", key_path))?;

    debug!("TLS certificate and key files validated successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_cert() -> (NamedTempFile, NamedTempFile) {
        // Sample self-signed certificate for testing (NOT for production use)
        let cert_pem = r#"-----BEGIN CERTIFICATE-----
MIIDazCCAlOgAwIBAgIUJeohtgk8nnt8ofratXJg7KUJsI4wDQYJKoZIhvcNAQEL
BQAwRTELMAkGA1UEBhMCVVMxEzARBgNVBAgMClNvbWUtU3RhdGUxITAfBgNVBAoM
GEludGVybmV0IFdpZGdpdHMgUHR5IEx0ZDAeFw0yNDAxMDEwMDAwMDBaFw0yNTAx
MDEwMDAwMDBaMEUxCzAJBgNVBAYTAlVTMRMwEQYDVQQIDApTb21lLVN0YXRlMSEw
HwYDVQQKDBhJbnRlcm5ldCBXaWRnaXRzIFB0eSBMdGQwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQC7W8rTAzhBuLjz/E4Y8EJ3z3vJhYvWBjCpvpBOvShd
VEYz5bJMiqZKJQq0T2BqGHQiY2dTs4kLqp5MOkDz7YjEIn1zJIDNKR/w9F1sE0m1
tqj9iGaPgG4PUQ3VjBl6JdNfF4IQsZ5dMSwGSIQM0nYBhoBWWeBJxGhQ8+KvMRy5
L/4YXP4HWIP9FKKLtVEKJR0vbKIYRCdFiKTF8OT2ELCLdD7lJ0m9aGBz8g9v5+0l
Y0M6F0v3HItLQi4wH5Eh/+mEg8xtpIyG1YUQiwLymfLFpKKKTk5dTdDZZgdxLqJt
rM2GWV5JoZVzJaH1TjmdqRkVDlOZwx05iI8p7VLfEpKXAgMBAAGjUzBRMB0GA1Ud
DgQWBBRMqcUIK5BLx0Wa3rVj7/aL9RklOzAfBgNVHSMEGDAWgBRMqcUIK5BLx0Wa
3rVj7/aL9RklOzAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQCP
Xcdx8S4A7N2V1s9kd2KzSF5Yqrt2/qYu2KO2A2L7r3H3N6JpJ8wmVmTbLz8leI4D
-----END CERTIFICATE-----"#;

        let key_pem = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7W8rTAzhBuLjz
/E4Y8EJ3z3vJhYvWBjCpvpBOvShdVEYz5bJMiqZKJQq0T2BqGHQiY2dTs4kLqp5M
OkDz7YjEIn1zJIDNKR/w9F1sE0m1tqj9iGaPgG4PUQ3VjBl6JdNfF4IQsZ5dMSwG
SIQM0nYBhoBWWeBJxGhQ8+KvMRy5L/4YXP4HWIP9FKKLtVEKJR0vbKIYRCdFiKTF
8OT2ELCLdD7lJ0m9aGBz8g9v5+0lY0M6F0v3HItLQi4wH5Eh/+mEg8xtpIyG1YUQ
iwLymfLFpKKKTk5dTdDZZgdxLqJtrM2GWV5JoZVzJaH1TjmdqRkVDlOZwx05iI8p
7VLfEpKXAgMBAAECggEAG0k8Pp8SH5f9CUmuLb9dG3mcxJIUQigjJ8yL9Zrxcphs
-----END PRIVATE KEY-----"#;

        let mut cert_file = NamedTempFile::new().unwrap();
        cert_file.write_all(cert_pem.as_bytes()).unwrap();

        let mut key_file = NamedTempFile::new().unwrap();
        key_file.write_all(key_pem.as_bytes()).unwrap();

        (cert_file, key_file)
    }

    #[test]
    fn test_validate_tls_paths() {
        let (cert_file, key_file) = create_test_cert();

        // Should succeed with valid paths
        assert!(validate_tls_paths(
            cert_file.path().to_str().unwrap(),
            key_file.path().to_str().unwrap()
        ).is_ok());

        // Should fail with non-existent paths
        assert!(validate_tls_paths(
            "/non/existent/cert.pem",
            "/non/existent/key.pem"
        ).is_err());
    }
}