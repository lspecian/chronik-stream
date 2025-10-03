//! DeleteAcls API types (API Key 31)

use bytes::{BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use chronik_common::{Error, Result};

/// ACL filter for deletion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclDeletionFilter {
    pub resource_type: i8,
    pub resource_name: Option<String>,
    pub resource_pattern_type: i8,  // v1+ only
    pub principal: Option<String>,
    pub host: Option<String>,
    pub operation: i8,
    pub permission_type: i8,
}

/// DeleteAcls request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteAclsRequest {
    pub filters: Vec<AclDeletionFilter>,
}

/// Matching ACL for deletion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchingAcl {
    pub error_code: i16,
    pub error_message: Option<String>,
    pub resource_type: i8,
    pub resource_name: String,
    pub resource_pattern_type: i8,  // v1+ only
    pub principal: String,
    pub host: String,
    pub operation: i8,
    pub permission_type: i8,
}

/// Filter result for DeleteAcls
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterResult {
    pub error_code: i16,
    pub error_message: Option<String>,
    pub matching_acls: Vec<MatchingAcl>,
}

/// DeleteAcls response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteAclsResponse {
    pub throttle_time_ms: i32,
    pub filter_results: Vec<FilterResult>,
}

/// Parse DeleteAcls request
pub fn parse_delete_acls_request(decoder: &mut crate::parser::Decoder, version: i16) -> Result<DeleteAclsRequest> {
    let filter_count = decoder.read_i32()? as usize;
    let mut filters = Vec::with_capacity(filter_count);

    for _ in 0..filter_count {
        let resource_type = decoder.read_i8()?;
        let resource_name = decoder.read_string()?;

        let resource_pattern_type = if version >= 1 {
            decoder.read_i8()?
        } else {
            3  // Default to Literal
        };

        let principal = decoder.read_string()?;
        let host = decoder.read_string()?;
        let operation = decoder.read_i8()?;
        let permission_type = decoder.read_i8()?;

        filters.push(AclDeletionFilter {
            resource_type,
            resource_name,
            resource_pattern_type,
            principal,
            host,
            operation,
            permission_type,
        });
    }

    Ok(DeleteAclsRequest { filters })
}

/// Encode DeleteAcls response
pub fn encode_delete_acls_response(response: &DeleteAclsResponse, version: i16) -> BytesMut {
    let mut buf = BytesMut::new();

    // Throttle time
    buf.put_i32(response.throttle_time_ms);

    // Filter results array
    buf.put_i32(response.filter_results.len() as i32);

    for result in &response.filter_results {
        // Error code
        buf.put_i16(result.error_code);

        // Error message (nullable)
        if let Some(ref msg) = result.error_message {
            buf.put_i16(msg.len() as i16);
            buf.put_slice(msg.as_bytes());
        } else {
            buf.put_i16(-1);  // null
        }

        // Matching ACLs array
        buf.put_i32(result.matching_acls.len() as i32);

        for acl in &result.matching_acls {
            // Error code
            buf.put_i16(acl.error_code);

            // Error message (nullable)
            if let Some(ref msg) = acl.error_message {
                buf.put_i16(msg.len() as i16);
                buf.put_slice(msg.as_bytes());
            } else {
                buf.put_i16(-1);  // null
            }

            // Resource type
            buf.put_i8(acl.resource_type);

            // Resource name
            buf.put_i16(acl.resource_name.len() as i16);
            buf.put_slice(acl.resource_name.as_bytes());

            // Resource pattern type (v1+)
            if version >= 1 {
                buf.put_i8(acl.resource_pattern_type);
            }

            // Principal
            buf.put_i16(acl.principal.len() as i16);
            buf.put_slice(acl.principal.as_bytes());

            // Host
            buf.put_i16(acl.host.len() as i16);
            buf.put_slice(acl.host.as_bytes());

            // Operation
            buf.put_i8(acl.operation);

            // Permission type
            buf.put_i8(acl.permission_type);
        }
    }

    buf
}