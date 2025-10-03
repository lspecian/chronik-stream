//! SASL Authentication Integration Test
//!
//! This module contains comprehensive tests for the SASL authentication
//! implementation including handshake, authentication, and connection state management.

#[cfg(test)]
mod tests {
    use crate::connection_state::{ConnectionRegistry, AuthenticationState};
    use crate::integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};
    use chronik_protocol::parser::ApiKey;
    use std::sync::Arc;

    /// Test data for SASL authentication
    const TEST_USERNAME: &str = "admin";
    const TEST_PASSWORD: &str = "admin123";
    const SASL_PLAIN_MECHANISM: &str = "PLAIN";


    #[tokio::test]
    async fn test_connection_registry_lifecycle() {
        // Test connection registry basic functionality
        let registry = Arc::new(ConnectionRegistry::new(true));

        // Register a connection
        let conn_id = registry.register_connection("127.0.0.1:12345".to_string()).await;
        assert!(!registry.is_authenticated(conn_id).await);

        // Test authentication validation
        assert!(registry.validate_connection_for_request(conn_id, 17).await.is_ok()); // SaslHandshake allowed
        assert!(registry.validate_connection_for_request(conn_id, 36).await.is_ok()); // SaslAuthenticate allowed
        assert!(registry.validate_connection_for_request(conn_id, 18).await.is_ok()); // ApiVersions allowed
        assert!(registry.validate_connection_for_request(conn_id, 0).await.is_err()); // Produce not allowed

        // Simulate authentication process
        if let Some(conn_state) = registry.get_connection(conn_id).await {
            let mut state = conn_state.write().await;

            // Start handshake
            state.start_sasl_handshake();
            assert!(matches!(state.auth_state, AuthenticationState::SaslHandshakeInProgress));

            // Start authentication
            state.start_sasl_authentication();
            assert!(matches!(state.auth_state, AuthenticationState::SaslAuthenticationInProgress));

            // Mark as authenticated
            state.mark_authenticated(TEST_USERNAME.to_string(), SASL_PLAIN_MECHANISM.to_string());
            assert!(state.auth_state.is_authenticated());
            assert_eq!(state.auth_state.username(), Some(TEST_USERNAME));
        }

        // Verify authentication works
        assert!(registry.is_authenticated(conn_id).await);
        assert!(registry.validate_connection_for_request(conn_id, 0).await.is_ok()); // Produce now allowed

        // Clean up
        registry.remove_connection(conn_id).await;
        assert!(registry.get_connection(conn_id).await.is_none());
    }


    #[tokio::test]
    async fn test_sasl_authentication_failure() {
        // Create connection registry with SASL enabled
        let registry = Arc::new(ConnectionRegistry::new(true));
        let connection_id = registry.register_connection("127.0.0.1:12345".to_string()).await;

        // Test with wrong credentials
        if let Some(conn_state) = registry.get_connection(connection_id).await {
            let mut state = conn_state.write().await;

            // Simulate failed authentication
            state.start_sasl_handshake();
            state.start_sasl_authentication();

            // Try with wrong credentials
            let auth_bytes = format!("\0{}\0{}", "admin", "wrong-password").into_bytes();
            let result = state.sasl_authenticator.handle_authenticate(&auth_bytes);

            // Should fail
            assert!(result.is_err() || result.unwrap().error_code != 0);

            // Mark as failed
            state.mark_authentication_failed("Invalid credentials".to_string());
            assert!(!state.auth_state.is_authenticated());
            assert!(matches!(state.auth_state, AuthenticationState::AuthenticationFailed { .. }));
        }

        // Verify connection is not authenticated
        assert!(!registry.is_authenticated(connection_id).await);
        assert!(registry.validate_connection_for_request(connection_id, 0).await.is_err());
    }

    #[tokio::test]
    async fn test_sasl_disabled_mode() {
        // Create connection registry with SASL disabled
        let registry = Arc::new(ConnectionRegistry::new(false));
        let connection_id = registry.register_connection("127.0.0.1:12345".to_string()).await;

        // When SASL is disabled, all connections should be considered authenticated
        assert!(registry.is_authenticated(connection_id).await);
        assert!(registry.validate_connection_for_request(connection_id, 0).await.is_ok());
        assert!(registry.validate_connection_for_request(connection_id, 1).await.is_ok());
    }

    #[tokio::test]
    async fn test_connection_registry_stats() {
        let registry = Arc::new(ConnectionRegistry::new(true));

        // Create multiple connections in different states
        let conn1 = registry.register_connection("127.0.0.1:1".to_string()).await;
        let conn2 = registry.register_connection("127.0.0.1:2".to_string()).await;
        let conn3 = registry.register_connection("127.0.0.1:3".to_string()).await;

        // Set different authentication states
        if let Some(state) = registry.get_connection(conn1).await {
            let mut s = state.write().await;
            s.mark_authenticated("user1".to_string(), "PLAIN".to_string());
        }

        if let Some(state) = registry.get_connection(conn2).await {
            let mut s = state.write().await;
            s.start_sasl_handshake();
        }

        if let Some(state) = registry.get_connection(conn3).await {
            let mut s = state.write().await;
            s.mark_authentication_failed("Failed auth".to_string());
        }

        // Get stats
        let stats = registry.get_stats().await;
        assert_eq!(stats.total_connections, 3);
        assert_eq!(stats.authenticated_connections, 1);
        assert_eq!(stats.handshake_in_progress, 1);
        assert_eq!(stats.failed_connections, 1);

        // Clean up
        registry.remove_connection(conn1).await;
        registry.remove_connection(conn2).await;
        registry.remove_connection(conn3).await;
    }

    #[tokio::test]
    async fn test_unsupported_sasl_mechanism() {
        let registry = Arc::new(ConnectionRegistry::with_mechanisms(true, vec!["PLAIN".to_string()]));
        let connection_id = registry.register_connection("127.0.0.1:12345".to_string()).await;

        if let Some(conn_state) = registry.get_connection(connection_id).await {
            let mut state = conn_state.write().await;

            // Try unsupported mechanism
            let result = state.sasl_authenticator.handle_handshake(1, &["GSSAPI".to_string()]);

            // Should fail
            assert!(result.is_err());
        }
    }

    /// Integration test that simulates a complete client connection flow
    #[tokio::test]
    async fn test_complete_client_flow_simulation() {
        // This test simulates what a real Kafka client would do:
        // 1. Connect to server
        // 2. Send SASL handshake
        // 3. Send SASL authenticate
        // 4. Send other requests (should work after auth)

        let registry = Arc::new(ConnectionRegistry::new(true));
        let connection_id = registry.register_connection("test-client".to_string()).await;

        // Step 1: Verify initial state
        assert!(!registry.is_authenticated(connection_id).await);

        // Step 2: Handshake should be allowed
        assert!(registry.validate_connection_for_request(connection_id, 17).await.is_ok());

        // Step 3: Simulate handshake success
        if let Some(conn_state) = registry.get_connection(connection_id).await {
            let mut state = conn_state.write().await;
            state.start_sasl_handshake();
            // Handshake would happen here...
        }

        // Step 4: Authenticate should be allowed
        assert!(registry.validate_connection_for_request(connection_id, 36).await.is_ok());

        // Step 5: Simulate authentication success
        if let Some(conn_state) = registry.get_connection(connection_id).await {
            let mut state = conn_state.write().await;
            state.start_sasl_authentication();
            state.mark_authenticated("test-user".to_string(), "PLAIN".to_string());
        }

        // Step 6: Verify connection is now authenticated
        assert!(registry.is_authenticated(connection_id).await);

        // Step 7: All other requests should now be allowed
        assert!(registry.validate_connection_for_request(connection_id, 0).await.is_ok()); // Produce
        assert!(registry.validate_connection_for_request(connection_id, 1).await.is_ok()); // Fetch
        assert!(registry.validate_connection_for_request(connection_id, 3).await.is_ok()); // Metadata

        // Step 8: Connection info should be properly tracked
        if let Some(conn_state) = registry.get_connection(connection_id).await {
            let state = conn_state.read().await;
            assert!(state.auth_state.is_authenticated());
            assert_eq!(state.auth_state.username(), Some("test-user"));
            assert_eq!(state.auth_state.mechanism(), Some("PLAIN"));
            assert!(state.request_count > 0);
        }

        println!("✅ Complete SASL authentication flow test passed!");
    }
}