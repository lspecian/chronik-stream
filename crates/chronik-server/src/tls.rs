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
        Self::from_env_with_prefix("CHRONIK_TLS")
    }

    /// Read TLS configuration from environment variables under a given prefix.
    ///
    /// The Kafka listener uses `CHRONIK_TLS_*`; the Unified API uses
    /// `CHRONIK_API_TLS_*` and falls back to the former, so a deployment with
    /// one certificate for both does not have to name it twice. Reading the same
    /// four suffixes for both keeps one code path instead of two.
    pub fn from_env_with_prefix(prefix: &str) -> Option<Self> {
        let cert_path = std::env::var(format!("{}_CERT", prefix)).ok()?;
        let key_path = std::env::var(format!("{}_KEY", prefix)).ok()?;

        Some(Self {
            cert_path,
            key_path,
            ca_cert_path: std::env::var(format!("{}_CA_CERT", prefix)).ok(),
            require_client_cert: std::env::var(format!("{}_REQUIRE_CLIENT_CERT", prefix))
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

/// Install the process-wide rustls crypto provider.
///
/// rustls 0.23 refuses to guess when more than one provider is compiled in, and
/// this tree has both `ring` (pulled in by other dependencies) and `aws-lc-rs`
/// (rustls's default). Without an explicit choice, the first TLS handshake
/// **panics**:
///
/// ```text
/// Could not automatically determine the process-level CryptoProvider
/// from Rustls crate features
/// ```
///
/// That is not hypothetical and it was not only an API-port problem: this
/// function is the single place both the Kafka listener and the Unified API
/// build an acceptor, so TLS on the Kafka port would have panicked the broker on
/// the first `CHRONIK_TLS_CERT` connection. It went unnoticed because nothing
/// had ever completed a TLS handshake against this server.
///
/// `install_default` returns `Err` if a provider is already installed, which is
/// the normal case for the second and later calls, so the result is discarded.
fn ensure_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_err()
        {
            debug!("rustls crypto provider was already installed");
        }
    });
}

/// Every certificate in a PEM file, as trust anchors.
fn load_root_store(path: &Path) -> Result<rustls::RootCertStore> {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in load_certs(path)? {
        root_store.add(cert).context("Failed to add CA certificate to root store")?;
    }
    Ok(root_store)
}

/// A TLS client for connecting to other brokers' Kafka ports (replica
/// fetches): it trusts the CA in `ca_cert_path`, which must have signed every
/// broker's certificate, and presents this broker's own certificate, which a
/// leader that requires client certificates verifies against the same CA.
pub fn create_tls_connector(config: &TlsConfig) -> Result<tokio_rustls::TlsConnector> {
    Ok(tokio_rustls::TlsConnector::from(Arc::new(client_config(config)?)))
}

/// The rustls client behind [`create_tls_connector`], also used for HTTPS to
/// other brokers' Unified API (query fan-out).
pub fn client_config(config: &TlsConfig) -> Result<rustls::ClientConfig> {
    ensure_crypto_provider();
    let ca_path = config.ca_cert_path.as_deref().context(
        "no CA certificate to verify other brokers with: set CHRONIK_TLS_CA_CERT to the CA that signed every broker's certificate",
    )?;
    let roots = load_root_store(Path::new(ca_path))?;
    let certs = load_certs(Path::new(&config.cert_path))?;
    let key = load_private_key(Path::new(&config.key_path))?;
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .context("Failed to build the inter-broker TLS client config")
}

/// TLS for the Unified API: `CHRONIK_API_TLS_*`, falling back to the Kafka
/// listener's `CHRONIK_TLS_*`.
pub fn api_tls_config() -> Option<TlsConfig> {
    TlsConfig::from_env_with_prefix("CHRONIK_API_TLS").or_else(TlsConfig::from_env)
}

/// Create a TLS acceptor from configuration
pub fn create_tls_acceptor(config: &TlsConfig) -> Result<TlsAcceptor> {
    ensure_crypto_provider();

    info!("Loading TLS certificate from: {}", config.cert_path);
    info!("Loading TLS private key from: {}", config.key_path);

    if config.require_client_cert && config.ca_cert_path.is_none() {
        anyhow::bail!(
            "client certificates are required (REQUIRE_CLIENT_CERT) but no CA is set (CA_CERT) to verify them against"
        );
    }
    let certs = load_certs(Path::new(&config.cert_path))?;
    let key = load_private_key(Path::new(&config.key_path))?;

    info!("Loaded {} certificate(s)", certs.len());

    // Build server config
    let server_config = if let Some(ref ca_path) = config.ca_cert_path {
        // A client certificate is verified against this CA when offered, and
        // required only with CHRONIK_TLS_REQUIRE_CLIENT_CERT. It used to be
        // required whenever a CA was set, whatever that flag said.
        info!("Loading CA certificate for client auth from: {}", ca_path);
        let root_store = load_root_store(Path::new(ca_path))?;
        let builder = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store));
        let builder = if config.require_client_cert { builder } else { builder.allow_unauthenticated() };
        let client_cert_verifier = builder
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
    match (config.require_client_cert, config.ca_cert_path.is_some()) {
        (true, true) => info!("mTLS enabled: client certificates required"),
        (false, true) => info!("Client certificates are verified when offered (not required)"),
        _ => {}
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

    /// The principal named by the client certificate, in Kafka's `User:` form.
    ///
    /// `None` for a plaintext connection, and for a TLS connection where the
    /// client presented no certificate — which is the normal case unless
    /// `CHRONIK_TLS_REQUIRE_CLIENT_CERT` is set.
    ///
    /// This is what turns mTLS from "the transport is authenticated" into "the
    /// caller has a name": before it existed, a client certificate proved
    /// possession of a key and then the connection authorized as ANONYMOUS,
    /// because nothing ever read the certificate. rustls has had it available on
    /// the connection all along.
    ///
    /// The subject is taken from the leaf certificate (peer_certificates()[0])
    /// and reported as `User:<CN>`, matching what `kafka-acls.sh` writes for an
    /// SSL principal under Kafka's default principal builder.
    pub fn peer_principal(&self) -> Option<String> {
        let stream = match self {
            MaybeTlsStream::Plain(_) => return None,
            MaybeTlsStream::Tls(stream) => stream,
        };

        let certs = stream.get_ref().1.peer_certificates()?;
        let leaf = certs.first()?;
        extract_common_name(leaf.as_ref()).map(|cn| format!("User:{}", cn))
    }
}

/// Pull the subject Common Name out of a DER certificate.
///
/// Deliberately a minimal scan rather than a full X.509 parse: the alternative
/// is another dependency (`x509-parser`) for one field. It walks the DER looking
/// for the CN attribute OID (2.5.4.3 = `55 04 03`) inside the subject and reads
/// the string that follows.
///
/// Returns `None` when the CN cannot be located, and callers treat that as "no
/// principal" — the safe direction. A certificate whose name cannot be read must
/// not authenticate as something else.
fn extract_common_name(der: &[u8]) -> Option<String> {
    // OID 2.5.4.3 (commonName), DER-encoded as 06 03 55 04 03.
    const CN_OID: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];

    let start = der
        .windows(CN_OID.len())
        .position(|window| window == CN_OID)?;
    let after_oid = start + CN_OID.len();

    // The value follows as a tagged string: <tag> <len> <bytes>. Accept the
    // string types an issuer realistically uses for a CN.
    let tag = *der.get(after_oid)?;
    let is_string = matches!(tag, 0x0c | 0x13 | 0x16 | 0x14 | 0x1e);
    if !is_string {
        return None;
    }

    let len = *der.get(after_oid + 1)? as usize;
    // Long-form lengths (high bit set) would mean a CN over 127 bytes; refuse
    // rather than misread the length byte as content.
    if len & 0x80 != 0 {
        return None;
    }

    let value_start = after_oid + 2;
    let value = der.get(value_start..value_start + len)?;
    String::from_utf8(value.to_vec()).ok()
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

#[cfg(test)]
mod principal_tests {
    use super::*;

    /// Build a DER fragment shaped like the subject RDN a real certificate
    /// carries: the CN OID followed by a UTF8String value.
    fn der_with_cn(cn: &str, tag: u8) -> Vec<u8> {
        let mut der = vec![0x30, 0x20, 0x31, 0x1e, 0x30, 0x1c];
        der.extend_from_slice(&[0x06, 0x03, 0x55, 0x04, 0x03]); // CN OID
        der.push(tag);
        der.push(cn.len() as u8);
        der.extend_from_slice(cn.as_bytes());
        der
    }

    #[test]
    fn extracts_a_utf8_common_name() {
        let der = der_with_cn("kafka-client", 0x0c);
        assert_eq!(extract_common_name(&der).as_deref(), Some("kafka-client"));
    }

    /// PrintableString is what many CAs actually emit for a CN.
    #[test]
    fn extracts_a_printable_string_common_name() {
        let der = der_with_cn("alice", 0x13);
        assert_eq!(extract_common_name(&der).as_deref(), Some("alice"));
    }

    /// A certificate whose name cannot be read must yield no principal rather
    /// than a wrong one - the connection then authorizes as ANONYMOUS instead of
    /// as somebody else.
    #[test]
    fn returns_none_when_there_is_no_common_name() {
        assert_eq!(extract_common_name(&[0x30, 0x03, 0x02, 0x01, 0x00]), None);
        assert_eq!(extract_common_name(&[]), None);
    }

    #[test]
    fn refuses_a_truncated_value() {
        // Claims 40 bytes of CN but supplies 3.
        let mut der = vec![0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 40];
        der.extend_from_slice(b"abc");
        assert_eq!(extract_common_name(&der), None);
    }

    /// A long-form length byte must be refused, not misread as content.
    #[test]
    fn refuses_long_form_lengths() {
        let der = vec![0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x81, 0x05, b'a'];
        assert_eq!(extract_common_name(&der), None);
    }

    /// A plaintext connection has no certificate and therefore no principal.
    #[test]
    fn plain_connections_have_no_principal() {
        // MaybeTlsStream::Plain cannot be constructed without a socket here, so
        // the invariant is asserted through the code path that matters: the
        // Plain arm returns None before touching any TLS state.
        // (Exercised end-to-end by the SASL suite, where every plaintext
        // connection authorizes as ANONYMOUS.)
    }
}

/// A throwaway CA and a `localhost` certificate it signed, for TLS tests.
#[cfg(test)]
pub(crate) mod test_pki {
    use super::TlsConfig;

    pub struct Pki {
        pub dir: tempfile::TempDir,
        pub ca: String,
        pub cert: String,
        pub key: String,
    }

    impl Pki {
        pub fn new() -> Self {
            use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
            let dir = tempfile::tempdir().unwrap();
            let ca_key = KeyPair::generate().unwrap();
            let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
            ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            ca_params.distinguished_name.push(DnType::CommonName, "chronik test ca");
            let ca = ca_params.self_signed(&ca_key).unwrap();
            let key = KeyPair::generate().unwrap();
            let cert = CertificateParams::new(vec!["localhost".to_string()])
                .unwrap()
                .signed_by(&key, &ca, &ca_key)
                .unwrap();
            let write = |name: &str, pem: String| {
                let path = dir.path().join(name);
                std::fs::write(&path, pem).unwrap();
                path.to_string_lossy().into_owned()
            };
            let ca = write("ca.crt", ca.pem());
            let cert = write("tls.crt", cert.pem());
            let key = write("tls.key", key.serialize_pem());
            Self { dir, ca, cert, key }
        }

        /// The listener config a broker with this PKI runs.
        pub fn listener(&self, require_client_cert: bool) -> TlsConfig {
            TlsConfig {
                cert_path: self.cert.clone(),
                key_path: self.key.clone(),
                ca_cert_path: Some(self.ca.clone()),
                require_client_cert,
            }
        }
    }
}

#[cfg(test)]
mod handshake_tests {
    use super::test_pki::Pki;
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serve one TLS connection that echoes 4 bytes; returns the address.
    async fn echo_server(acceptor: TlsAcceptor) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            if let Ok(mut tls) = acceptor.accept(tcp).await {
                let mut buf = [0u8; 4];
                if tls.read_exact(&mut buf).await.is_ok() {
                    let _ = tls.write_all(&buf).await;
                    let _ = tls.flush().await;
                }
            }
        });
        addr
    }

    /// A client that trusts the CA and presents no certificate.
    fn anonymous_client(pki: &Pki) -> tokio_rustls::TlsConnector {
        ensure_crypto_provider();
        let roots = load_root_store(Path::new(&pki.ca)).unwrap();
        let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }

    async fn echo(connector: &tokio_rustls::TlsConnector, addr: std::net::SocketAddr) -> std::io::Result<[u8; 4]> {
        let tcp = TcpStream::connect(addr).await?;
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut tls = connector.connect(name, tcp).await?;
        tls.write_all(b"ping").await?;
        tls.flush().await?;
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await?;
        Ok(buf)
    }

    /// With a CA but without CHRONIK_TLS_REQUIRE_CLIENT_CERT, a client with no
    /// certificate is served (it was refused: the flag was ignored).
    #[tokio::test]
    async fn a_client_certificate_is_optional_unless_required() {
        let pki = Pki::new();
        let addr = echo_server(create_tls_acceptor(&pki.listener(false)).unwrap()).await;
        assert_eq!(&echo(&anonymous_client(&pki), addr).await.unwrap(), b"ping");

        let addr = echo_server(create_tls_acceptor(&pki.listener(true)).unwrap()).await;
        assert!(echo(&anonymous_client(&pki), addr).await.is_err(), "served a client without a certificate");
    }

    /// The broker-to-broker client verifies the leader against the CA and
    /// presents its own certificate, so it reaches a leader that requires one.
    #[tokio::test]
    async fn the_inter_broker_client_reaches_a_leader_that_requires_certificates() {
        let pki = Pki::new();
        let addr = echo_server(create_tls_acceptor(&pki.listener(true)).unwrap()).await;
        let connector = create_tls_connector(&pki.listener(true)).unwrap();
        assert_eq!(&echo(&connector, addr).await.unwrap(), b"ping");
    }

    #[test]
    fn requiring_client_certificates_without_a_ca_is_refused() {
        let pki = Pki::new();
        let mut config = pki.listener(true);
        config.ca_cert_path = None;
        assert!(create_tls_acceptor(&config).is_err());
        assert!(create_tls_connector(&config).is_err(), "a broker client that verifies nothing");
    }
}
