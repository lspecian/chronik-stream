//! Broker-to-broker authentication (`__chronik_broker`).
//!
//! With SASL required, every connection must authenticate before it may do
//! anything but negotiate, and that includes the replica fetcher. It had no
//! credentials, so on a SASL cluster every follower fetch was cut off: the
//! followers held none of their leaders' partitions while the cluster looked
//! healthy (the in-sync set still listed them, and `acks=all` writes were still
//! acknowledged through the duplicate-retry path).
//!
//! Brokers authenticate as the reserved user
//! [`INTER_BROKER_USER`](chronik_protocol::sasl::INTER_BROKER_USER) with
//! SCRAM-SHA-512, using a secret shared by every broker in the cluster. The
//! name is fixed; the secret is per deployment and never in the source.
//!
//! The principal bypasses ACLs, as Kafka's inter-broker principal effectively
//! does: replication must read every partition, and a missing rule would stop
//! it silently.

use std::sync::OnceLock;

use chronik_protocol::sasl::SaslUser;
use chronik_protocol::scram::{ScramCredential, DEFAULT_ITERATIONS};

use crate::connection::SaslMode;

/// Environment variable holding the shared secret.
pub const SECRET_ENV: &str = "CHRONIK_INTER_BROKER_SECRET";

/// Shortest secret accepted. The secret guards a principal that bypasses every
/// ACL, and anyone who can reach the Kafka port can try to guess it.
pub const MIN_SECRET_LEN: usize = 16;

/// The `User:` principal brokers authenticate as.
pub fn principal() -> String {
    format!("User:{}", chronik_protocol::sasl::INTER_BROKER_USER)
}

fn parse_secret(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The configured secret, read once.
pub fn secret() -> Option<&'static str> {
    static SECRET: OnceLock<Option<String>> = OnceLock::new();
    SECRET
        .get_or_init(|| parse_secret(std::env::var(SECRET_ENV).ok()))
        .as_deref()
}

/// TLS for replica fetches, set once at cluster startup by [`configure_tls`].
static TLS: OnceLock<Option<tokio_rustls::TlsConnector>> = OnceLock::new();

/// Make replica fetches speak TLS when this node's Kafka port does: every
/// broker of a cluster serves the same listener, so a follower must dial its
/// leaders the way clients dial it. Before, followers always dialled in
/// plaintext; against a TLS port every fetch failed, every ISR shrank to its
/// leader, and `acks=all` writes timed out while clients saw a running cluster.
///
/// Refuses (the node does not start) when the port uses TLS but other brokers
/// cannot be verified: no `CHRONIK_TLS_CA_CERT`, or unreadable files.
pub fn configure_tls(listener: Option<&crate::tls::TlsConfig>) -> Result<(), String> {
    let connector = match listener {
        None => None,
        Some(config) => Some(crate::tls::create_tls_connector(config).map_err(|e| {
            format!(
                "the Kafka port uses TLS (CHRONIK_TLS_CERT), so followers fetch from their leaders \
                 over TLS, but that client cannot be built: {e:#}"
            )
        })?),
    };
    let _ = TLS.set(connector);

    // Query fan-out reaches peers' Unified API, which serves HTTPS when this
    // node's does (CHRONIK_API_TLS_*, or the Kafka listener's certificate).
    let api = match crate::tls::api_tls_config() {
        None => None,
        Some(config) => Some(crate::tls::client_config(&config).map_err(|e| {
            format!(
                "the Unified API serves HTTPS, so queries fan out to peers over HTTPS, but that \
                 client cannot be built: {e:#}"
            )
        })?),
    };
    let _ = API_TLS.set(api);
    Ok(())
}

/// TLS for query fan-out to peers' Unified API, set by [`configure_tls`].
static API_TLS: OnceLock<Option<rustls::ClientConfig>> = OnceLock::new();

/// The client config for HTTPS to peers' Unified API, if it serves HTTPS.
pub fn api_tls() -> Option<rustls::ClientConfig> {
    API_TLS.get().and_then(Option::clone)
}

/// The TLS client for replica fetches, if this node's Kafka port uses TLS.
pub fn tls() -> Option<&'static tokio_rustls::TlsConnector> {
    TLS.get().and_then(Option::as_ref)
}

/// The identity of a connection that presented a verified client certificate.
/// A peer whose certificate names the same principal as this broker's own
/// (`CHRONIK_TLS_CERT`) is a broker: the handshake proved it holds a key the CA
/// certified under that name, which is how brokers sharing one certificate (the
/// operator's `spec.tls`) recognise each other. It authenticates as the broker
/// principal, so it may replicate. Before, an mTLS cluster without SASL
/// refused every replica fetch (CLUSTER_AUTHORIZATION_FAILED) and replicated
/// nothing. The name, not the exact certificate, is compared, so brokers keep
/// replicating while they restart one by one onto a renewed certificate.
pub fn certificate_identity(certificate_principal: Option<String>) -> Option<String> {
    static OWN: OnceLock<Option<String>> = OnceLock::new();
    let own = OWN.get_or_init(|| {
        crate::tls::TlsConfig::from_env().and_then(|c| crate::tls::certificate_file_principal(&c.cert_path))
    });
    identity_for(own.as_deref(), certificate_principal)
}

fn identity_for(own: Option<&str>, certificate_principal: Option<String>) -> Option<String> {
    match certificate_principal {
        Some(p) if own == Some(p.as_str()) => Some(principal()),
        other => other,
    }
}

/// The broker user the server accepts, or `None` when no secret is set.
///
/// SCRAM-SHA-512 only. PLAIN would send the secret in the clear on every
/// reconnect when brokers do not use TLS.
pub fn server_user() -> Option<SaslUser> {
    let secret = secret()?;
    match ScramCredential::derive(
        secret,
        chronik_protocol::sasl::SaslMechanism::ScramSha512,
        DEFAULT_ITERATIONS,
    ) {
        Ok(credential) => Some(SaslUser::from_scram_credential(credential)),
        Err(e) => {
            tracing::error!("Could not derive the inter-broker credential: {}", e);
            None
        }
    }
}

/// Decide whether a cluster node may start with this SASL mode and secret.
///
/// Required without a secret is refused rather than warned about, because the
/// failure it causes is silent: the broker runs, serves clients, and replicates
/// nothing. Optional without a secret still works (unauthenticated fetches are
/// served) and only warns.
pub fn check_cluster_startup(mode: SaslMode, secret: Option<&str>) -> Result<(), String> {
    if let Some(secret) = secret {
        if secret.len() < MIN_SECRET_LEN {
            return Err(format!(
                "{SECRET_ENV} is {} characters; it must be at least {MIN_SECRET_LEN}. \
                 It authenticates brokers as a principal that bypasses ACLs.",
                secret.len()
            ));
        }
        return Ok(());
    }
    match mode {
        SaslMode::Required => Err(format!(
            "CHRONIK_SASL_ENABLED requires authentication, but {SECRET_ENV} is not set. \
             Followers could not authenticate to their leaders and would replicate nothing. \
             Set the same random secret (at least {MIN_SECRET_LEN} characters) on every broker."
        )),
        SaslMode::Optional => {
            tracing::warn!(
                "SASL is optional and {} is not set: replica fetches run unauthenticated. \
                 Set it before switching CHRONIK_SASL_ENABLED to true, or followers will stop \
                 replicating at that moment.",
                SECRET_ENV
            );
            Ok(())
        }
        SaslMode::Disabled => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer presenting a certificate under this broker's name is a broker;
    /// any other certificate keeps its own principal.
    #[test]
    fn a_certificate_under_the_broker_name_is_the_broker() {
        let own = Some("User:chronik-broker");
        assert_eq!(identity_for(own, Some("User:chronik-broker".into())), Some(principal()));
        assert_eq!(identity_for(own, Some("User:alice".into())).as_deref(), Some("User:alice"));
        assert_eq!(identity_for(own, None), None);
        assert_eq!(identity_for(None, Some("User:alice".into())).as_deref(), Some("User:alice"));
    }

    #[test]
    fn required_sasl_without_a_secret_refuses_to_start() {
        let err = check_cluster_startup(SaslMode::Required, None).unwrap_err();
        assert!(err.contains(SECRET_ENV), "{err}");
    }

    #[test]
    fn a_short_secret_is_refused_in_every_mode() {
        for mode in [SaslMode::Required, SaslMode::Optional, SaslMode::Disabled] {
            assert!(check_cluster_startup(mode, Some("short")).is_err());
        }
    }

    #[test]
    fn optional_or_disabled_without_a_secret_may_start() {
        assert!(check_cluster_startup(SaslMode::Optional, None).is_ok());
        assert!(check_cluster_startup(SaslMode::Disabled, None).is_ok());
    }

    #[test]
    fn required_with_a_good_secret_starts() {
        assert!(check_cluster_startup(SaslMode::Required, Some("0123456789abcdef")).is_ok());
    }

    #[test]
    fn blank_secret_counts_as_unset() {
        assert_eq!(parse_secret(Some("   ".into())), None);
        assert_eq!(parse_secret(Some(" abc ".into())), Some("abc".into()));
    }
}
