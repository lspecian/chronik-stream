//! ACL (Access Control List) Storage and Authorization
//!
//! This module provides:
//! - ACL storage and persistence
//! - Authorization enforcement for Kafka operations
//! - Support for resource types: Topic, Group, Cluster, TransactionalId
//!
//! # Usage
//!
//! ACLs are stored in memory and optionally persisted to disk.
//! Authorization is checked before each operation.
//!
//! # ACL Format
//!
//! Each ACL entry specifies:
//! - Resource (type + name + pattern)
//! - Principal (user identity)
//! - Host (client host)
//! - Operation (read, write, create, etc.)
//! - Permission (allow/deny)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use chronik_protocol::describe_acls_types::{
    AclEntry, AclOperation, AclPermissionType, PatternType, ResourceAcls, ResourceType,
};

/// ACL binding - a complete ACL entry with resource and access info
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AclBinding {
    /// Resource type (Topic, Group, Cluster, etc.)
    pub resource_type: ResourceType,
    /// Resource name (topic name, group id, etc.)
    pub resource_name: String,
    /// Pattern type (Literal, Prefixed, etc.)
    pub pattern_type: PatternType,
    /// Principal (e.g., "User:alice")
    pub principal: String,
    /// Host pattern (e.g., "*" for any host)
    pub host: String,
    /// Operation being allowed/denied
    pub operation: AclOperation,
    /// Permission type (Allow/Deny)
    pub permission_type: AclPermissionType,
}

/// Result of an authorization check
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationResult {
    /// Access is allowed
    Allowed,
    /// Access is denied by explicit deny rule
    Denied,
    /// No matching ACL found (depends on default behavior)
    NoMatchingAcl,
}

/// ACL Store - manages ACL entries
pub struct AclStore {
    /// ACLs indexed by resource type and name
    acls: RwLock<HashMap<(ResourceType, String), Vec<AclBinding>>>,

    /// Whether authorization is enabled
    enabled: bool,

    /// Whether to allow operations when no ACL matches (permissive mode)
    /// If false (default), unmatched operations are denied
    allow_if_no_acl: bool,

    /// Super users who bypass all ACL checks
    super_users: Vec<String>,
}

impl Default for AclStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AclStore {
    /// Create a new ACL store
    pub fn new() -> Self {
        // Check for super users from environment
        let super_users: Vec<String> = std::env::var("CHRONIK_ACL_SUPER_USERS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.trim().to_string())
            .collect();

        // Check if ACLs are enabled
        let enabled = std::env::var("CHRONIK_ACL_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        // Check default behavior
        let allow_if_no_acl = std::env::var("CHRONIK_ACL_ALLOW_IF_NO_ACL")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(true); // Default to permissive for compatibility

        if enabled {
            info!("ACL authorization enabled");
            if !super_users.is_empty() {
                info!("Super users configured: {:?}", super_users);
            }
            if allow_if_no_acl {
                info!("Permissive mode: operations without matching ACLs are ALLOWED");
            } else {
                warn!("Strict mode: operations without matching ACLs are DENIED");
            }
        } else {
            info!("ACL authorization disabled (set CHRONIK_ACL_ENABLED=true to enable)");
        }

        Self {
            acls: RwLock::new(HashMap::new()),
            enabled,
            allow_if_no_acl,
            super_users,
        }
    }

    /// Check if authorization is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Create a new ACL binding
    pub async fn create_acl(&self, binding: AclBinding) -> Result<(), AclError> {
        if !self.enabled {
            debug!("ACL creation ignored (ACLs disabled)");
            return Ok(());
        }

        let key = (binding.resource_type, binding.resource_name.clone());

        let mut acls = self.acls.write().await;
        let entries = acls.entry(key).or_insert_with(Vec::new);

        // Check for duplicate
        if entries.iter().any(|e| e == &binding) {
            return Err(AclError::DuplicateAcl);
        }

        info!(
            "Creating ACL: {:?} {:?} {} -> {} {} {:?}",
            binding.resource_type,
            binding.pattern_type,
            binding.resource_name,
            binding.principal,
            binding.host,
            binding.operation
        );

        entries.push(binding);
        Ok(())
    }

    /// Delete ACLs matching a filter
    pub async fn delete_acls(&self, filter: &AclFilter) -> Vec<AclBinding> {
        if !self.enabled {
            return Vec::new();
        }

        let mut acls = self.acls.write().await;
        let mut deleted = Vec::new();

        // Iterate through all ACLs and remove matching ones
        for entries in acls.values_mut() {
            let mut i = 0;
            while i < entries.len() {
                if filter.matches(&entries[i]) {
                    deleted.push(entries.remove(i));
                } else {
                    i += 1;
                }
            }
        }

        if !deleted.is_empty() {
            info!("Deleted {} ACLs matching filter", deleted.len());
        }

        deleted
    }

    /// Describe ACLs matching a filter
    pub async fn describe_acls(&self, filter: &AclFilter) -> Vec<ResourceAcls> {
        let acls = self.acls.read().await;
        let mut results: HashMap<(ResourceType, String, PatternType), Vec<AclEntry>> =
            HashMap::new();

        for entries in acls.values() {
            for entry in entries {
                if filter.matches(entry) {
                    let key = (entry.resource_type, entry.resource_name.clone(), entry.pattern_type);
                    results.entry(key).or_insert_with(Vec::new).push(AclEntry {
                        principal: entry.principal.clone(),
                        host: entry.host.clone(),
                        operation: entry.operation,
                        permission_type: entry.permission_type,
                    });
                }
            }
        }

        results
            .into_iter()
            .map(|((resource_type, resource_name, pattern_type), acls)| ResourceAcls {
                resource_type,
                resource_name,
                resource_pattern_type: pattern_type,
                acls,
            })
            .collect()
    }

    /// Authorize an operation
    ///
    /// Returns true if the operation is allowed, false if denied.
    pub async fn authorize(
        &self,
        principal: &str,
        host: &str,
        resource_type: ResourceType,
        resource_name: &str,
        operation: AclOperation,
    ) -> bool {
        // If ACLs are disabled, allow everything
        if !self.enabled {
            return true;
        }

        // Super users bypass all checks
        if self.super_users.iter().any(|u| u == principal) {
            debug!("Super user {} bypasses ACL check", principal);
            return true;
        }

        let result = self
            .check_authorization(principal, host, resource_type, resource_name, operation)
            .await;

        match result {
            AuthorizationResult::Allowed => {
                debug!(
                    "ALLOWED: {} from {} -> {:?} {} {:?}",
                    principal, host, resource_type, resource_name, operation
                );
                true
            }
            AuthorizationResult::Denied => {
                warn!(
                    "DENIED: {} from {} -> {:?} {} {:?}",
                    principal, host, resource_type, resource_name, operation
                );
                false
            }
            AuthorizationResult::NoMatchingAcl => {
                if self.allow_if_no_acl {
                    debug!(
                        "NO ACL (allowed): {} from {} -> {:?} {} {:?}",
                        principal, host, resource_type, resource_name, operation
                    );
                    true
                } else {
                    warn!(
                        "NO ACL (denied): {} from {} -> {:?} {} {:?}",
                        principal, host, resource_type, resource_name, operation
                    );
                    false
                }
            }
        }
    }

    /// Check authorization and return detailed result
    async fn check_authorization(
        &self,
        principal: &str,
        host: &str,
        resource_type: ResourceType,
        resource_name: &str,
        operation: AclOperation,
    ) -> AuthorizationResult {
        let acls = self.acls.read().await;

        // Look for matching ACLs
        // Order: Check exact resource name first, then prefixed patterns

        // Check exact match
        if let Some(entries) = acls.get(&(resource_type, resource_name.to_string())) {
            if let Some(result) = self.check_entries(entries, principal, host, operation) {
                return result;
            }
        }

        // Check prefixed patterns
        for ((rt, rn), entries) in acls.iter() {
            if *rt != resource_type {
                continue;
            }

            for entry in entries {
                if entry.pattern_type == PatternType::Prefixed && resource_name.starts_with(rn) {
                    if self.entry_matches(entry, principal, host, operation) {
                        return if entry.permission_type == AclPermissionType::Allow {
                            AuthorizationResult::Allowed
                        } else {
                            AuthorizationResult::Denied
                        };
                    }
                }
            }
        }

        // Check wildcard resource name (*)
        if let Some(entries) = acls.get(&(resource_type, "*".to_string())) {
            if let Some(result) = self.check_entries(entries, principal, host, operation) {
                return result;
            }
        }

        AuthorizationResult::NoMatchingAcl
    }

    /// Check a list of ACL entries for a match
    fn check_entries(
        &self,
        entries: &[AclBinding],
        principal: &str,
        host: &str,
        operation: AclOperation,
    ) -> Option<AuthorizationResult> {
        // First check for explicit deny
        for entry in entries {
            if entry.permission_type == AclPermissionType::Deny
                && self.entry_matches(entry, principal, host, operation)
            {
                return Some(AuthorizationResult::Denied);
            }
        }

        // Then check for allow
        for entry in entries {
            if entry.permission_type == AclPermissionType::Allow
                && self.entry_matches(entry, principal, host, operation)
            {
                return Some(AuthorizationResult::Allowed);
            }
        }

        None
    }

    /// Check if an ACL entry matches the request
    fn entry_matches(
        &self,
        entry: &AclBinding,
        principal: &str,
        host: &str,
        operation: AclOperation,
    ) -> bool {
        // Check principal
        let principal_matches = entry.principal == "*"
            || entry.principal == principal
            || entry.principal == "User:*";

        // Check host
        let host_matches = entry.host == "*" || entry.host == host;

        // Check operation
        let operation_matches = entry.operation == AclOperation::All
            || entry.operation == AclOperation::Any
            || entry.operation == operation;

        principal_matches && host_matches && operation_matches
    }
}

/// ACL filter for queries
#[derive(Debug, Clone)]
pub struct AclFilter {
    pub resource_type: Option<ResourceType>,
    pub resource_name: Option<String>,
    pub pattern_type: Option<PatternType>,
    pub principal: Option<String>,
    pub host: Option<String>,
    pub operation: Option<AclOperation>,
    pub permission_type: Option<AclPermissionType>,
}

impl Default for AclFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl AclFilter {
    /// Create a filter that matches everything
    pub fn new() -> Self {
        Self {
            resource_type: None,
            resource_name: None,
            pattern_type: None,
            principal: None,
            host: None,
            operation: None,
            permission_type: None,
        }
    }

    /// Check if a binding matches this filter
    pub fn matches(&self, binding: &AclBinding) -> bool {
        if let Some(rt) = self.resource_type {
            if rt != ResourceType::Any && rt != binding.resource_type {
                return false;
            }
        }

        if let Some(ref rn) = self.resource_name {
            if rn != &binding.resource_name {
                return false;
            }
        }

        if let Some(pt) = self.pattern_type {
            if pt != PatternType::Any && pt != binding.pattern_type {
                return false;
            }
        }

        if let Some(ref p) = self.principal {
            if p != &binding.principal {
                return false;
            }
        }

        if let Some(ref h) = self.host {
            if h != &binding.host {
                return false;
            }
        }

        if let Some(op) = self.operation {
            if op != AclOperation::Any && op != binding.operation {
                return false;
            }
        }

        if let Some(pt) = self.permission_type {
            if pt != AclPermissionType::Any && pt != binding.permission_type {
                return false;
            }
        }

        true
    }
}

/// ACL error types
#[derive(Debug, Clone, thiserror::Error)]
pub enum AclError {
    #[error("Duplicate ACL entry")]
    DuplicateAcl,

    #[error("Invalid resource type")]
    InvalidResourceType,

    #[error("Invalid pattern type")]
    InvalidPatternType,

    #[error("Invalid operation")]
    InvalidOperation,

    #[error("Invalid permission type")]
    InvalidPermissionType,

    #[error("ACL not found")]
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_binding(principal: &str, operation: AclOperation) -> AclBinding {
        AclBinding {
            resource_type: ResourceType::Topic,
            resource_name: "test-topic".to_string(),
            pattern_type: PatternType::Literal,
            principal: principal.to_string(),
            host: "*".to_string(),
            operation,
            permission_type: AclPermissionType::Allow,
        }
    }

    #[tokio::test]
    async fn test_acl_store_disabled_by_default() {
        let store = AclStore::new();
        assert!(!store.is_enabled());

        // When disabled, all operations should be allowed
        assert!(
            store
                .authorize(
                    "User:alice",
                    "127.0.0.1",
                    ResourceType::Topic,
                    "test-topic",
                    AclOperation::Read
                )
                .await
        );
    }

    #[tokio::test]
    async fn test_acl_create_and_describe() {
        std::env::set_var("CHRONIK_ACL_ENABLED", "true");
        let store = AclStore::new();

        let binding = create_test_binding("User:alice", AclOperation::Read);
        store.create_acl(binding.clone()).await.unwrap();

        let filter = AclFilter::new();
        let results = store.describe_acls(&filter).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].resource_name, "test-topic");
        assert_eq!(results[0].acls.len(), 1);

        std::env::remove_var("CHRONIK_ACL_ENABLED");
    }

    #[tokio::test]
    async fn test_acl_filter_matches() {
        let binding = create_test_binding("User:alice", AclOperation::Read);

        // Match all filter
        let filter = AclFilter::new();
        assert!(filter.matches(&binding));

        // Resource type filter
        let filter = AclFilter {
            resource_type: Some(ResourceType::Topic),
            ..Default::default()
        };
        assert!(filter.matches(&binding));

        let filter = AclFilter {
            resource_type: Some(ResourceType::Group),
            ..Default::default()
        };
        assert!(!filter.matches(&binding));

        // Principal filter
        let filter = AclFilter {
            principal: Some("User:alice".to_string()),
            ..Default::default()
        };
        assert!(filter.matches(&binding));

        let filter = AclFilter {
            principal: Some("User:bob".to_string()),
            ..Default::default()
        };
        assert!(!filter.matches(&binding));
    }

    #[tokio::test]
    async fn test_authorization_deny_takes_precedence() {
        std::env::set_var("CHRONIK_ACL_ENABLED", "true");
        std::env::set_var("CHRONIK_ACL_ALLOW_IF_NO_ACL", "false");
        let store = AclStore::new();

        // Add allow for User:alice
        store
            .create_acl(AclBinding {
                resource_type: ResourceType::Topic,
                resource_name: "secret-topic".to_string(),
                pattern_type: PatternType::Literal,
                principal: "User:*".to_string(),
                host: "*".to_string(),
                operation: AclOperation::Read,
                permission_type: AclPermissionType::Allow,
            })
            .await
            .unwrap();

        // Add explicit deny for User:mallory
        store
            .create_acl(AclBinding {
                resource_type: ResourceType::Topic,
                resource_name: "secret-topic".to_string(),
                pattern_type: PatternType::Literal,
                principal: "User:mallory".to_string(),
                host: "*".to_string(),
                operation: AclOperation::Read,
                permission_type: AclPermissionType::Deny,
            })
            .await
            .unwrap();

        // alice should be allowed
        assert!(
            store
                .authorize(
                    "User:alice",
                    "127.0.0.1",
                    ResourceType::Topic,
                    "secret-topic",
                    AclOperation::Read
                )
                .await
        );

        // mallory should be denied (explicit deny)
        assert!(
            !store
                .authorize(
                    "User:mallory",
                    "127.0.0.1",
                    ResourceType::Topic,
                    "secret-topic",
                    AclOperation::Read
                )
                .await
        );

        std::env::remove_var("CHRONIK_ACL_ENABLED");
        std::env::remove_var("CHRONIK_ACL_ALLOW_IF_NO_ACL");
    }
}
