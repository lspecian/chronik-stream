//! Access Control List (ACL) implementation.

use crate::{AuthError, AuthResult, UserPrincipal};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

/// Resource type
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Resource {
    Topic(String),
    ConsumerGroup(String),
    Cluster,
}

/// Operation type
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Operation {
    Read,
    Write,
    Create,
    Delete,
    Alter,
    Describe,
    ClusterAction,
}

/// Permission
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Permission {
    pub resource: Resource,
    pub operation: Operation,
    pub allow: bool,
}

/// Access Control List
pub struct Acl {
    /// User permissions
    user_permissions: RwLock<HashMap<String, HashSet<Permission>>>,
    
    /// Default permissions for authenticated users
    default_permissions: RwLock<HashSet<Permission>>,
}

impl Acl {
    /// Create new ACL
    pub fn new() -> Self {
        Self {
            user_permissions: RwLock::new(HashMap::new()),
            default_permissions: RwLock::new(HashSet::new()),
        }
    }
    
    /// Add permission for user
    pub fn add_user_permission(
        &self,
        username: &str,
        permission: Permission,
    ) -> AuthResult<()> {
        let mut perms = self.user_permissions.write()
            .map_err(|_| AuthError::Internal("Lock poisoned".to_string()))?;
        
        perms.entry(username.to_string())
            .or_insert_with(HashSet::new)
            .insert(permission);
        
        Ok(())
    }
    
    /// Add default permission
    pub fn add_default_permission(&self, permission: Permission) -> AuthResult<()> {
        let mut perms = self.default_permissions.write()
            .map_err(|_| AuthError::Internal("Lock poisoned".to_string()))?;
        
        perms.insert(permission);
        Ok(())
    }
    
    /// Check if user has permission
    pub fn check_permission(
        &self,
        principal: &UserPrincipal,
        resource: &Resource,
        operation: &Operation,
    ) -> AuthResult<()> {
        if !principal.authenticated {
            return Err(AuthError::PermissionDenied);
        }
        
        // Check user-specific permissions
        let user_perms = self.user_permissions.read()
            .map_err(|_| AuthError::Internal("Lock poisoned".to_string()))?;
        
        if let Some(perms) = user_perms.get(&principal.username) {
            for perm in perms {
                if perm.resource == *resource && perm.operation == *operation {
                    return if perm.allow {
                        Ok(())
                    } else {
                        Err(AuthError::PermissionDenied)
                    };
                }
            }
        }
        
        // Check default permissions
        let default_perms = self.default_permissions.read()
            .map_err(|_| AuthError::Internal("Lock poisoned".to_string()))?;
        
        for perm in default_perms.iter() {
            if perm.resource == *resource && perm.operation == *operation {
                return if perm.allow {
                    Ok(())
                } else {
                    Err(AuthError::PermissionDenied)
                };
            }
        }
        
        // No explicit permission found, deny by default
        Err(AuthError::PermissionDenied)
    }
    
    /// Get all permissions for a user
    pub fn get_user_permissions(&self, username: &str) -> AuthResult<Vec<Permission>> {
        let user_perms = self.user_permissions.read()
            .map_err(|_| AuthError::Internal("Lock poisoned".to_string()))?;
        
        let default_perms = self.default_permissions.read()
            .map_err(|_| AuthError::Internal("Lock poisoned".to_string()))?;
        
        let mut permissions = Vec::new();
        
        // Add user-specific permissions
        if let Some(perms) = user_perms.get(username) {
            permissions.extend(perms.iter().cloned());
        }
        
        // Add default permissions
        permissions.extend(default_perms.iter().cloned());
        
        Ok(permissions)
    }
    
    /// Initialize with default Kafka-compatible ACLs
    pub fn init_kafka_defaults(&self) -> AuthResult<()> {
        // Allow all authenticated users to read from any topic
        self.add_default_permission(Permission {
            resource: Resource::Topic("*".to_string()),
            operation: Operation::Read,
            allow: true,
        })?;
        
        // Allow all authenticated users to describe topics
        self.add_default_permission(Permission {
            resource: Resource::Topic("*".to_string()),
            operation: Operation::Describe,
            allow: true,
        })?;
        
        // Allow all authenticated users to describe the cluster
        self.add_default_permission(Permission {
            resource: Resource::Cluster,
            operation: Operation::Describe,
            allow: true,
        })?;
        
        Ok(())
    }
}

impl Default for Acl {
    fn default() -> Self {
        Self::new()
    }
}