// ACL Manager - Bridges Kafka ACL protocol with internal ACL system

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use chronik_common::{Result, Error};
use tracing::{info, debug, warn};

use crate::describe_acls_types::{
    ResourceType, PatternType, AclOperation, AclPermissionType,
    AclFilter, AclEntry, ResourceAcls, DescribeAclsResponse,
};
use crate::create_acls_types::{
    AclCreation, CreateAclsRequest, AclCreationResult, CreateAclsResponse,
};
use crate::delete_acls_types::{
    AclDeletionFilter, DeleteAclsRequest, MatchingAcl, FilterResult, DeleteAclsResponse,
};

/// Stored ACL binding
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AclBinding {
    pub resource_type: ResourceType,
    pub resource_name: String,
    pub resource_pattern_type: PatternType,
    pub principal: String,
    pub host: String,
    pub operation: AclOperation,
    pub permission_type: AclPermissionType,
}

/// ACL Manager for managing Kafka-compatible ACLs
pub struct AclManager {
    /// All ACL bindings
    acls: Arc<RwLock<HashSet<AclBinding>>>,

    /// Metadata store for persistence
    metadata_store: Option<Arc<dyn chronik_common::metadata::traits::MetadataStore>>,
}

impl AclManager {
    pub fn new(metadata_store: Option<Arc<dyn chronik_common::metadata::traits::MetadataStore>>) -> Self {
        let manager = Self {
            acls: Arc::new(RwLock::new(HashSet::new())),
            metadata_store,
        };

        // Initialize with default ACLs
        let default_manager = manager.clone();
        tokio::spawn(async move {
            if let Err(e) = default_manager.initialize_defaults().await {
                warn!("Failed to initialize default ACLs: {}", e);
            }
        });

        manager
    }

    /// Initialize default ACLs for development/testing
    async fn initialize_defaults(&self) -> Result<()> {
        info!("Initializing default ACLs");

        // Allow all users to read all topics
        self.add_acl(AclBinding {
            resource_type: ResourceType::Topic,
            resource_name: "*".to_string(),
            resource_pattern_type: PatternType::Literal,
            principal: "User:*".to_string(),
            host: "*".to_string(),
            operation: AclOperation::Read,
            permission_type: AclPermissionType::Allow,
        }).await?;

        // Allow all users to write to all topics
        self.add_acl(AclBinding {
            resource_type: ResourceType::Topic,
            resource_name: "*".to_string(),
            resource_pattern_type: PatternType::Literal,
            principal: "User:*".to_string(),
            host: "*".to_string(),
            operation: AclOperation::Write,
            permission_type: AclPermissionType::Allow,
        }).await?;

        // Allow all users to describe all topics
        self.add_acl(AclBinding {
            resource_type: ResourceType::Topic,
            resource_name: "*".to_string(),
            resource_pattern_type: PatternType::Literal,
            principal: "User:*".to_string(),
            host: "*".to_string(),
            operation: AclOperation::Describe,
            permission_type: AclPermissionType::Allow,
        }).await?;

        // Allow all users to manage consumer groups
        self.add_acl(AclBinding {
            resource_type: ResourceType::Group,
            resource_name: "*".to_string(),
            resource_pattern_type: PatternType::Literal,
            principal: "User:*".to_string(),
            host: "*".to_string(),
            operation: AclOperation::All,
            permission_type: AclPermissionType::Allow,
        }).await?;

        // Allow cluster operations
        self.add_acl(AclBinding {
            resource_type: ResourceType::Cluster,
            resource_name: "kafka-cluster".to_string(),
            resource_pattern_type: PatternType::Literal,
            principal: "User:*".to_string(),
            host: "*".to_string(),
            operation: AclOperation::ClusterAction,
            permission_type: AclPermissionType::Allow,
        }).await?;

        info!("Default ACLs initialized successfully");
        Ok(())
    }

    /// Add a new ACL
    async fn add_acl(&self, acl: AclBinding) -> Result<()> {
        let mut acls = self.acls.write().await;
        acls.insert(acl.clone());

        // Persist to metadata store
        if let Some(ref store) = self.metadata_store {
            debug!("Persisting ACL to metadata store: {:?}", acl);
            // TODO: Implement ACL persistence in metadata store
        }

        Ok(())
    }

    /// Create ACLs from request
    pub async fn create_acls(&self, request: CreateAclsRequest) -> Result<CreateAclsResponse> {
        info!("Creating {} ACLs", request.creations.len());

        let mut results = Vec::with_capacity(request.creations.len());

        for creation in request.creations {
            let result = self.create_single_acl(creation).await;
            results.push(result);
        }

        Ok(CreateAclsResponse {
            throttle_time_ms: 0,
            results,
        })
    }

    /// Create a single ACL
    async fn create_single_acl(&self, creation: AclCreation) -> AclCreationResult {
        let acl = AclBinding {
            resource_type: ResourceType::from_i8(creation.resource_type),
            resource_name: creation.resource_name,
            resource_pattern_type: PatternType::from_i8(creation.resource_pattern_type),
            principal: creation.principal,
            host: creation.host,
            operation: AclOperation::from_i8(creation.operation),
            permission_type: AclPermissionType::from_i8(creation.permission_type),
        };

        match self.add_acl(acl.clone()).await {
            Ok(_) => {
                info!("Created ACL: {:?}", acl);
                AclCreationResult {
                    error_code: 0,
                    error_message: None,
                }
            }
            Err(e) => {
                warn!("Failed to create ACL: {}", e);
                AclCreationResult {
                    error_code: 1,
                    error_message: Some(e.to_string()),
                }
            }
        }
    }

    /// Describe ACLs based on filter
    pub async fn describe_acls(&self, filter: AclFilter) -> Result<DescribeAclsResponse> {
        debug!("Describing ACLs with filter: {:?}", filter);

        let acls = self.acls.read().await;
        let mut resource_map: HashMap<(ResourceType, String, PatternType), Vec<AclEntry>> = HashMap::new();

        for acl in acls.iter() {
            if self.acl_matches_filter(acl, &filter) {
                let key = (acl.resource_type, acl.resource_name.clone(), acl.resource_pattern_type);
                let entry = AclEntry {
                    principal: acl.principal.clone(),
                    host: acl.host.clone(),
                    operation: acl.operation,
                    permission_type: acl.permission_type,
                };

                resource_map.entry(key).or_insert_with(Vec::new).push(entry);
            }
        }

        let resources: Vec<ResourceAcls> = resource_map
            .into_iter()
            .map(|((resource_type, resource_name, resource_pattern_type), acls)| {
                ResourceAcls {
                    resource_type,
                    resource_name,
                    resource_pattern_type,
                    acls,
                }
            })
            .collect();

        info!("Found {} resources with matching ACLs", resources.len());

        Ok(DescribeAclsResponse {
            throttle_time_ms: 0,
            error_code: 0,
            error_message: None,
            resources,
        })
    }

    /// Check if an ACL matches the filter
    fn acl_matches_filter(&self, acl: &AclBinding, filter: &AclFilter) -> bool {
        // Check resource type
        if filter.resource_type != ResourceType::Any && filter.resource_type != acl.resource_type {
            return false;
        }

        // Check resource name
        if let Some(ref name) = filter.resource_name {
            if !self.resource_name_matches(name, &acl.resource_name, filter.resource_pattern_type) {
                return false;
            }
        }

        // Check pattern type
        if filter.resource_pattern_type != PatternType::Any && filter.resource_pattern_type != acl.resource_pattern_type {
            return false;
        }

        // Check principal
        if let Some(ref principal) = filter.principal {
            if principal != &acl.principal && principal != "*" {
                return false;
            }
        }

        // Check host
        if let Some(ref host) = filter.host {
            if host != &acl.host && host != "*" {
                return false;
            }
        }

        // Check operation
        if filter.operation != AclOperation::Any && filter.operation != acl.operation {
            return false;
        }

        // Check permission type
        if filter.permission_type != AclPermissionType::Any && filter.permission_type != acl.permission_type {
            return false;
        }

        true
    }

    /// Check if resource name matches based on pattern type
    fn resource_name_matches(&self, filter_name: &str, acl_name: &str, pattern_type: PatternType) -> bool {
        match pattern_type {
            PatternType::Literal => filter_name == acl_name || filter_name == "*",
            PatternType::Prefixed => acl_name.starts_with(filter_name),
            PatternType::Any | PatternType::Match => true,
            _ => filter_name == acl_name,
        }
    }

    /// Delete ACLs based on filters
    pub async fn delete_acls(&self, request: DeleteAclsRequest) -> Result<DeleteAclsResponse> {
        info!("Deleting ACLs with {} filters", request.filters.len());

        let mut results = Vec::with_capacity(request.filters.len());

        for filter in request.filters {
            let result = self.delete_acls_by_filter(filter).await;
            results.push(result);
        }

        Ok(DeleteAclsResponse {
            throttle_time_ms: 0,
            filter_results: results,
        })
    }

    /// Delete ACLs matching a filter
    async fn delete_acls_by_filter(&self, filter: AclDeletionFilter) -> FilterResult {
        let filter_converted = AclFilter {
            resource_type: ResourceType::from_i8(filter.resource_type),
            resource_name: filter.resource_name,
            resource_pattern_type: PatternType::from_i8(filter.resource_pattern_type),
            principal: filter.principal,
            host: filter.host,
            operation: AclOperation::from_i8(filter.operation),
            permission_type: AclPermissionType::from_i8(filter.permission_type),
        };

        let mut acls = self.acls.write().await;
        let mut matching_acls = Vec::new();
        let mut to_remove = Vec::new();

        for acl in acls.iter() {
            if self.acl_matches_filter(acl, &filter_converted) {
                matching_acls.push(MatchingAcl {
                    error_code: 0,
                    error_message: None,
                    resource_type: acl.resource_type as i8,
                    resource_name: acl.resource_name.clone(),
                    resource_pattern_type: acl.resource_pattern_type as i8,
                    principal: acl.principal.clone(),
                    host: acl.host.clone(),
                    operation: acl.operation as i8,
                    permission_type: acl.permission_type as i8,
                });
                to_remove.push(acl.clone());
            }
        }

        // Remove matching ACLs
        for acl in to_remove {
            acls.remove(&acl);
            info!("Deleted ACL: {:?}", acl);

            // Persist deletion to metadata store
            if let Some(ref store) = self.metadata_store {
                debug!("Persisting ACL deletion to metadata store");
                // TODO: Implement ACL deletion persistence
            }
        }

        FilterResult {
            error_code: 0,
            error_message: None,
            matching_acls,
        }
    }

    /// Check if a principal has permission for an operation
    pub async fn check_permission(
        &self,
        principal: &str,
        host: &str,
        resource_type: ResourceType,
        resource_name: &str,
        operation: AclOperation,
    ) -> bool {
        let acls = self.acls.read().await;

        for acl in acls.iter() {
            if self.acl_applies(acl, principal, host, resource_type, resource_name, operation) {
                return acl.permission_type == AclPermissionType::Allow;
            }
        }

        // No matching ACL found, default deny
        false
    }

    /// Check if an ACL applies to the given context
    fn acl_applies(
        &self,
        acl: &AclBinding,
        principal: &str,
        host: &str,
        resource_type: ResourceType,
        resource_name: &str,
        operation: AclOperation,
    ) -> bool {
        // Check resource type
        if acl.resource_type != resource_type {
            return false;
        }

        // Check resource name
        if !self.resource_matches(&acl.resource_name, resource_name, acl.resource_pattern_type) {
            return false;
        }

        // Check principal
        if !self.principal_matches(&acl.principal, principal) {
            return false;
        }

        // Check host
        if !self.host_matches(&acl.host, host) {
            return false;
        }

        // Check operation
        if !self.operation_matches(acl.operation, operation) {
            return false;
        }

        true
    }

    fn resource_matches(&self, acl_resource: &str, resource: &str, pattern_type: PatternType) -> bool {
        match pattern_type {
            PatternType::Literal => acl_resource == "*" || acl_resource == resource,
            PatternType::Prefixed => resource.starts_with(acl_resource),
            _ => acl_resource == resource,
        }
    }

    fn principal_matches(&self, acl_principal: &str, principal: &str) -> bool {
        acl_principal == "User:*" || acl_principal == principal
    }

    fn host_matches(&self, acl_host: &str, host: &str) -> bool {
        acl_host == "*" || acl_host == host
    }

    fn operation_matches(&self, acl_op: AclOperation, op: AclOperation) -> bool {
        acl_op == AclOperation::All || acl_op == op
    }

    /// Load ACLs from metadata store on recovery
    pub async fn recover_from_metadata_store(&self) -> Result<()> {
        if let Some(ref store) = self.metadata_store {
            info!("Recovering ACLs from metadata store");
            // TODO: Implement ACL recovery from metadata store
        }
        Ok(())
    }
}

impl Clone for AclManager {
    fn clone(&self) -> Self {
        Self {
            acls: self.acls.clone(),
            metadata_store: self.metadata_store.clone(),
        }
    }
}