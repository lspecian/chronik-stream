//! Authentication middleware for Chronik Stream services.

use crate::{AuthError, AuthResult, UserPrincipal, Acl, Resource, Operation, SaslAuthenticator};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Authentication context attached to connections
#[derive(Clone)]
pub struct AuthContext {
    pub principal: Option<UserPrincipal>,
    pub session_id: String,
}

impl AuthContext {
    /// Create new anonymous context
    pub fn anonymous(session_id: String) -> Self {
        Self {
            principal: None,
            session_id,
        }
    }
    
    /// Create authenticated context
    pub fn authenticated(principal: UserPrincipal, session_id: String) -> Self {
        Self {
            principal: Some(principal),
            session_id,
        }
    }
    
    /// Check if context is authenticated
    pub fn is_authenticated(&self) -> bool {
        self.principal.is_some()
    }
    
    /// Get username if authenticated
    pub fn username(&self) -> Option<&str> {
        self.principal.as_ref().map(|p| p.username.as_str())
    }
}

/// Authentication middleware
pub struct AuthMiddleware {
    acl: Arc<Acl>,
    sasl: Arc<RwLock<SaslAuthenticator>>,
    contexts: Arc<RwLock<HashMap<String, AuthContext>>>,
    allow_anonymous: bool,
}

use std::collections::HashMap;

impl AuthMiddleware {
    /// Create new authentication middleware
    pub fn new(allow_anonymous: bool) -> Self {
        let acl = Arc::new(Acl::new());
        let sasl = Arc::new(RwLock::new(SaslAuthenticator::new()));
        
        Self {
            acl,
            sasl,
            contexts: Arc::new(RwLock::new(HashMap::new())),
            allow_anonymous,
        }
    }
    
    /// Initialize with default settings
    pub async fn init_defaults(&self) -> AuthResult<()> {
        // Initialize default ACLs
        self.acl.init_kafka_defaults()?;
        
        // Add default admin user (for testing)
        let mut sasl = self.sasl.write().await;
        sasl.add_user("admin".to_string(), "admin123".to_string())?;
        
        Ok(())
    }
    
    /// Get or create session context
    pub async fn get_or_create_context(&self, session_id: &str) -> AuthContext {
        let contexts = self.contexts.read().await;
        if let Some(ctx) = contexts.get(session_id) {
            return ctx.clone();
        }
        drop(contexts);
        
        // Create new anonymous context
        let ctx = AuthContext::anonymous(session_id.to_string());
        let mut contexts = self.contexts.write().await;
        contexts.insert(session_id.to_string(), ctx.clone());
        ctx
    }
    
    /// Authenticate session with SASL
    pub async fn authenticate_sasl(
        &self,
        session_id: &str,
        mechanism: &crate::SaslMechanism,
        auth_bytes: &[u8],
    ) -> AuthResult<UserPrincipal> {
        let sasl = self.sasl.read().await;
        let principal = sasl.authenticate(mechanism, auth_bytes)?;
        
        // Update context
        let mut contexts = self.contexts.write().await;
        contexts.insert(
            session_id.to_string(),
            AuthContext::authenticated(principal.clone(), session_id.to_string()),
        );
        
        debug!("Session {} authenticated as {}", session_id, principal.username);
        Ok(principal)
    }
    
    /// Check permission for operation
    pub async fn check_permission(
        &self,
        session_id: &str,
        resource: Resource,
        operation: Operation,
    ) -> AuthResult<()> {
        let ctx = self.get_or_create_context(session_id).await;
        
        if let Some(principal) = &ctx.principal {
            self.acl.check_permission(principal, &resource, &operation)?;
        } else if !self.allow_anonymous {
            return Err(AuthError::PermissionDenied);
        } else {
            // Check if anonymous access is allowed for this operation
            match (&resource, &operation) {
                // Allow anonymous metadata operations
                (_, Operation::Describe) => {}
                // Deny all other operations for anonymous users
                _ => return Err(AuthError::PermissionDenied),
            }
        }
        
        Ok(())
    }
    
    /// Check topic read permission
    pub async fn check_topic_read(&self, session_id: &str, topic: &str) -> AuthResult<()> {
        self.check_permission(
            session_id,
            Resource::Topic(topic.to_string()),
            Operation::Read,
        ).await
    }
    
    /// Check topic write permission
    pub async fn check_topic_write(&self, session_id: &str, topic: &str) -> AuthResult<()> {
        self.check_permission(
            session_id,
            Resource::Topic(topic.to_string()),
            Operation::Write,
        ).await
    }
    
    /// Check consumer group permission
    pub async fn check_consumer_group(&self, session_id: &str, group_id: &str) -> AuthResult<()> {
        self.check_permission(
            session_id,
            Resource::ConsumerGroup(group_id.to_string()),
            Operation::Read,
        ).await
    }
    
    /// Handle disconnection
    pub async fn disconnect(&self, session_id: &str) {
        let mut contexts = self.contexts.write().await;
        if let Some(ctx) = contexts.remove(session_id) {
            if let Some(principal) = &ctx.principal {
                debug!("Session {} (user: {}) disconnected", session_id, principal.username);
            }
        }
    }
    
    /// Get current sessions info
    pub async fn get_sessions_info(&self) -> Vec<(String, Option<String>)> {
        let contexts = self.contexts.read().await;
        contexts
            .iter()
            .map(|(id, ctx)| {
                let username = ctx.principal.as_ref().map(|p| p.username.clone());
                (id.clone(), username)
            })
            .collect()
    }
    
    /// Add user with permissions
    pub async fn add_user_with_permissions(
        &self,
        username: String,
        password: String,
        permissions: Vec<(Resource, Operation, bool)>,
    ) -> AuthResult<()> {
        // Add user to SASL
        let mut sasl = self.sasl.write().await;
        sasl.add_user(username.clone(), password)?;
        drop(sasl);
        
        // Add permissions
        for (resource, operation, allow) in permissions {
            self.acl.add_user_permission(
                &username,
                crate::Permission { resource, operation, allow },
            )?;
        }
        
        Ok(())
    }
    
    /// Get supported SASL mechanisms
    pub async fn get_sasl_mechanisms(&self, requested: &[String]) -> Vec<String> {
        let sasl = self.sasl.read().await;
        sasl.handshake(requested)
    }
}

/// Kafka API key for authorization checks
#[derive(Debug, Clone, Copy)]
pub enum KafkaApiKey {
    Produce = 0,
    Fetch = 1,
    ListOffsets = 2,
    Metadata = 3,
    LeaderAndIsr = 4,
    StopReplica = 5,
    UpdateMetadata = 6,
    ControlledShutdown = 7,
    OffsetCommit = 8,
    OffsetFetch = 9,
    FindCoordinator = 10,
    JoinGroup = 11,
    Heartbeat = 12,
    LeaveGroup = 13,
    SyncGroup = 14,
    DescribeGroups = 15,
    ListGroups = 16,
    SaslHandshake = 17,
    ApiVersions = 18,
    CreateTopics = 19,
    DeleteTopics = 20,
    DeleteRecords = 21,
    InitProducerId = 22,
    OffsetForLeaderEpoch = 23,
    AddPartitionsToTxn = 24,
    AddOffsetsToTxn = 25,
    EndTxn = 26,
    WriteTxnMarkers = 27,
    TxnOffsetCommit = 28,
    DescribeAcls = 29,
    CreateAcls = 30,
    DeleteAcls = 31,
    DescribeConfigs = 32,
    AlterConfigs = 33,
    AlterReplicaLogDirs = 34,
    DescribeLogDirs = 35,
    SaslAuthenticate = 36,
    CreatePartitions = 37,
    CreateDelegationToken = 38,
    RenewDelegationToken = 39,
    ExpireDelegationToken = 40,
    DescribeDelegationToken = 41,
    DeleteGroups = 42,
    ElectLeaders = 43,
    IncrementalAlterConfigs = 44,
    AlterPartitionReassignments = 45,
    ListPartitionReassignments = 46,
    OffsetDelete = 47,
}

impl KafkaApiKey {
    /// Get required permission for API key
    pub fn required_permission(&self) -> Option<(ResourceType, Operation)> {
        match self {
            Self::Produce => Some((ResourceType::Topic, Operation::Write)),
            Self::Fetch => Some((ResourceType::Topic, Operation::Read)),
            Self::ListOffsets => Some((ResourceType::Topic, Operation::Read)),
            Self::Metadata => Some((ResourceType::Cluster, Operation::Describe)),
            Self::OffsetCommit => Some((ResourceType::ConsumerGroup, Operation::Write)),
            Self::OffsetFetch => Some((ResourceType::ConsumerGroup, Operation::Read)),
            Self::FindCoordinator => Some((ResourceType::Cluster, Operation::Describe)),
            Self::JoinGroup => Some((ResourceType::ConsumerGroup, Operation::Write)),
            Self::Heartbeat => Some((ResourceType::ConsumerGroup, Operation::Write)),
            Self::LeaveGroup => Some((ResourceType::ConsumerGroup, Operation::Write)),
            Self::SyncGroup => Some((ResourceType::ConsumerGroup, Operation::Write)),
            Self::DescribeGroups => Some((ResourceType::ConsumerGroup, Operation::Describe)),
            Self::ListGroups => Some((ResourceType::Cluster, Operation::Describe)),
            Self::CreateTopics => Some((ResourceType::Cluster, Operation::Create)),
            Self::DeleteTopics => Some((ResourceType::Topic, Operation::Delete)),
            Self::DescribeConfigs => Some((ResourceType::Cluster, Operation::Describe)),
            Self::AlterConfigs => Some((ResourceType::Cluster, Operation::Alter)),
            Self::SaslHandshake | Self::SaslAuthenticate | Self::ApiVersions => None,
            _ => Some((ResourceType::Cluster, Operation::ClusterAction)),
        }
    }
}

/// Resource type for permission checks
#[derive(Debug, Clone)]
pub enum ResourceType {
    Topic,
    ConsumerGroup,
    Cluster,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_auth_middleware() {
        let middleware = AuthMiddleware::new(false);
        middleware.init_defaults().await.unwrap();
        
        // Test anonymous access
        let session_id = "test-session";
        let result = middleware.check_topic_read(session_id, "test-topic").await;
        assert!(result.is_err()); // Should fail without auth
        
        // Test authentication
        let auth_bytes = b"\0admin\0admin123";
        let principal = middleware
            .authenticate_sasl(session_id, &crate::SaslMechanism::Plain, auth_bytes)
            .await
            .unwrap();
        assert_eq!(principal.username, "admin");
        
        // Test authenticated access
        let result = middleware.check_topic_read(session_id, "test-topic").await;
        assert!(result.is_ok()); // Should succeed with auth
    }
    
    #[tokio::test]
    async fn test_session_management() {
        let middleware = AuthMiddleware::new(true);
        
        // Create anonymous session
        let ctx1 = middleware.get_or_create_context("session1").await;
        assert!(!ctx1.is_authenticated());
        
        // Get existing session
        let ctx2 = middleware.get_or_create_context("session1").await;
        assert_eq!(ctx1.session_id, ctx2.session_id);
        
        // Check sessions info
        let sessions = middleware.get_sessions_info().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "session1");
        assert!(sessions[0].1.is_none());
        
        // Disconnect session
        middleware.disconnect("session1").await;
        let sessions = middleware.get_sessions_info().await;
        assert_eq!(sessions.len(), 0);
    }
}