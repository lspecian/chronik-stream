//! Connection state management for Chronik Stream
//!
//! This module manages per-connection state including SASL authentication status,
//! user identity, and connection lifecycle.

use std::collections::HashMap;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};
use uuid::Uuid;
use chronik_protocol::sasl::SaslAuthenticator;
use chronik_protocol::client_compat::{ClientProfile, ClientRegistry};
use tracing::{debug, info, warn};

/// Unique identifier for each connection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Create a new unique connection ID
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ConnectionId(COUNTER.fetch_add(1, Ordering::SeqCst))
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn-{}", self.0)
    }
}

/// Authentication state for a connection
#[derive(Debug, Clone)]
pub enum AuthenticationState {
    /// Connection not yet authenticated
    Unauthenticated,
    /// SASL handshake in progress
    SaslHandshakeInProgress,
    /// SASL authentication in progress
    SaslAuthenticationInProgress,
    /// Successfully authenticated
    Authenticated {
        username: String,
        mechanism: String,
        authenticated_at: DateTime<Utc>,
    },
    /// Authentication failed
    AuthenticationFailed {
        reason: String,
        failed_at: DateTime<Utc>,
    },
}

impl AuthenticationState {
    /// Check if connection is authenticated
    pub fn is_authenticated(&self) -> bool {
        matches!(self, AuthenticationState::Authenticated { .. })
    }

    /// Get authenticated username if available
    pub fn username(&self) -> Option<&str> {
        match self {
            AuthenticationState::Authenticated { username, .. } => Some(username),
            _ => None,
        }
    }

    /// Get authentication mechanism if available
    pub fn mechanism(&self) -> Option<&str> {
        match self {
            AuthenticationState::Authenticated { mechanism, .. } => Some(mechanism),
            _ => None,
        }
    }
}

/// Per-connection state information
#[derive(Debug)]
pub struct ConnectionState {
    /// Unique connection identifier
    pub id: ConnectionId,
    /// Remote address of the connection
    pub remote_addr: String,
    /// When the connection was established
    pub connected_at: DateTime<Utc>,
    /// Current authentication state
    pub auth_state: AuthenticationState,
    /// SASL authenticator instance for this connection
    pub sasl_authenticator: SaslAuthenticator,
    /// Client profile for protocol compatibility
    pub client_profile: Option<ClientProfile>,
    /// Last activity timestamp
    pub last_activity: DateTime<Utc>,
    /// Number of requests processed
    pub request_count: u64,
    /// Connection-specific metadata
    pub metadata: HashMap<String, String>,
}

impl ConnectionState {
    /// Create new connection state
    pub fn new(remote_addr: String) -> Self {
        let id = ConnectionId::new();
        let now = Utc::now();

        debug!("Creating new connection state for {} with ID {}", remote_addr, id);

        Self {
            id,
            remote_addr,
            connected_at: now,
            auth_state: AuthenticationState::Unauthenticated,
            sasl_authenticator: SaslAuthenticator::new(),
            client_profile: None,
            last_activity: now,
            request_count: 0,
            metadata: HashMap::new(),
        }
    }

    /// Update last activity timestamp
    pub fn touch(&mut self) {
        self.last_activity = Utc::now();
        self.request_count += 1;
    }

    /// Check if connection requires authentication
    pub fn requires_authentication(&self) -> bool {
        !self.auth_state.is_authenticated()
    }

    /// Update authentication state to handshake in progress
    pub fn start_sasl_handshake(&mut self) {
        debug!("Connection {} starting SASL handshake", self.id);
        self.auth_state = AuthenticationState::SaslHandshakeInProgress;
        self.touch();
    }

    /// Update authentication state to authentication in progress
    pub fn start_sasl_authentication(&mut self) {
        debug!("Connection {} starting SASL authentication", self.id);
        self.auth_state = AuthenticationState::SaslAuthenticationInProgress;
        self.touch();
    }

    /// Mark connection as successfully authenticated
    pub fn mark_authenticated(&mut self, username: String, mechanism: String) {
        info!("Connection {} successfully authenticated as {} using {}",
              self.id, username, mechanism);
        self.auth_state = AuthenticationState::Authenticated {
            username,
            mechanism,
            authenticated_at: Utc::now(),
        };
        self.touch();
    }

    /// Mark connection as authentication failed
    pub fn mark_authentication_failed(&mut self, reason: String) {
        warn!("Connection {} authentication failed: {}", self.id, reason);
        self.auth_state = AuthenticationState::AuthenticationFailed {
            reason,
            failed_at: Utc::now(),
        };
        self.touch();
    }

    /// Set client profile after detection
    pub fn set_client_profile(&mut self, profile: ClientProfile) {
        info!("Connection {} identified as {:?} client", self.id, profile.client_type);
        self.client_profile = Some(profile);
    }

    /// Get connection info for logging
    pub fn info(&self) -> String {
        format!("{}@{} ({})", self.id, self.remote_addr,
                match &self.auth_state {
                    AuthenticationState::Authenticated { username, .. } =>
                        format!("authenticated as {}", username),
                    AuthenticationState::Unauthenticated => "unauthenticated".to_string(),
                    AuthenticationState::SaslHandshakeInProgress => "handshake".to_string(),
                    AuthenticationState::SaslAuthenticationInProgress => "authenticating".to_string(),
                    AuthenticationState::AuthenticationFailed { .. } => "auth-failed".to_string(),
                })
    }
}

/// Connection registry that manages all active connections
#[derive(Debug)]
pub struct ConnectionRegistry {
    connections: RwLock<HashMap<ConnectionId, Arc<RwLock<ConnectionState>>>>,
    /// Client compatibility registry
    client_registry: RwLock<ClientRegistry>,
    /// Optional SASL configuration
    sasl_enabled: bool,
    /// Allowed authentication mechanisms
    allowed_mechanisms: Vec<String>,
}

impl ConnectionRegistry {
    /// Create a new connection registry
    pub fn new(sasl_enabled: bool) -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
            client_registry: RwLock::new(ClientRegistry::new()),
            sasl_enabled,
            allowed_mechanisms: vec![
                "PLAIN".to_string(),
                "SCRAM-SHA-256".to_string(),
                "SCRAM-SHA-512".to_string(),
            ],
        }
    }

    /// Create a new connection registry with custom mechanisms
    pub fn with_mechanisms(sasl_enabled: bool, mechanisms: Vec<String>) -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
            client_registry: RwLock::new(ClientRegistry::new()),
            sasl_enabled,
            allowed_mechanisms: mechanisms,
        }
    }

    /// Register a new connection
    pub async fn register_connection(&self, remote_addr: String) -> ConnectionId {
        let connection_state = Arc::new(RwLock::new(ConnectionState::new(remote_addr)));
        let connection_id = connection_state.read().await.id;

        let mut connections = self.connections.write().await;
        connections.insert(connection_id, connection_state);

        info!("Registered new connection: {}", connection_id);
        connection_id
    }

    /// Remove a connection from the registry
    pub async fn remove_connection(&self, connection_id: ConnectionId) {
        let mut connections = self.connections.write().await;
        if let Some(conn_state) = connections.remove(&connection_id) {
            let state = conn_state.read().await;
            info!("Removed connection: {}", state.info());
        } else {
            warn!("Attempted to remove unknown connection: {}", connection_id);
        }
    }

    /// Get connection state
    pub async fn get_connection(&self, connection_id: ConnectionId) -> Option<Arc<RwLock<ConnectionState>>> {
        let connections = self.connections.read().await;
        connections.get(&connection_id).cloned()
    }

    /// Check if SASL is enabled
    pub fn is_sasl_enabled(&self) -> bool {
        self.sasl_enabled
    }

    /// Get allowed SASL mechanisms
    pub fn allowed_mechanisms(&self) -> &[String] {
        &self.allowed_mechanisms
    }

    /// Check if a connection is authenticated
    pub async fn is_authenticated(&self, connection_id: ConnectionId) -> bool {
        if !self.sasl_enabled {
            return true; // If SASL is disabled, all connections are considered authenticated
        }

        if let Some(conn_state) = self.get_connection(connection_id).await {
            let state = conn_state.read().await;
            state.auth_state.is_authenticated()
        } else {
            false
        }
    }

    /// Validate that a connection can make API requests
    pub async fn validate_connection_for_request(&self, connection_id: ConnectionId, api_key: i16) -> Result<(), String> {
        if let Some(conn_state) = self.get_connection(connection_id).await {
            let state = conn_state.read().await;

            // Allow SaslHandshake (17) and SaslAuthenticate (36) without authentication
            if api_key == 17 || api_key == 36 {
                return Ok(());
            }

            // Allow ApiVersions (18) without authentication (standard Kafka behavior)
            if api_key == 18 {
                return Ok(());
            }

            // If SASL is enabled, require authentication for all other requests
            if self.sasl_enabled && !state.auth_state.is_authenticated() {
                return Err(format!("Connection {} is not authenticated and SASL is required", connection_id));
            }

            Ok(())
        } else {
            Err(format!("Unknown connection: {}", connection_id))
        }
    }

    /// Get connection statistics
    pub async fn get_stats(&self) -> ConnectionStats {
        let connections = self.connections.read().await;
        let mut stats = ConnectionStats::default();

        for conn_state in connections.values() {
            let state = conn_state.read().await;
            stats.total_connections += 1;

            match &state.auth_state {
                AuthenticationState::Authenticated { .. } => stats.authenticated_connections += 1,
                AuthenticationState::Unauthenticated => stats.unauthenticated_connections += 1,
                AuthenticationState::SaslHandshakeInProgress => stats.handshake_in_progress += 1,
                AuthenticationState::SaslAuthenticationInProgress => stats.authentication_in_progress += 1,
                AuthenticationState::AuthenticationFailed { .. } => stats.failed_connections += 1,
            }

            stats.total_requests += state.request_count;
        }

        stats
    }

    /// Set client profile for a connection based on ApiVersions handshake
    pub async fn set_client_profile(
        &self,
        connection_id: ConnectionId,
        client_id: Option<&str>,
        software_name: Option<&str>,
        software_version: Option<&str>,
    ) {
        if let Some(conn_state) = self.get_connection(connection_id).await {
            let mut client_registry = self.client_registry.write().await;
            let profile = client_registry.get_or_create_profile(
                client_id,
                software_name,
                software_version,
            );

            let mut state = conn_state.write().await;
            state.set_client_profile(profile);
        }
    }

    /// Get client profile for a connection
    pub async fn get_client_profile(&self, connection_id: ConnectionId) -> Option<ClientProfile> {
        if let Some(conn_state) = self.get_connection(connection_id).await {
            let state = conn_state.read().await;
            state.client_profile.clone()
        } else {
            None
        }
    }

    /// Cleanup old failed connections
    pub async fn cleanup_old_connections(&self, max_age_hours: i64) {
        let mut connections = self.connections.write().await;
        let cutoff = Utc::now() - chrono::Duration::hours(max_age_hours);

        let mut to_remove = Vec::new();
        for (id, conn_state) in connections.iter() {
            let state = conn_state.read().await;

            // Remove old failed connections or very old inactive connections
            let should_remove = match &state.auth_state {
                AuthenticationState::AuthenticationFailed { failed_at, .. } if *failed_at < cutoff => true,
                _ if state.last_activity < cutoff => true,
                _ => false,
            };

            if should_remove {
                to_remove.push(*id);
            }
        }

        for id in to_remove {
            connections.remove(&id);
            debug!("Cleaned up old connection: {}", id);
        }
    }
}

/// Connection statistics
#[derive(Debug, Default)]
pub struct ConnectionStats {
    pub total_connections: usize,
    pub authenticated_connections: usize,
    pub unauthenticated_connections: usize,
    pub handshake_in_progress: usize,
    pub authentication_in_progress: usize,
    pub failed_connections: usize,
    pub total_requests: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::test;

    #[test]
    async fn test_connection_lifecycle() {
        let registry = ConnectionRegistry::new(true);

        // Register connection
        let conn_id = registry.register_connection("127.0.0.1:12345".to_string()).await;
        assert!(!registry.is_authenticated(conn_id).await);

        // Start authentication flow
        let conn_state = registry.get_connection(conn_id).await.unwrap();
        {
            let mut state = conn_state.write().await;
            state.start_sasl_handshake();
            state.start_sasl_authentication();
            state.mark_authenticated("testuser".to_string(), "PLAIN".to_string());
        }

        assert!(registry.is_authenticated(conn_id).await);

        // Remove connection
        registry.remove_connection(conn_id).await;
        assert!(registry.get_connection(conn_id).await.is_none());
    }

    #[test]
    async fn test_request_validation() {
        let registry = ConnectionRegistry::new(true);
        let conn_id = registry.register_connection("127.0.0.1:12345".to_string()).await;

        // Should allow SASL requests
        assert!(registry.validate_connection_for_request(conn_id, 17).await.is_ok()); // SaslHandshake
        assert!(registry.validate_connection_for_request(conn_id, 36).await.is_ok()); // SaslAuthenticate
        assert!(registry.validate_connection_for_request(conn_id, 18).await.is_ok()); // ApiVersions

        // Should reject other requests when not authenticated
        assert!(registry.validate_connection_for_request(conn_id, 0).await.is_err()); // Produce

        // Authenticate connection
        let conn_state = registry.get_connection(conn_id).await.unwrap();
        {
            let mut state = conn_state.write().await;
            state.mark_authenticated("testuser".to_string(), "PLAIN".to_string());
        }

        // Should now allow all requests
        assert!(registry.validate_connection_for_request(conn_id, 0).await.is_ok()); // Produce
    }

    #[test]
    async fn test_sasl_disabled() {
        let registry = ConnectionRegistry::new(false);
        let conn_id = registry.register_connection("127.0.0.1:12345".to_string()).await;

        // When SASL is disabled, all connections are considered authenticated
        assert!(registry.is_authenticated(conn_id).await);
        assert!(registry.validate_connection_for_request(conn_id, 0).await.is_ok());
    }
}