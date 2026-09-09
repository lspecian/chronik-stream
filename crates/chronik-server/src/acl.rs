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

        Self::with_config(enabled, allow_if_no_acl, super_users)
    }

    /// Construct a store from explicit settings rather than the environment.
    ///
    /// `new()` reads process-wide environment variables, which makes it unusable
    /// from tests that run in parallel: one test setting `CHRONIK_ACL_ENABLED`
    /// changes what another observes. Configuration is passed explicitly here so
    /// behaviour is a function of arguments.
    pub fn with_config(
        enabled: bool,
        allow_if_no_acl: bool,
        super_users: Vec<String>,
    ) -> Self {
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

    /// Whether an operation with no matching ACL is allowed.
    pub fn allows_if_no_acl(&self) -> bool {
        self.allow_if_no_acl
    }

    /// Load bootstrap ACLs from `CHRONIK_ACL_BINDINGS`.
    ///
    /// ACLs have a bootstrapping problem: the store starts empty, so enabling
    /// authorization either permits everything (`allow_if_no_acl=true`) or locks
    /// out every client including the one that would create the first rule.
    /// Kafka solves it with a super-user list plus an external admin tool; this
    /// adds the declarative half, so a deployment can express its policy in
    /// configuration and start closed.
    ///
    /// Format: bindings separated by `;`, fields within a binding by `,`:
    ///
    /// ```text
    /// <principal>,<resource_type>,<resource_name>,<operation>,<permission>[,<host>]
    /// User:alice,Topic,orders,Read,Allow;User:bob,Group,analytics,Read,Allow
    /// ```
    ///
    /// `resource_name` may end with `*` for a prefixed pattern
    /// (`Topic,app-*` matches `app-orders`). `host` defaults to `*`.
    pub async fn load_bootstrap_acls(&self, spec: &str) -> usize {
        let mut loaded = 0;
        for entry in spec.split(';') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            match parse_acl_binding(entry) {
                Ok(binding) => {
                    let description = format!(
                        "{} {:?} {:?} on {:?} '{}'",
                        binding.principal,
                        binding.permission_type,
                        binding.operation,
                        binding.resource_type,
                        binding.resource_name
                    );
                    if let Err(e) = self.create_acl(binding).await {
                        warn!("Bootstrap ACL '{}' rejected: {}", entry, e);
                    } else {
                        info!("Bootstrap ACL: {}", description);
                        loaded += 1;
                    }
                }
                Err(e) => warn!("Ignoring malformed bootstrap ACL '{}': {}", entry, e),
            }
        }
        loaded
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

        // Check operation, honouring Kafka's implication rules.
        let operation_matches = entry.operation == AclOperation::All
            || entry.operation == AclOperation::Any
            || entry.operation == operation
            || implies(entry.operation, operation);

        principal_matches && host_matches && operation_matches
    }
}

/// Whether a granted operation implies a requested one.
///
/// Kafka's authorizer does not require every operation to be granted
/// explicitly: `Describe` is implied by `Read`, `Write`, `Delete` and `Alter`,
/// and `DescribeConfigs` is implied by `AlterConfigs`
/// (`AclOperation.describe()`/`describeConfigs()` upstream).
///
/// Without this, an ordinary policy silently fails. `User:alice,Group,g,Read`
/// let alice join the group but not `OffsetFetch` it, because OffsetFetch needs
/// Describe — so a consumer authorized to read was refused mid-session. Every
/// real policy would have needed redundant Describe grants that no operator
/// writes and `kafka-acls.sh` does not emit.
fn implies(granted: AclOperation, requested: AclOperation) -> bool {
    match requested {
        AclOperation::Describe => matches!(
            granted,
            AclOperation::Read
                | AclOperation::Write
                | AclOperation::Delete
                | AclOperation::Alter
        ),
        AclOperation::DescribeConfigs => granted == AclOperation::AlterConfigs,
        _ => false,
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

    /// The rule could not be written to the metadata log, so it was not applied.
    #[error("ACL storage error: {0}")]
    StorageError(String),

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
    use std::sync::Mutex;

    // Serialize ACL tests that mutate CHRONIK_ACL_ENABLED env var
    static ACL_ENV_MUTEX: Mutex<()> = Mutex::new(());

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
        let _lock = ACL_ENV_MUTEX.lock().unwrap();
        std::env::remove_var("CHRONIK_ACL_ENABLED");
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
        let _lock = ACL_ENV_MUTEX.lock().unwrap();
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
        let _lock = ACL_ENV_MUTEX.lock().unwrap();
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

/// Parse one `CHRONIK_ACL_BINDINGS` entry.
///
/// `<principal>,<resource_type>,<resource_name>,<operation>,<permission>[,<host>]`
fn parse_acl_binding(entry: &str) -> std::result::Result<AclBinding, String> {
    let fields: Vec<&str> = entry.split(',').map(|f| f.trim()).collect();
    if fields.len() < 5 {
        return Err(format!(
            "expected at least 5 comma-separated fields \
             (principal,resource_type,resource_name,operation,permission), got {}",
            fields.len()
        ));
    }

    let principal = fields[0].to_string();
    if principal.is_empty() {
        return Err("principal is empty".to_string());
    }
    // Kafka principals are "User:name". Accept a bare name and qualify it, so a
    // config that says `alice` behaves the way its author expects rather than
    // silently matching nothing.
    let principal = if principal.contains(':') || principal == "*" {
        principal
    } else {
        format!("User:{}", principal)
    };

    let resource_type = match fields[1].to_ascii_lowercase().as_str() {
        "topic" => ResourceType::Topic,
        "group" => ResourceType::Group,
        "cluster" => ResourceType::Cluster,
        "transactionalid" | "transactional_id" => ResourceType::TransactionalId,
        "delegationtoken" | "delegation_token" => ResourceType::DelegationToken,
        other => return Err(format!("unknown resource type '{}'", other)),
    };

    // A trailing '*' means a prefixed pattern, matching kafka-acls.sh usage.
    let raw_name = fields[2];
    let (resource_name, pattern_type) = if raw_name.len() > 1 && raw_name.ends_with('*') {
        (raw_name[..raw_name.len() - 1].to_string(), PatternType::Prefixed)
    } else {
        (raw_name.to_string(), PatternType::Literal)
    };

    let operation = match fields[3].to_ascii_lowercase().as_str() {
        "all" => AclOperation::All,
        "read" => AclOperation::Read,
        "write" => AclOperation::Write,
        "create" => AclOperation::Create,
        "delete" => AclOperation::Delete,
        "alter" => AclOperation::Alter,
        "describe" => AclOperation::Describe,
        "clusteraction" | "cluster_action" => AclOperation::ClusterAction,
        "describeconfigs" | "describe_configs" => AclOperation::DescribeConfigs,
        "alterconfigs" | "alter_configs" => AclOperation::AlterConfigs,
        "idempotentwrite" | "idempotent_write" => AclOperation::IdempotentWrite,
        other => return Err(format!("unknown operation '{}'", other)),
    };

    let permission_type = match fields[4].to_ascii_lowercase().as_str() {
        "allow" => AclPermissionType::Allow,
        "deny" => AclPermissionType::Deny,
        other => return Err(format!("unknown permission '{}' (expected Allow or Deny)", other)),
    };

    let host = fields.get(5).map(|h| h.to_string()).unwrap_or_else(|| "*".to_string());

    Ok(AclBinding {
        resource_type,
        resource_name,
        pattern_type,
        principal,
        host,
        operation,
        permission_type,
    })
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    #[test]
    fn parses_a_literal_binding() {
        let b = parse_acl_binding("User:alice,Topic,orders,Read,Allow").unwrap();
        assert_eq!(b.principal, "User:alice");
        assert_eq!(b.resource_type, ResourceType::Topic);
        assert_eq!(b.resource_name, "orders");
        assert_eq!(b.pattern_type, PatternType::Literal);
        assert_eq!(b.operation, AclOperation::Read);
        assert_eq!(b.permission_type, AclPermissionType::Allow);
        assert_eq!(b.host, "*");
    }

    /// A trailing '*' is a prefix pattern, not a literal topic called "app-*".
    #[test]
    fn parses_a_prefixed_binding() {
        let b = parse_acl_binding("User:alice,Topic,app-*,Write,Allow").unwrap();
        assert_eq!(b.resource_name, "app-");
        assert_eq!(b.pattern_type, PatternType::Prefixed);
    }

    /// A bare name is qualified rather than silently matching nothing.
    #[test]
    fn bare_principal_is_qualified() {
        let b = parse_acl_binding("alice,Topic,orders,Read,Allow").unwrap();
        assert_eq!(b.principal, "User:alice");
    }

    #[test]
    fn host_is_optional() {
        let b = parse_acl_binding("User:alice,Topic,orders,Read,Allow,10.0.0.1").unwrap();
        assert_eq!(b.host, "10.0.0.1");
    }

    #[test]
    fn malformed_entries_are_errors_not_silent_allows() {
        assert!(parse_acl_binding("User:alice,Topic,orders").is_err());
        assert!(parse_acl_binding("User:alice,Nonsense,orders,Read,Allow").is_err());
        assert!(parse_acl_binding("User:alice,Topic,orders,Fly,Allow").is_err());
        assert!(parse_acl_binding("User:alice,Topic,orders,Read,Maybe").is_err());
    }

    #[tokio::test]
    async fn bootstrap_acls_are_enforced() {
        let store = AclStore::with_config(true, false, Vec::new());
        let loaded = store
            .load_bootstrap_acls(
                "User:alice,Topic,orders,Read,Allow;User:bob,Topic,secrets,Read,Allow",
            )
            .await;
        assert_eq!(loaded, 2);

        assert!(
            store
                .authorize("User:alice", "1.2.3.4", ResourceType::Topic, "orders", AclOperation::Read)
                .await
        );
        // alice has no rule for 'secrets'
        assert!(
            !store
                .authorize("User:alice", "1.2.3.4", ResourceType::Topic, "secrets", AclOperation::Read)
                .await
        );
        // and no Write on 'orders'
        assert!(
            !store
                .authorize("User:alice", "1.2.3.4", ResourceType::Topic, "orders", AclOperation::Write)
                .await
        );
    }

    /// Malformed entries must not abort the whole policy, but must be counted
    /// out - a typo should lose one rule, not silently grant everything.
    #[tokio::test]
    async fn malformed_bootstrap_entries_are_skipped() {
        let store = AclStore::with_config(true, false, Vec::new());
        let loaded = store
            .load_bootstrap_acls("User:alice,Topic,orders,Read,Allow;garbage;;User:bob,Bad,x,Read,Allow")
            .await;
        assert_eq!(loaded, 1);
    }
}

// ============================================================================
// Persistence (Security Phase 3 follow-up)
//
// The in-memory index above is the hot path: an authorization check must not
// touch the metadata store. Durability is layered on top of it — writes go to
// the event log (which replicates and is replayed on recovery), and the index is
// rebuilt from that log at startup.
//
// Without this, `CreateAcls` answered success and the rule lived in one broker's
// memory until the next restart, invisible to every other node. That is the same
// "reports success while doing nothing" shape as the SASL handshake that
// verified credentials and discarded the answer.
// ============================================================================

use chronik_common::metadata::traits::{AclBindingRecord, MetadataStore};

impl AclBinding {
    /// Convert to the wire-valued form the metadata log stores.
    pub fn to_record(&self) -> AclBindingRecord {
        AclBindingRecord {
            resource_type: self.resource_type as i8,
            resource_name: self.resource_name.clone(),
            pattern_type: self.pattern_type as i8,
            principal: self.principal.clone(),
            host: self.host.clone(),
            operation: self.operation as i8,
            permission_type: self.permission_type as i8,
        }
    }

    /// Rebuild from the persisted form.
    pub fn from_record(record: &AclBindingRecord) -> Self {
        Self {
            resource_type: ResourceType::from_i8(record.resource_type),
            resource_name: record.resource_name.clone(),
            pattern_type: PatternType::from_i8(record.pattern_type),
            principal: record.principal.clone(),
            host: record.host.clone(),
            operation: AclOperation::from_i8(record.operation),
            permission_type: AclPermissionType::from_i8(record.permission_type),
        }
    }
}

impl AclStore {
    /// Load every persisted ACL into the in-memory index.
    ///
    /// Called once at startup, after the metadata store has recovered its log.
    /// Bootstrap bindings from `CHRONIK_ACL_BINDINGS` are applied separately and
    /// are additive: configuration and persisted rules coexist, so an operator
    /// can express a baseline policy in config and still manage rules at runtime.
    pub async fn load_persisted(&self, store: &dyn MetadataStore) -> usize {
        if !self.enabled {
            return 0;
        }

        let records = match store.list_acls().await {
            Ok(records) => records,
            Err(e) => {
                // Failing to read the policy must be loud. Continuing with an
                // empty index would either deny everything or - worse, with
                // allow_if_no_acl - permit everything.
                warn!("Failed to load persisted ACLs: {} - the in-memory policy may be incomplete", e);
                return 0;
            }
        };

        let mut loaded = 0;
        let mut acls = self.acls.write().await;
        for record in &records {
            let binding = AclBinding::from_record(record);
            let key = (binding.resource_type, binding.resource_name.clone());
            let entries = acls.entry(key).or_insert_with(Vec::new);
            if !entries.iter().any(|e| e == &binding) {
                entries.push(binding);
                loaded += 1;
            }
        }

        if loaded > 0 {
            info!("Loaded {} persisted ACL binding(s)", loaded);
        }
        loaded
    }

    /// Create an ACL and persist it.
    ///
    /// Persistence happens FIRST: if the event log write fails, the rule must not
    /// appear to exist in memory, or a broker would enforce a policy no other
    /// broker knows about and which would vanish on restart.
    pub async fn create_acl_durable(
        &self,
        binding: AclBinding,
        store: &dyn MetadataStore,
    ) -> Result<(), AclError> {
        if !self.enabled {
            debug!("ACL creation ignored (ACLs disabled)");
            return Ok(());
        }

        {
            let acls = self.acls.read().await;
            let key = (binding.resource_type, binding.resource_name.clone());
            if let Some(entries) = acls.get(&key) {
                if entries.iter().any(|e| e == &binding) {
                    return Err(AclError::DuplicateAcl);
                }
            }
        }

        store
            .create_acl(binding.to_record())
            .await
            .map_err(|e| AclError::StorageError(e.to_string()))?;

        self.create_acl(binding).await
    }

    /// Delete ACLs matching a filter, removing them from the log as well.
    pub async fn delete_acls_durable(
        &self,
        filter: &AclFilter,
        store: &dyn MetadataStore,
    ) -> Vec<AclBinding> {
        let removed = self.delete_acls(filter).await;
        for binding in &removed {
            if let Err(e) = store.delete_acl(binding.to_record()).await {
                // The in-memory removal already happened, so this broker has
                // stopped honouring the rule. Say so rather than let the two
                // views diverge silently.
                warn!(
                    "ACL removed from memory but NOT from the metadata log ({}): it will \
                     return on restart",
                    e
                );
            }
        }
        removed
    }
}

#[cfg(test)]
mod implication_tests {
    use super::*;

    fn read_on_group(principal: &str, group: &str) -> AclBinding {
        AclBinding {
            resource_type: ResourceType::Group,
            resource_name: group.to_string(),
            pattern_type: PatternType::Literal,
            principal: principal.to_string(),
            host: "*".to_string(),
            operation: AclOperation::Read,
            permission_type: AclPermissionType::Allow,
        }
    }

    /// The bug this fixes, exactly: `Group,g,Read` let a consumer join the group
    /// but not OffsetFetch it, because OffsetFetch requires Describe. Kafka
    /// implies Describe from Read, so every real policy depended on it.
    #[tokio::test]
    async fn read_implies_describe() {
        let store = AclStore::with_config(true, false, Vec::new());
        store.create_acl(read_on_group("User:alice", "g")).await.unwrap();

        assert!(
            store
                .authorize("User:alice", "1.2.3.4", ResourceType::Group, "g", AclOperation::Read)
                .await
        );
        assert!(
            store
                .authorize("User:alice", "1.2.3.4", ResourceType::Group, "g", AclOperation::Describe)
                .await,
            "Read must imply Describe, or a consumer authorized to read cannot fetch its offsets"
        );
    }

    /// Implication must not leak the other way: Describe does not grant Read.
    #[tokio::test]
    async fn describe_does_not_imply_read() {
        let store = AclStore::with_config(true, false, Vec::new());
        let mut binding = read_on_group("User:alice", "g");
        binding.operation = AclOperation::Describe;
        store.create_acl(binding).await.unwrap();

        assert!(
            store
                .authorize("User:alice", "1.2.3.4", ResourceType::Group, "g", AclOperation::Describe)
                .await
        );
        assert!(
            !store
                .authorize("User:alice", "1.2.3.4", ResourceType::Group, "g", AclOperation::Read)
                .await,
            "Describe must NOT grant Read - that would make a read-only grant a full one"
        );
    }

    /// Write, Delete and Alter imply Describe too; Read does not imply Write.
    #[tokio::test]
    async fn implication_table_matches_kafka() {
        for granted in [
            AclOperation::Read,
            AclOperation::Write,
            AclOperation::Delete,
            AclOperation::Alter,
        ] {
            assert!(implies(granted, AclOperation::Describe), "{:?} should imply Describe", granted);
        }
        assert!(implies(AclOperation::AlterConfigs, AclOperation::DescribeConfigs));

        assert!(!implies(AclOperation::Read, AclOperation::Write));
        assert!(!implies(AclOperation::Describe, AclOperation::Read));
        assert!(!implies(AclOperation::DescribeConfigs, AclOperation::AlterConfigs));
    }
}
