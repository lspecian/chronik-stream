//! TLS support for Kafka Protocol connections
//!
//! This module provides TLS encryption for Kafka client connections using rustls.
//! It supports configurable certificates and optional client authentication.
//!
//! # Usage
//!
//! ```bash
//! # Generate self-signed certificates for testing
//! openssl req -x509 -newkey rsa:4096 -keyout server.key -out server.crt -days 365 -nodes
//!
//! # Start server with TLS
//! CHRONIK_TLS_CERT=/path/to/server.crt \
//! CHRONIK_TLS_KEY=/path/to/server.key \
//! chronik-server start
//! ```

use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

/// TLS configuration for Kafka connections
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// Path to the server certificate file (PEM format)
    pub cert_path: String,
    /// Path to the server private key file (PEM format)
    pub key_path: String,
    /// Optional path to CA certificate for client authentication
    pub ca_cert_path: Option<String>,
    /// Require client certificates (mTLS)
    pub require_client_cert: bool,
}

impl TlsConfig {
    /// Create TLS config from environment variables
    ///
    /// - `CHRONIK_TLS_CERT`: Path to server certificate (required for TLS)
    /// - `CHRONIK_TLS_KEY`: Path to server private key (required for TLS)
    /// - `CHRONIK_TLS_CA_CERT`: Path to CA certificate (optional, for mTLS)
    /// - `CHRONIK_TLS_REQUIRE_CLIENT_CERT`: Set to "true" to require client certs
    pub fn from_env() -> Option<Self> {
        let cert_path = std::env::var("CHRONIK_TLS_CERT").ok()?;
        let key_path = std::env::var("CHRONIK_TLS_KEY").ok()?;

        Some(Self {
            cert_path,
            key_path,
            ca_cert_path: std::env::var("CHRONIK_TLS_CA_CERT").ok(),
            require_client_cert: std::env::var("CHRONIK_TLS_REQUIRE_CLIENT_CERT")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
        })
    }

    /// Create TLS config from paths
    pub fn new(cert_path: impl Into<String>, key_path: impl Into<String>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            ca_cert_path: None,
            require_client_cert: false,
        }
    }

    /// Enable mutual TLS (mTLS) with client certificate verification
    pub fn with_client_auth(mut self, ca_cert_path: impl Into<String>) -> Self {
        self.ca_cert_path = Some(ca_cert_path.into());
        self.require_client_cert = true;
        self
    }
}

/// Load certificates from a PEM file
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open certificate file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to parse certificates from: {}", path.display()))?;

    if certs.is_empty() {
        anyhow::bail!("No certificates found in: {}", path.display());
    }

    Ok(certs)
}

/// Load private key from a PEM file
fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open private key file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    // Try to read PKCS#8 key first, then RSA key, then EC key
    let keys = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("Failed to parse private key from: {}", path.display()))?;

    match keys {
        Some(key) => Ok(key),
        None => anyhow::bail!("No private key found in: {}", path.display()),
    }
}

/// Create a TLS acceptor from configuration
pub fn create_tls_acceptor(config: &TlsConfig) -> Result<TlsAcceptor> {
    info!("Loading TLS certificate from: {}", config.cert_path);
    info!("Loading TLS private key from: {}", config.key_path);

    let certs = load_certs(Path::new(&config.cert_path))?;
    let key = load_private_key(Path::new(&config.key_path))?;

    info!("Loaded {} certificate(s)", certs.len());

    // Build server config
    let server_config = if let Some(ref ca_path) = config.ca_cert_path {
        // mTLS: require client certificates
        info!("Loading CA certificate for client auth from: {}", ca_path);
        let ca_certs = load_certs(Path::new(ca_path))?;

        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store.add(cert)
                .context("Failed to add CA certificate to root store")?;
        }

        let client_cert_verifier = rustls::server::WebPkiClientVerifier::builder(
            Arc::new(root_store)
        )
        .build()
        .context("Failed to build client certificate verifier")?;

        ServerConfig::builder()
            .with_client_cert_verifier(client_cert_verifier)
            .with_single_cert(certs, key)
            .context("Failed to build TLS server config with client auth")?
    } else {
        // No client authentication
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("Failed to build TLS server config")?
    };

    info!("TLS configuration loaded successfully");
    if config.require_client_cert {
        info!("mTLS enabled: client certificates required");
    }

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// TLS-wrapped listener that can accept both TLS and plain connections
pub struct TlsListener {
    acceptor: Option<TlsAcceptor>,
}

impl TlsListener {
    /// Create a new TLS listener
    pub fn new(tls_config: Option<&TlsConfig>) -> Result<Self> {
        let acceptor = if let Some(config) = tls_config {
            Some(create_tls_acceptor(config)?)
        } else {
            warn!("TLS not configured - connections will be unencrypted");
            warn!("Set CHRONIK_TLS_CERT and CHRONIK_TLS_KEY to enable TLS");
            None
        };

        Ok(Self { acceptor })
    }

    /// Check if TLS is enabled
    pub fn is_tls_enabled(&self) -> bool {
        self.acceptor.is_some()
    }

    /// Get the TLS acceptor (if TLS is enabled)
    pub fn acceptor(&self) -> Option<&TlsAcceptor> {
        self.acceptor.as_ref()
    }
}

/// A stream that can be either plain TCP or TLS-wrapped
///
/// This abstraction allows the server to handle both encrypted and unencrypted
/// connections uniformly.
pub enum MaybeTlsStream {
    /// Plain TCP connection (no encryption)
    Plain(TcpStream),
    /// TLS-encrypted connection
    Tls(TlsStream<TcpStream>),
}

impl MaybeTlsStream {
    /// Get the remote peer address
    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            MaybeTlsStream::Plain(stream) => stream.peer_addr(),
            MaybeTlsStream::Tls(stream) => stream.get_ref().0.peer_addr(),
        }
    }

    /// Set TCP_NODELAY option
    pub fn set_nodelay(&self, nodelay: bool) -> std::io::Result<()> {
        match self {
            MaybeTlsStream::Plain(stream) => stream.set_nodelay(nodelay),
            MaybeTlsStream::Tls(stream) => stream.get_ref().0.set_nodelay(nodelay),
        }
    }

    /// Check if this is a TLS connection
    pub fn is_tls(&self) -> bool {
        matches!(self, MaybeTlsStream::Tls(_))
    }
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_flush(cx),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            MaybeTlsStream::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Split halves for a MaybeTlsStream
pub struct MaybeTlsReadHalf {
    inner: MaybeTlsReadHalfInner,
}

enum MaybeTlsReadHalfInner {
    Plain(tokio::net::tcp::OwnedReadHalf),
    Tls(tokio::io::ReadHalf<TlsStream<TcpStream>>),
}

pub struct MaybeTlsWriteHalf {
    inner: MaybeTlsWriteHalfInner,
}

enum MaybeTlsWriteHalfInner {
    Plain(tokio::net::tcp::OwnedWriteHalf),
    Tls(tokio::io::WriteHalf<TlsStream<TcpStream>>),
}

impl MaybeTlsStream {
    /// Split the stream into read and write halves
    pub fn into_split(self) -> (MaybeTlsReadHalf, MaybeTlsWriteHalf) {
        match self {
            MaybeTlsStream::Plain(stream) => {
                let (read, write) = stream.into_split();
                (
                    MaybeTlsReadHalf { inner: MaybeTlsReadHalfInner::Plain(read) },
                    MaybeTlsWriteHalf { inner: MaybeTlsWriteHalfInner::Plain(write) },
                )
            }
            MaybeTlsStream::Tls(stream) => {
                let (read, write) = tokio::io::split(stream);
                (
                    MaybeTlsReadHalf { inner: MaybeTlsReadHalfInner::Tls(read) },
                    MaybeTlsWriteHalf { inner: MaybeTlsWriteHalfInner::Tls(write) },
                )
            }
        }
    }
}

impl AsyncRead for MaybeTlsReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().inner {
            MaybeTlsReadHalfInner::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            MaybeTlsReadHalfInner::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.get_mut().inner {
            MaybeTlsWriteHalfInner::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            MaybeTlsWriteHalfInner::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().inner {
            MaybeTlsWriteHalfInner::Plain(stream) => Pin::new(stream).poll_flush(cx),
            MaybeTlsWriteHalfInner::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().inner {
            MaybeTlsWriteHalfInner::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            MaybeTlsWriteHalfInner::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Accept connections with optional TLS
///
/// This struct wraps a TCP listener and optional TLS acceptor to provide
/// a unified interface for accepting both plain and TLS connections.
pub struct TlsConnectionAcceptor {
    listener: TcpListener,
    tls_acceptor: Option<TlsAcceptor>,
}

impl TlsConnectionAcceptor {
    /// Create a new connection acceptor
    pub async fn bind(bind_addr: &str, tls_config: Option<&TlsConfig>) -> Result<Self> {
        let listener = TcpListener::bind(bind_addr).await
            .with_context(|| format!("Failed to bind to {}", bind_addr))?;

        let tls_acceptor = if let Some(config) = tls_config {
            Some(create_tls_acceptor(config)?)
        } else {
            None
        };

        if tls_acceptor.is_some() {
            info!("TLS listener bound to {} (encrypted)", bind_addr);
        } else {
            info!("TCP listener bound to {} (unencrypted)", bind_addr);
        }

        Ok(Self { listener, tls_acceptor })
    }

    /// Accept a new connection (performs TLS handshake if TLS is enabled)
    pub async fn accept(&self) -> Result<(MaybeTlsStream, SocketAddr)> {
        let (tcp_stream, addr) = self.listener.accept().await
            .context("Failed to accept TCP connection")?;

        debug!("Accepted TCP connection from {}", addr);

        // Perform TLS handshake if TLS is enabled
        let stream = if let Some(ref acceptor) = self.tls_acceptor {
            match acceptor.accept(tcp_stream).await {
                Ok(tls_stream) => {
                    debug!("TLS handshake completed for {}", addr);
                    MaybeTlsStream::Tls(tls_stream)
                }
                Err(e) => {
                    error!("TLS handshake failed for {}: {}", addr, e);
                    return Err(anyhow::anyhow!("TLS handshake failed: {}", e));
                }
            }
        } else {
            MaybeTlsStream::Plain(tcp_stream)
        };

        Ok((stream, addr))
    }

    /// Check if TLS is enabled
    pub fn is_tls_enabled(&self) -> bool {
        self.tls_acceptor.is_some()
    }

    /// Get local address
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_config_from_env_disabled() {
        // Clear any existing env vars
        std::env::remove_var("CHRONIK_TLS_CERT");
        std::env::remove_var("CHRONIK_TLS_KEY");

        let config = TlsConfig::from_env();
        assert!(config.is_none());
    }

    #[test]
    fn test_tls_config_new() {
        let config = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem");
        assert_eq!(config.cert_path, "/path/to/cert.pem");
        assert_eq!(config.key_path, "/path/to/key.pem");
        assert!(!config.require_client_cert);
    }

    #[test]
    fn test_tls_config_with_client_auth() {
        let config = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem")
            .with_client_auth("/path/to/ca.pem");

        assert_eq!(config.ca_cert_path, Some("/path/to/ca.pem".to_string()));
        assert!(config.require_client_cert);
    }
}
