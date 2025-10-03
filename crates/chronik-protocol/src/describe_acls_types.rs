//! DescribeAcls API types (API Key 29)

use bytes::{BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::hash::Hash;

/// ACL resource type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[repr(i8)]
pub enum ResourceType {
    Unknown = 0,
    Any = 1,
    Topic = 2,
    Group = 3,
    Cluster = 4,
    TransactionalId = 5,
    DelegationToken = 6,
}

impl ResourceType {
    pub fn from_i8(value: i8) -> Self {
        match value {
            1 => ResourceType::Any,
            2 => ResourceType::Topic,
            3 => ResourceType::Group,
            4 => ResourceType::Cluster,
            5 => ResourceType::TransactionalId,
            6 => ResourceType::DelegationToken,
            _ => ResourceType::Unknown,
        }
    }
}

/// ACL resource pattern type (v1+)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[repr(i8)]
pub enum PatternType {
    Unknown = 0,
    Any = 1,
    Match = 2,
    Literal = 3,
    Prefixed = 4,
}

impl PatternType {
    pub fn from_i8(value: i8) -> Self {
        match value {
            1 => PatternType::Any,
            2 => PatternType::Match,
            3 => PatternType::Literal,
            4 => PatternType::Prefixed,
            _ => PatternType::Unknown,
        }
    }
}

/// ACL operation
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[repr(i8)]
pub enum AclOperation {
    Unknown = 0,
    Any = 1,
    All = 2,
    Read = 3,
    Write = 4,
    Create = 5,
    Delete = 6,
    Alter = 7,
    Describe = 8,
    ClusterAction = 9,
    DescribeConfigs = 10,
    AlterConfigs = 11,
    IdempotentWrite = 12,
}

impl AclOperation {
    pub fn from_i8(value: i8) -> Self {
        match value {
            1 => AclOperation::Any,
            2 => AclOperation::All,
            3 => AclOperation::Read,
            4 => AclOperation::Write,
            5 => AclOperation::Create,
            6 => AclOperation::Delete,
            7 => AclOperation::Alter,
            8 => AclOperation::Describe,
            9 => AclOperation::ClusterAction,
            10 => AclOperation::DescribeConfigs,
            11 => AclOperation::AlterConfigs,
            12 => AclOperation::IdempotentWrite,
            _ => AclOperation::Unknown,
        }
    }
}

/// ACL permission type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[repr(i8)]
pub enum AclPermissionType {
    Unknown = 0,
    Any = 1,
    Deny = 2,
    Allow = 3,
}

impl AclPermissionType {
    pub fn from_i8(value: i8) -> Self {
        match value {
            1 => AclPermissionType::Any,
            2 => AclPermissionType::Deny,
            3 => AclPermissionType::Allow,
            _ => AclPermissionType::Unknown,
        }
    }
}

/// ACL filter for DescribeAcls request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclFilter {
    pub resource_type: ResourceType,
    pub resource_name: Option<String>,
    pub resource_pattern_type: PatternType,  // v1+ only
    pub principal: Option<String>,
    pub host: Option<String>,
    pub operation: AclOperation,
    pub permission_type: AclPermissionType,
}

/// ACL entry in the response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclEntry {
    pub principal: String,
    pub host: String,
    pub operation: AclOperation,
    pub permission_type: AclPermissionType,
}

/// Resource with ACLs in the response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceAcls {
    pub resource_type: ResourceType,
    pub resource_name: String,
    pub resource_pattern_type: PatternType,  // v1+ only
    pub acls: Vec<AclEntry>,
}

/// DescribeAcls response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeAclsResponse {
    pub throttle_time_ms: i32,
    pub error_code: i16,
    pub error_message: Option<String>,
    pub resources: Vec<ResourceAcls>,
}

/// Encode DescribeAcls response
pub fn encode_describe_acls_response(response: &DescribeAclsResponse, version: i16) -> BytesMut {
    let mut buf = BytesMut::new();

    // Throttle time
    buf.put_i32(response.throttle_time_ms);

    // Error code
    buf.put_i16(response.error_code);

    // Error message (nullable)
    if let Some(ref msg) = response.error_message {
        buf.put_i16(msg.len() as i16);
        buf.put_slice(msg.as_bytes());
    } else {
        buf.put_i16(-1);  // null
    }

    // Resources array
    buf.put_i32(response.resources.len() as i32);

    for resource in &response.resources {
        // Resource type
        buf.put_i8(resource.resource_type as i8);

        // Resource name
        buf.put_i16(resource.resource_name.len() as i16);
        buf.put_slice(resource.resource_name.as_bytes());

        // Resource pattern type (v1+)
        if version >= 1 {
            buf.put_i8(resource.resource_pattern_type as i8);
        }

        // ACLs array
        buf.put_i32(resource.acls.len() as i32);

        for acl in &resource.acls {
            // Principal
            buf.put_i16(acl.principal.len() as i16);
            buf.put_slice(acl.principal.as_bytes());

            // Host
            buf.put_i16(acl.host.len() as i16);
            buf.put_slice(acl.host.as_bytes());

            // Operation
            buf.put_i8(acl.operation as i8);

            // Permission type
            buf.put_i8(acl.permission_type as i8);
        }
    }

    buf
}