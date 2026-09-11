//! Authentication for the Unified API's data plane (Security Phase 4).
//!
//! # The hole this closes
//!
//! `/_sql`, `/_search`, `/_vector` and `/_query` read topic data, and until now
//! they required nothing at all: any process that could reach port 6092 could
//! `SELECT * FROM any_topic` over plain HTTP. That bypasses every control on the
//! Kafka port — SASL authentication (Phase 0/1) and ACLs (Phase 3) are enforced
//! at :9092 and were simply absent here, so the HTTP surface was a complete way
//! around them.
//!
//! Only `/memory/v1/*` had any check (tenant + API key), and `/admin/*` has its
//! own. This covers the query endpoints.
//!
//! # Model
//!
//! A shared API key in `X-API-Key`, matching the pattern already used by the
//! admin API rather than inventing a second scheme. Disabled by default, because
//! turning it on without warning would break every existing dashboard and
//! ingestion job in one step; operators opt in with `CHRONIK_API_KEY`.
//!
//! # What this is not
//!
//! It authenticates the *caller*, it does not authorize per topic. A holder of
//! the key can query any topic. Per-topic authorization here needs a principal
//! and, for `/_sql`, the set of tables a statement touches — which means
//! resolving the query plan, not string-matching SQL. That is tracked as the
//! remainder of Phase 4 in docs/ROADMAP_SECURITY.md. Authenticating the surface
//! is the part that stops anonymous exfiltration, and it is worth having before
//! the finer-grained half exists.

use axum::{
    body::BoxBody,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use subtle::ConstantTimeEq;
use tracing::{info, warn};

/// Header carrying the API key, same as the admin API.
pub const API_KEY_HEADER: &str = "X-API-Key";

/// Configuration for data-plane authentication.
#[derive(Debug, Clone, Default)]
pub struct DataAuthConfig {
    /// The required key. `None` disables authentication.
    key: Option<String>,
}

impl DataAuthConfig {
    /// Read from the environment.
    ///
    /// `CHRONIK_API_KEY` protects the query endpoints. If it is unset the
    /// endpoints stay open and the broker says so loudly at startup — silence
    /// would let an operator believe Phase 4 protects them when it does not.
    pub fn from_env() -> Self {
        match std::env::var("CHRONIK_API_KEY") {
            Ok(key) if !key.trim().is_empty() => {
                info!(
                    "Unified API data endpoints require {} (/_sql, /_search, /_vector, /_query)",
                    API_KEY_HEADER
                );
                Self {
                    key: Some(key.trim().to_string()),
                }
            }
            _ => {
                warn!(
                    "CHRONIK_API_KEY is not set - /_sql, /_search and /_vector are reachable \
                     WITHOUT authentication. Anyone who can reach this port can read every \
                     topic, bypassing Kafka-port SASL and ACLs entirely."
                );
                Self { key: None }
            }
        }
    }

    /// Build with an explicit key (tests, embedding).
    pub fn with_key(key: Option<String>) -> Self {
        Self {
            key: key.filter(|k| !k.trim().is_empty()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    /// Whether a presented key is correct.
    fn accepts(&self, presented: &str) -> bool {
        match &self.key {
            // Constant-time: a byte-by-byte comparison leaks the key's prefix
            // through timing to anyone who can call this endpoint in a loop.
            Some(expected) => {
                let expected = expected.as_bytes();
                let presented = presented.as_bytes();
                expected.len() == presented.len()
                    && expected.ct_eq(presented).unwrap_u8() == 1
            }
            None => true,
        }
    }
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "code": "unauthorized",
                "message": message,
            }
        })),
    )
        .into_response()
}

/// Axum middleware enforcing [`DataAuthConfig`] on the routes it wraps.
pub async fn require_api_key<B>(
    config: DataAuthConfig,
    request: Request<B>,
    next: Next<B>,
) -> Response<BoxBody> {
    if !config.is_enabled() {
        return next.run(request).await;
    }

    let presented = request
        .headers()
        .get(API_KEY_HEADER)
        .and_then(|v| v.to_str().ok());

    match presented {
        Some(key) if config.accepts(key) => next.run(request).await,
        Some(_) => {
            warn!(
                "Rejected {} {} - invalid {}",
                request.method(),
                request.uri().path(),
                API_KEY_HEADER
            );
            unauthorized("invalid API key")
        }
        None => unauthorized(&format!("{} required", API_KEY_HEADER)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_config_accepts_anything() {
        let config = DataAuthConfig::with_key(None);
        assert!(!config.is_enabled());
        assert!(config.accepts("whatever"));
    }

    #[test]
    fn an_empty_key_does_not_enable_auth() {
        // Otherwise `CHRONIK_API_KEY=""` would look like it protects the API
        // while accepting the empty string as the password.
        assert!(!DataAuthConfig::with_key(Some("".to_string())).is_enabled());
        assert!(!DataAuthConfig::with_key(Some("   ".to_string())).is_enabled());
    }

    #[test]
    fn correct_key_is_accepted_and_others_are_not() {
        let config = DataAuthConfig::with_key(Some("s3cret".to_string()));
        assert!(config.is_enabled());
        assert!(config.accepts("s3cret"));
        assert!(!config.accepts("wrong"));
        // A prefix must not pass, which a length-first comparison could allow if
        // it were written carelessly.
        assert!(!config.accepts("s3cre"));
        assert!(!config.accepts("s3cretx"));
        assert!(!config.accepts(""));
    }
}
