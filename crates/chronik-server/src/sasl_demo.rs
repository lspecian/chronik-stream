//! SASL Authentication Demo
//!
//! This demonstrates the SASL authentication flow that has been implemented.

use crate::connection_state::{ConnectionRegistry, AuthenticationState};
use std::sync::Arc;

pub async fn demonstrate_sasl_flow() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 SASL Authentication Flow Demo");
    println!("=====================================");

    // Create connection registry with SASL enabled
    let registry = Arc::new(ConnectionRegistry::new(true));

    // Simulate a new client connection
    let connection_id = registry.register_connection("client@192.168.1.100:52341".to_string()).await;
    println!("✅ Connection registered: {}", connection_id);

    // Step 1: Verify initial state - not authenticated
    assert!(!registry.is_authenticated(connection_id).await);
    println!("✅ Initial state: Connection is NOT authenticated");

    // Step 2: Verify SASL requests are allowed but others are not
    assert!(registry.validate_connection_for_request(connection_id, 17).await.is_ok()); // SaslHandshake
    assert!(registry.validate_connection_for_request(connection_id, 36).await.is_ok()); // SaslAuthenticate
    assert!(registry.validate_connection_for_request(connection_id, 18).await.is_ok()); // ApiVersions
    assert!(registry.validate_connection_for_request(connection_id, 0).await.is_err());  // Produce - not allowed
    println!("✅ Authentication enforcement: SASL requests allowed, others blocked");

    // Step 3: Start SASL handshake
    if let Some(conn_state) = registry.get_connection(connection_id).await {
        let mut state = conn_state.write().await;
        state.start_sasl_handshake();
        assert!(matches!(state.auth_state, AuthenticationState::SaslHandshakeInProgress));
        println!("✅ SASL handshake started");
    }

    // Step 4: Start SASL authentication
    if let Some(conn_state) = registry.get_connection(connection_id).await {
        let mut state = conn_state.write().await;
        state.start_sasl_authentication();
        assert!(matches!(state.auth_state, AuthenticationState::SaslAuthenticationInProgress));
        println!("✅ SASL authentication started");

        // Test successful authentication with built-in credentials
        let auth_bytes = format!("\0admin\0admin123").into_bytes();
        match state.sasl_authenticator.handle_authenticate(&auth_bytes) {
            Ok(response) => {
                if response.error_code == 0 {
                    if let Some(username) = state.sasl_authenticator.username() {
                        state.mark_authenticated(username.to_string(), "PLAIN".to_string());
                        println!("✅ SASL PLAIN authentication successful for user: {}", username);
                    }
                } else {
                    println!("❌ SASL authentication failed with error code: {}", response.error_code);
                }
            }
            Err(e) => {
                println!("❌ SASL authentication error: {:?}", e);
            }
        }
    }

    // Step 5: Verify connection is now authenticated
    assert!(registry.is_authenticated(connection_id).await);
    println!("✅ Connection is now AUTHENTICATED");

    // Step 6: Verify all requests are now allowed
    assert!(registry.validate_connection_for_request(connection_id, 0).await.is_ok());  // Produce
    assert!(registry.validate_connection_for_request(connection_id, 1).await.is_ok());  // Fetch
    assert!(registry.validate_connection_for_request(connection_id, 3).await.is_ok());  // Metadata
    println!("✅ All Kafka API requests now allowed");

    // Step 7: Display connection info
    if let Some(conn_state) = registry.get_connection(connection_id).await {
        let state = conn_state.read().await;
        println!("📊 Connection Info: {}", state.info());
        assert_eq!(state.auth_state.username(), Some("admin"));
        assert_eq!(state.auth_state.mechanism(), Some("PLAIN"));
    }

    // Step 8: Test connection registry statistics
    let stats = registry.get_stats().await;
    println!("📊 Registry Stats:");
    println!("   - Total connections: {}", stats.total_connections);
    println!("   - Authenticated: {}", stats.authenticated_connections);
    println!("   - Unauthenticated: {}", stats.unauthenticated_connections);
    println!("   - In progress: {}", stats.handshake_in_progress + stats.authentication_in_progress);
    println!("   - Failed: {}", stats.failed_connections);
    println!("   - Total requests: {}", stats.total_requests);

    // Step 9: Test authentication failure scenario
    println!("\n🔒 Testing Authentication Failure");
    let failed_conn = registry.register_connection("bad-client@192.168.1.101:52342".to_string()).await;

    if let Some(conn_state) = registry.get_connection(failed_conn).await {
        let mut state = conn_state.write().await;
        state.start_sasl_handshake();
        state.start_sasl_authentication();

        // Try with wrong credentials
        let bad_auth_bytes = format!("\0admin\0wrong-password").into_bytes();
        match state.sasl_authenticator.handle_authenticate(&bad_auth_bytes) {
            Ok(response) => {
                if response.error_code != 0 {
                    state.mark_authentication_failed("Invalid credentials".to_string());
                    println!("✅ Authentication correctly failed for bad credentials");
                }
            }
            Err(_) => {
                state.mark_authentication_failed("Invalid credentials".to_string());
                println!("✅ Authentication correctly failed for bad credentials");
            }
        }
        assert!(!state.auth_state.is_authenticated());
    }

    // Step 10: Test SASL disabled mode
    println!("\n🔓 Testing SASL Disabled Mode");
    let no_sasl_registry = Arc::new(ConnectionRegistry::new(false));
    let no_sasl_conn = no_sasl_registry.register_connection("client@192.168.1.102:52343".to_string()).await;

    // When SASL is disabled, all connections should be considered authenticated
    assert!(no_sasl_registry.is_authenticated(no_sasl_conn).await);
    assert!(no_sasl_registry.validate_connection_for_request(no_sasl_conn, 0).await.is_ok());
    println!("✅ SASL disabled mode: All requests allowed without authentication");

    // Cleanup
    registry.remove_connection(connection_id).await;
    registry.remove_connection(failed_conn).await;
    no_sasl_registry.remove_connection(no_sasl_conn).await;
    println!("✅ Connections cleaned up");

    println!("\n🎉 SASL Authentication Flow Demo Complete!");
    println!("=====================================");
    println!("✅ Per-connection state management working");
    println!("✅ SASL handshake and authentication working");
    println!("✅ Connection-aware request validation working");
    println!("✅ Authentication enforcement working");
    println!("✅ PLAIN SASL mechanism working");
    println!("✅ Connection lifecycle management working");

    Ok(())
}