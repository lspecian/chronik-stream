#!/usr/bin/env rust-script

//! Standalone SASL Authentication Test
//!
//! This tests the core SASL authentication components independently.

use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};

// Simplified version of our SASL components for testing
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ConnectionId(COUNTER.fetch_add(1, Ordering::SeqCst))
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn-{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub enum AuthenticationState {
    Unauthenticated,
    SaslHandshakeInProgress,
    SaslAuthenticationInProgress,
    Authenticated {
        username: String,
        mechanism: String,
        authenticated_at: DateTime<Utc>,
    },
    AuthenticationFailed {
        reason: String,
        failed_at: DateTime<Utc>,
    },
}

impl AuthenticationState {
    pub fn is_authenticated(&self) -> bool {
        matches!(self, AuthenticationState::Authenticated { .. })
    }

    pub fn username(&self) -> Option<&str> {
        match self {
            AuthenticationState::Authenticated { username, .. } => Some(username),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct SimpleConnectionState {
    pub id: ConnectionId,
    pub remote_addr: String,
    pub auth_state: AuthenticationState,
    pub request_count: u64,
}

impl SimpleConnectionState {
    pub fn new(remote_addr: String) -> Self {
        let id = ConnectionId::new();
        Self {
            id,
            remote_addr,
            auth_state: AuthenticationState::Unauthenticated,
            request_count: 0,
        }
    }

    pub fn mark_authenticated(&mut self, username: String, mechanism: String) {
        self.auth_state = AuthenticationState::Authenticated {
            username,
            mechanism,
            authenticated_at: Utc::now(),
        };
    }
}

pub struct SimpleConnectionRegistry {
    connections: RwLock<HashMap<ConnectionId, Arc<RwLock<SimpleConnectionState>>>>,
    sasl_enabled: bool,
}

impl SimpleConnectionRegistry {
    pub fn new(sasl_enabled: bool) -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
            sasl_enabled,
        }
    }

    pub async fn register_connection(&self, remote_addr: String) -> ConnectionId {
        let connection_state = Arc::new(RwLock::new(SimpleConnectionState::new(remote_addr)));
        let connection_id = connection_state.read().await.id;

        let mut connections = self.connections.write().await;
        connections.insert(connection_id, connection_state);

        connection_id
    }

    pub async fn is_authenticated(&self, connection_id: ConnectionId) -> bool {
        if !self.sasl_enabled {
            return true;
        }

        if let Some(conn_state) = self.get_connection(connection_id).await {
            let state = conn_state.read().await;
            state.auth_state.is_authenticated()
        } else {
            false
        }
    }

    pub async fn get_connection(&self, connection_id: ConnectionId) -> Option<Arc<RwLock<SimpleConnectionState>>> {
        let connections = self.connections.read().await;
        connections.get(&connection_id).cloned()
    }

    pub async fn validate_connection_for_request(&self, connection_id: ConnectionId, api_key: i16) -> Result<(), String> {
        // Allow SASL requests without authentication
        if api_key == 17 || api_key == 36 || api_key == 18 {
            return Ok(());
        }

        if self.sasl_enabled && !self.is_authenticated(connection_id).await {
            return Err("Authentication required".to_string());
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 Standalone SASL Test");
    println!("=======================");

    // Test 1: Connection Registry Basic Functionality
    println!("\n📝 Test 1: Connection Registry");
    let registry = Arc::new(SimpleConnectionRegistry::new(true));
    let conn_id = registry.register_connection("192.168.1.100:52341".to_string()).await;
    println!("   ✅ Connection registered: {}", conn_id);

    // Initially not authenticated
    assert!(!registry.is_authenticated(conn_id).await);
    println!("   ✅ Initial state: Not authenticated");

    // SASL requests allowed, others blocked
    assert!(registry.validate_connection_for_request(conn_id, 17).await.is_ok()); // SaslHandshake
    assert!(registry.validate_connection_for_request(conn_id, 36).await.is_ok()); // SaslAuthenticate
    assert!(registry.validate_connection_for_request(conn_id, 0).await.is_err());  // Produce blocked
    println!("   ✅ Request validation working");

    // Mark as authenticated
    if let Some(conn_state) = registry.get_connection(conn_id).await {
        let mut state = conn_state.write().await;
        state.mark_authenticated("test-user".to_string(), "PLAIN".to_string());
    }

    // Now authenticated
    assert!(registry.is_authenticated(conn_id).await);
    assert!(registry.validate_connection_for_request(conn_id, 0).await.is_ok()); // Produce allowed
    println!("   ✅ Authentication state change working");

    // Test 2: SASL Disabled Mode
    println!("\n📝 Test 2: SASL Disabled Mode");
    let no_auth_registry = Arc::new(SimpleConnectionRegistry::new(false));
    let no_auth_conn = no_auth_registry.register_connection("192.168.1.101:52342".to_string()).await;

    assert!(no_auth_registry.is_authenticated(no_auth_conn).await);
    assert!(no_auth_registry.validate_connection_for_request(no_auth_conn, 0).await.is_ok());
    println!("   ✅ SASL disabled mode working - all requests allowed");

    // Test 3: Authentication States
    println!("\n📝 Test 3: Authentication States");
    if let Some(conn_state) = registry.get_connection(conn_id).await {
        let state = conn_state.read().await;
        assert!(state.auth_state.is_authenticated());
        assert_eq!(state.auth_state.username(), Some("test-user"));
        println!("   ✅ Authentication state tracking working");
    }

    println!("\n🎉 All Standalone SASL Tests Passed!");
    println!("=====================================");
    println!("✅ Per-connection state management");
    println!("✅ Authentication enforcement");
    println!("✅ SASL enabled/disabled modes");
    println!("✅ Request validation logic");

    Ok(())
}