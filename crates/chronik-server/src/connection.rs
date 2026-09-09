//! Per-connection state: identity, transport security, and authentication.
//!
//! # Why this exists
//!
//! Previously the Kafka request dispatch was `handle_request(&self, bytes)` —
//! it had no idea which connection a request arrived on. SASL therefore could not
//! be enforced: `SaslAuthenticate` created a throwaway authenticator, answered
//! "authenticated", and discarded the result. A client could skip SASL entirely
//! and issue `Produce`. There was likewise no principal for an authorizer to act
//! on, which is why `acl.rs` had no call sites.
//!
//! [`ConnectionContext`] is the missing state. One is created per accepted TCP
//! connection and threaded into the dispatch, which consults it *before* routing
//! (see [`ConnectionContext::check_request_allowed`]).
//!
//! # Concurrency
//!
//! Requests on a single connection are handled concurrently (each is spawned as
//! its own task by the accept loop), so the mutable half lives behind a mutex.
//! The SASL exchange is inherently sequential — a client sends `SaslHandshake`,
//! waits for the response, then sends `SaslAuthenticate` — so contention here is
//! nil in practice, and correctness does not depend on client ordering.
//!
//! # Scope
//!
//! This is Phase 0 of docs/ROADMAP_SECURITY.md: authentication is enforced, and a
//! principal is recorded. Authorization (ACLs) consumes [`ConnectionContext::principal`]
//! in Phase 3; nothing checks permissions yet.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use chronik_protocol::sasl::{
    SaslAuthenticateResponse, SaslAuthenticator, SaslError, SaslHandshakeResponse,
};

/// Kafka error code: request received in a state that SASL does not permit.
pub const ERROR_ILLEGAL_SASL_STATE: i16 = 34;
/// Kafka error code: SASL authentication failed.
pub const ERROR_SASL_AUTHENTICATION_FAILED: i16 = 58;

/// API keys a client may send before it has authenticated.
///
/// `ApiVersions` is required for negotiation (clients send it first and cannot
/// know the broker's SASL configuration until they do), and the two SASL APIs are
/// how authentication happens. Everything else is refused.
const API_KEY_SASL_HANDSHAKE: i16 = 17;
const API_KEY_API_VERSIONS: i16 = 18;
const API_KEY_SASL_AUTHENTICATE: i16 = 36;

/// Monotonic per-process connection identifier, for correlating log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(u64);

impl ConnectionId {
    fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ConnectionId(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn-{}", self.0)
    }
}

/// How strictly SASL is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaslMode {
    /// No authentication. The default.
    Disabled,
    /// Authentication is offered and verified, but an unauthenticated client is
    /// still served — and logged.
    ///
    /// This exists because there is no listener model yet (Phase 2 of
    /// docs/ROADMAP_SECURITY.md): with a single port, going straight from
    /// `Disabled` to `Required` cuts off every client that has not been
    /// reconfigured, simultaneously. `Optional` is the staged rollout — turn it
    /// on, watch the logs until no client is reported unauthenticated, then set
    /// `Required`. It grants no access that `Disabled` did not already grant, so
    /// it is never a downgrade; it is only ever a step on the way up.
    Optional,
    /// Authentication is required before any other API.
    Required,
}

/// Server-wide SASL settings, resolved once at startup and shared by every
/// connection.
#[derive(Debug, Clone)]
pub struct SaslConfig {
    /// How strictly authentication is applied.
    mode: SaslMode,
    /// `username -> password`, from `CHRONIK_SASL_USERS`.
    users: Vec<(String, String)>,
}

impl SaslConfig {
    /// Read SASL configuration from the environment.
    ///
    /// `CHRONIK_SASL_ENABLED=true` turns on enforcement. It defaults to **off**:
    /// enabling authentication on an existing cluster locks out every client at
    /// once, and there is no listener model yet that would allow a staged
    /// rollout (Phase 2). Operators opt in deliberately.
    pub fn from_env() -> Self {
        // `CHRONIK_SASL_ENABLED` accepts true/1/yes (require) and, since the
        // staged-rollout mode was added, `optional`. It stays the single knob so
        // existing configurations keep their meaning.
        let mode = match std::env::var("CHRONIK_SASL_ENABLED") {
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "required" => SaslMode::Required,
                "optional" | "warn" => SaslMode::Optional,
                _ => SaslMode::Disabled,
            },
            Err(_) => SaslMode::Disabled,
        };

        let users = std::env::var("CHRONIK_SASL_USERS")
            .ok()
            .map(|config| parse_users(&config))
            .unwrap_or_default();

        match mode {
            SaslMode::Required if users.is_empty() => {
                warn!(
                    "CHRONIK_SASL_ENABLED requires authentication but CHRONIK_SASL_USERS \
                     provides no valid users - every client will be rejected. \
                     Set CHRONIK_SASL_USERS='user:pass,...'."
                );
            }
            SaslMode::Required => {
                info!(
                    "SASL authentication REQUIRED ({} user(s) configured). Unauthenticated \
                     clients may only send ApiVersions, SaslHandshake and SaslAuthenticate.",
                    users.len()
                );
            }
            SaslMode::Optional => {
                warn!(
                    "SASL authentication is OPTIONAL ({} user(s) configured): unauthenticated \
                     clients are still SERVED and logged. This is a migration setting - watch \
                     for 'unauthenticated request' warnings, and set CHRONIK_SASL_ENABLED=true \
                     once they stop.",
                    users.len()
                );
            }
            SaslMode::Disabled => {
                debug!("SASL authentication disabled (set CHRONIK_SASL_ENABLED=true to require it)");
            }
        }

        Self { mode, users }
    }

    /// A configuration with authentication disabled — the default, and what
    /// tests and internal callers use when they are not exercising auth.
    pub fn disabled() -> Self {
        Self {
            mode: SaslMode::Disabled,
            users: Vec::new(),
        }
    }

    /// Build a configuration requiring authentication (for tests).
    pub fn enabled_with_users(users: Vec<(String, String)>) -> Self {
        Self {
            mode: SaslMode::Required,
            users,
        }
    }

    /// Build a configuration offering but not requiring authentication.
    pub fn optional_with_users(users: Vec<(String, String)>) -> Self {
        Self {
            mode: SaslMode::Optional,
            users,
        }
    }

    pub fn mode(&self) -> SaslMode {
        self.mode
    }

    /// Whether authentication is offered at all (required or optional).
    pub fn is_enabled(&self) -> bool {
        self.mode != SaslMode::Disabled
    }

    /// Whether an unauthenticated connection is refused.
    pub fn is_required(&self) -> bool {
        self.mode == SaslMode::Required
    }

    /// Create a fresh authenticator carrying this configuration's users.
    fn new_authenticator(&self) -> SaslAuthenticator {
        let mut authenticator = SaslAuthenticator::new_empty();
        for (user, pass) in &self.users {
            authenticator.add_user(user.clone(), pass.clone());
        }
        authenticator
    }
}

fn parse_users(config: &str) -> Vec<(String, String)> {
    config
        .split(',')
        .filter_map(|pair| {
            let mut parts = pair.trim().splitn(2, ':');
            match (parts.next(), parts.next()) {
                (Some(user), Some(pass)) if !user.is_empty() => {
                    Some((user.to_string(), pass.to_string()))
                }
                _ => None,
            }
        })
        .collect()
}

/// The authenticated identity of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// Authentication is not required on this server.
    Disabled,
    /// Authentication is required and has not completed.
    Unauthenticated,
    /// Authentication completed.
    Authenticated {
        principal: String,
        mechanism: String,
        at: DateTime<Utc>,
    },
}

impl AuthState {
    pub fn is_authenticated(&self) -> bool {
        matches!(self, AuthState::Disabled | AuthState::Authenticated { .. })
    }

    /// The principal to authorize as, in Kafka's `User:name` form.
    ///
    /// `None` on an unauthenticated connection, and on a server with
    /// authentication disabled (there is no identity to speak of).
    pub fn principal(&self) -> Option<String> {
        match self {
            AuthState::Authenticated { principal, .. } => Some(format!("User:{}", principal)),
            _ => None,
        }
    }
}

/// Why a request was refused before it reached its handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRejection {
    pub error_code: i16,
    pub message: String,
}

struct ConnectionInner {
    authenticator: SaslAuthenticator,
    auth: AuthState,
    /// Whether this connection has already been reported as unauthenticated,
    /// so migration-mode logging is once per connection, not per request.
    warned_unauthenticated: bool,
}

/// Per-connection identity and authentication state.
pub struct ConnectionContext {
    id: ConnectionId,
    peer_addr: SocketAddr,
    /// Whether the connection arrived over TLS.
    tls: bool,
    sasl: Arc<SaslConfig>,
    inner: Mutex<ConnectionInner>,
}

impl ConnectionContext {
    /// Create the context for a newly accepted connection.
    pub fn new(peer_addr: SocketAddr, tls: bool, sasl: Arc<SaslConfig>) -> Self {
        let auth = if sasl.is_enabled() {
            AuthState::Unauthenticated
        } else {
            AuthState::Disabled
        };
        let authenticator = sasl.new_authenticator();

        Self {
            id: ConnectionId::next(),
            peer_addr,
            tls,
            sasl,
            inner: Mutex::new(ConnectionInner {
                authenticator,
                auth,
                warned_unauthenticated: false,
            }),
        }
    }

    /// A context for internal callers and tests that do not authenticate
    /// (in-process request handling, unit tests).
    pub fn internal() -> Self {
        Self::new(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            false,
            Arc::new(SaslConfig::disabled()),
        )
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    pub fn is_tls(&self) -> bool {
        self.tls
    }

    pub async fn auth_state(&self) -> AuthState {
        self.inner.lock().await.auth.clone()
    }

    pub async fn is_authenticated(&self) -> bool {
        self.inner.lock().await.auth.is_authenticated()
    }

    /// The principal for this connection, once Phase 3 needs it for ACL checks.
    pub async fn principal(&self) -> Option<String> {
        self.inner.lock().await.auth.principal()
    }

    /// The pre-authentication gate.
    ///
    /// Called by the dispatch for every request before routing. When SASL is
    /// enabled and the connection has not authenticated, only `ApiVersions`,
    /// `SaslHandshake` and `SaslAuthenticate` are permitted; anything else is
    /// refused with `ILLEGAL_SASL_STATE`.
    ///
    /// This is the check whose absence made SASL advisory: the credentials were
    /// verified, and then nothing consulted the answer.
    pub async fn check_request_allowed(&self, api_key: i16) -> Result<(), AuthRejection> {
        if !self.sasl.is_enabled() {
            return Ok(());
        }

        if self.inner.lock().await.auth.is_authenticated() {
            return Ok(());
        }

        match api_key {
            API_KEY_API_VERSIONS | API_KEY_SASL_HANDSHAKE | API_KEY_SASL_AUTHENTICATE => Ok(()),
            other if !self.sasl.is_required() => {
                // Migration mode: serve, but make the client visible so an
                // operator can tell when it is safe to switch to Required.
                // Logged once per connection rather than per request, or a
                // single busy producer would flood the log.
                let mut inner = self.inner.lock().await;
                if !inner.warned_unauthenticated {
                    inner.warned_unauthenticated = true;
                    warn!(
                        "{} ({}) is issuing unauthenticated requests (first: API key {}). \
                         SASL is OPTIONAL, so it is being served - it would be REFUSED with \
                         CHRONIK_SASL_ENABLED=true.",
                        self.id, self.peer_addr, other
                    );
                }
                Ok(())
            }
            other => {
                warn!(
                    "{} ({}) sent API key {} before authenticating - refusing",
                    self.id, self.peer_addr, other
                );
                Err(AuthRejection {
                    error_code: ERROR_ILLEGAL_SASL_STATE,
                    message: format!(
                        "Request (API key {}) is not permitted before SASL authentication completes",
                        other
                    ),
                })
            }
        }
    }

    /// Run a `SaslHandshake` against this connection's authenticator.
    pub async fn sasl_handshake(
        &self,
        version: i16,
        mechanisms: &[String],
    ) -> Result<SaslHandshakeResponse, SaslError> {
        let mut inner = self.inner.lock().await;
        inner.authenticator.handle_handshake(version, mechanisms)
    }

    /// The mechanisms this server will accept, for reporting in a failed
    /// handshake response.
    pub async fn enabled_mechanisms(&self) -> Vec<String> {
        let inner = self.inner.lock().await;
        inner
            .authenticator
            .supported_mechanisms()
            .iter()
            .map(|m| m.as_str().to_string())
            .collect()
    }

    /// Run a `SaslAuthenticate` against this connection's authenticator and, on
    /// success, record the principal on the connection.
    pub async fn sasl_authenticate(
        &self,
        auth_bytes: &[u8],
    ) -> Result<SaslAuthenticateResponse, SaslError> {
        let mut inner = self.inner.lock().await;
        let response = inner.authenticator.handle_authenticate(auth_bytes)?;

        // `username()` is Some only once the exchange has fully completed. That
        // matters for SCRAM, which takes two round trips: the intermediate step
        // returns Ok with a server-first message and must NOT mark the
        // connection authenticated.
        if let Some(username) = inner.authenticator.username() {
            let principal = username.to_string();
            let mechanism = inner
                .authenticator
                .negotiated_mechanism()
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| "UNKNOWN".to_string());
            inner.auth = AuthState::Authenticated {
                principal: principal.clone(),
                mechanism: mechanism.clone(),
                at: Utc::now(),
            };
            info!(
                "{} ({}) authenticated as User:{} via {}",
                self.id, self.peer_addr, principal, mechanism
            );
        }

        Ok(response)
    }
}

impl std::fmt::Debug for ConnectionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionContext")
            .field("id", &self.id)
            .field("peer_addr", &self.peer_addr)
            .field("tls", &self.tls)
            .field("sasl_enabled", &self.sasl.is_enabled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const API_PRODUCE: i16 = 0;
    const API_FETCH: i16 = 1;
    const API_METADATA: i16 = 3;

    fn enabled_ctx() -> ConnectionContext {
        ConnectionContext::new(
            SocketAddr::from(([127, 0, 0, 1], 9092)),
            false,
            Arc::new(SaslConfig::enabled_with_users(vec![(
                "alice".to_string(),
                "secret".to_string(),
            )])),
        )
    }

    /// The assertion that matters: an unauthenticated connection cannot produce.
    ///
    /// The previous `sasl_test_standalone.rs` asserted this against a mock
    /// registry defined inside the test file; the server had no such component.
    /// This exercises the real one.
    #[tokio::test]
    async fn unauthenticated_connection_cannot_produce_or_fetch() {
        let ctx = enabled_ctx();

        assert!(!ctx.is_authenticated().await);
        for api in [API_PRODUCE, API_FETCH, API_METADATA] {
            let rejection = ctx
                .check_request_allowed(api)
                .await
                .expect_err("API must be refused before authentication");
            assert_eq!(rejection.error_code, ERROR_ILLEGAL_SASL_STATE);
        }
    }

    #[tokio::test]
    async fn handshake_and_authenticate_are_allowed_before_auth() {
        let ctx = enabled_ctx();
        for api in [
            API_KEY_API_VERSIONS,
            API_KEY_SASL_HANDSHAKE,
            API_KEY_SASL_AUTHENTICATE,
        ] {
            assert!(ctx.check_request_allowed(api).await.is_ok());
        }
    }

    #[tokio::test]
    async fn valid_credentials_unlock_the_connection() {
        let ctx = enabled_ctx();

        ctx.sasl_handshake(1, &["PLAIN".to_string()]).await.unwrap();
        let response = ctx
            .sasl_authenticate(b"\0alice\0secret")
            .await
            .expect("valid credentials must authenticate");
        assert_eq!(response.error_code, 0);

        assert!(ctx.is_authenticated().await);
        assert_eq!(ctx.principal().await, Some("User:alice".to_string()));
        assert!(ctx.check_request_allowed(API_PRODUCE).await.is_ok());
    }

    #[tokio::test]
    async fn wrong_password_leaves_connection_locked() {
        let ctx = enabled_ctx();

        ctx.sasl_handshake(1, &["PLAIN".to_string()]).await.unwrap();
        assert!(ctx.sasl_authenticate(b"\0alice\0wrong").await.is_err());

        assert!(!ctx.is_authenticated().await);
        assert_eq!(ctx.principal().await, None);
        assert!(ctx.check_request_allowed(API_PRODUCE).await.is_err());
    }

    #[tokio::test]
    async fn unknown_user_leaves_connection_locked() {
        let ctx = enabled_ctx();

        ctx.sasl_handshake(1, &["PLAIN".to_string()]).await.unwrap();
        assert!(ctx.sasl_authenticate(b"\0mallory\0secret").await.is_err());

        assert!(!ctx.is_authenticated().await);
        assert!(ctx.check_request_allowed(API_PRODUCE).await.is_err());
    }

    /// SCRAM is advertised nowhere and accepted nowhere.
    #[tokio::test]
    async fn scram_is_offered_and_the_intermediate_step_does_not_authenticate() {
        let ctx = enabled_ctx();

        let mechanisms = ctx.enabled_mechanisms().await;
        assert!(mechanisms.contains(&"SCRAM-SHA-256".to_string()));
        assert!(mechanisms.contains(&"SCRAM-SHA-512".to_string()));

        assert!(ctx
            .sasl_handshake(1, &["SCRAM-SHA-256".to_string()])
            .await
            .is_ok());

        // SCRAM takes two round trips. The first returns a server-first message
        // and MUST leave the connection unauthenticated — otherwise a client
        // could send only client-first and then produce.
        let server_first = ctx
            .sasl_authenticate(b"n,,n=alice,r=clientnonce")
            .await
            .expect("server-first");
        assert_eq!(server_first.error_code, 0);
        assert!(server_first.auth_bytes.is_some());

        assert!(
            !ctx.is_authenticated().await,
            "a half-finished SCRAM exchange must not authenticate the connection"
        );
        assert!(ctx.check_request_allowed(API_PRODUCE).await.is_err());
    }

    /// With SASL disabled the gate must be transparent, so existing deployments
    /// are unaffected by Phase 0.
    #[tokio::test]
    async fn disabled_sasl_allows_everything() {
        let ctx = ConnectionContext::internal();

        assert!(ctx.is_authenticated().await);
        assert_eq!(ctx.auth_state().await, AuthState::Disabled);
        assert_eq!(ctx.principal().await, None);
        for api in [API_PRODUCE, API_FETCH, API_METADATA, API_KEY_API_VERSIONS] {
            assert!(ctx.check_request_allowed(api).await.is_ok());
        }
    }

    #[tokio::test]
    async fn enabled_sasl_without_users_rejects_everyone() {
        let ctx = ConnectionContext::new(
            SocketAddr::from(([127, 0, 0, 1], 9092)),
            false,
            Arc::new(SaslConfig::enabled_with_users(vec![])),
        );

        ctx.sasl_handshake(1, &["PLAIN".to_string()]).await.unwrap();
        assert!(ctx.sasl_authenticate(b"\0admin\0admin123").await.is_err());
        assert!(!ctx.is_authenticated().await);
    }

    #[test]
    fn user_parsing_skips_malformed_entries() {
        let users = parse_users("alice:secret, bob:pw:with:colons ,,noseparator,:emptyuser");
        assert_eq!(
            users,
            vec![
                ("alice".to_string(), "secret".to_string()),
                ("bob".to_string(), "pw:with:colons".to_string()),
            ]
        );
    }

    #[test]
    fn connection_ids_are_unique() {
        let a = ConnectionId::next();
        let b = ConnectionId::next();
        assert_ne!(a, b);
    }
}

#[cfg(test)]
mod migration_mode_tests {
    use super::*;

    const API_PRODUCE: i16 = 0;

    fn optional_ctx() -> ConnectionContext {
        ConnectionContext::new(
            SocketAddr::from(([127, 0, 0, 1], 9092)),
            false,
            Arc::new(SaslConfig::optional_with_users(vec![(
                "alice".to_string(),
                "secret".to_string(),
            )])),
        )
    }

    /// The point of Optional: an unauthenticated client is still served, so a
    /// cluster can turn authentication on without cutting every client off in
    /// the same instant.
    #[tokio::test]
    async fn optional_mode_serves_unauthenticated_clients() {
        let ctx = optional_ctx();
        assert!(!ctx.is_authenticated().await);
        assert!(ctx.check_request_allowed(API_PRODUCE).await.is_ok());
    }

    /// ...but it is still real authentication: valid credentials work and a
    /// wrong password is still rejected. Optional must not become "any password
    /// is fine", which would be a downgrade rather than a migration step.
    #[tokio::test]
    async fn optional_mode_still_verifies_credentials() {
        let ctx = optional_ctx();
        ctx.sasl_handshake(1, &["PLAIN".to_string()]).await.unwrap();
        assert!(ctx.sasl_authenticate(b"\0alice\0wrong").await.is_err());
        assert!(!ctx.is_authenticated().await);

        let ctx = optional_ctx();
        ctx.sasl_handshake(1, &["PLAIN".to_string()]).await.unwrap();
        assert!(ctx.sasl_authenticate(b"\0alice\0secret").await.is_ok());
        assert!(ctx.is_authenticated().await);
        assert_eq!(ctx.principal().await, Some("User:alice".to_string()));
    }

    /// Required still refuses, so Optional is a distinct state and not a
    /// silent weakening of the enforced one.
    #[tokio::test]
    async fn required_mode_still_refuses() {
        let ctx = ConnectionContext::new(
            SocketAddr::from(([127, 0, 0, 1], 9092)),
            false,
            Arc::new(SaslConfig::enabled_with_users(vec![(
                "alice".to_string(),
                "secret".to_string(),
            )])),
        );
        assert!(ctx.check_request_allowed(API_PRODUCE).await.is_err());
    }

    #[test]
    fn mode_parsing_covers_the_documented_spellings() {
        for (value, expected) in [
            ("true", SaslMode::Required),
            ("1", SaslMode::Required),
            ("yes", SaslMode::Required),
            ("required", SaslMode::Required),
            ("optional", SaslMode::Optional),
            ("warn", SaslMode::Optional),
            ("false", SaslMode::Disabled),
            ("nonsense", SaslMode::Disabled),
        ] {
            std::env::set_var("CHRONIK_SASL_ENABLED", value);
            let config = SaslConfig::from_env();
            assert_eq!(config.mode(), expected, "for CHRONIK_SASL_ENABLED={}", value);
        }
        std::env::remove_var("CHRONIK_SASL_ENABLED");
    }
}
