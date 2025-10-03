//! CreateAcls API types (API Key 30)

use bytes::{BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use chronik_common::{Error, Result};

/// ACL creation entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclCreation {
    pub resource_type: i8,
    pub resource_name: String,
    pub resource_pattern_type: i8,  // v1+ only
    pub principal: String,
    pub host: String,
    pub operation: i8,
    pub permission_type: i8,
}

/// CreateAcls request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAclsRequest {
    pub creations: Vec<AclCreation>,
}

/// ACL creation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclCreationResult {
    pub error_code: i16,
    pub error_message: Option<String>,
}

/// CreateAcls response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAclsResponse {
    pub throttle_time_ms: i32,
    pub results: Vec<AclCreationResult>,
}

/// Parse CreateAcls request
pub fn parse_create_acls_request(decoder: &mut crate::parser::Decoder, version: i16) -> Result<CreateAclsRequest> {
    let creation_count = decoder.read_i32()? as usize;
    let mut creations = Vec::with_capacity(creation_count);

    for _ in 0..creation_count {
        let resource_type = decoder.read_i8()?;
        let resource_name = decoder.read_string()?.unwrap_or_default();

        let resource_pattern_type = if version >= 1 {
            decoder.read_i8()?
        } else {
            3  // Default to Literal
        };

        let principal = decoder.read_string()?.unwrap_or_default();
        let host = decoder.read_string()?.unwrap_or_default();
        let operation = decoder.read_i8()?;
        let permission_type = decoder.read_i8()?;

        creations.push(AclCreation {
            resource_type,
            resource_name,
            resource_pattern_type,
            principal,
            host,
            operation,
            permission_type,
        });
    }

    Ok(CreateAclsRequest { creations })
}

/// Encode CreateAcls response
pub fn encode_create_acls_response(response: &CreateAclsResponse) -> BytesMut {
    let mut buf = BytesMut::new();

    // Throttle time
    buf.put_i32(response.throttle_time_ms);

    // Results array
    buf.put_i32(response.results.len() as i32);

    for result in &response.results {
        // Error code
        buf.put_i16(result.error_code);

        // Error message (nullable)
        if let Some(ref msg) = result.error_message {
            buf.put_i16(msg.len() as i16);
            buf.put_slice(msg.as_bytes());
        } else {
            buf.put_i16(-1);  // null
        }
    }

    buf
}