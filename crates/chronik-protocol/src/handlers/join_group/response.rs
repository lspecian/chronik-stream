//! JoinGroup response encoding
//!
//! Extracted from `encode_join_group_response()` to reduce complexity.
//! Handles version-specific response encoding for Kafka JoinGroup API v0-v9+.

use bytes::BytesMut;
use chronik_common::Result;
use crate::parser::Encoder;
use crate::join_group_types::{JoinGroupResponse, JoinGroupResponseMember};

/// JoinGroup response encoder
///
/// Encodes Kafka JoinGroup API responses with version-specific logic.
pub struct JoinGroupResponseEncoder;

impl JoinGroupResponseEncoder {
    /// Encode JoinGroup response with version-specific logic
    ///
    /// Complexity: < 25 (delegates to focused helper methods)
    pub fn encode(
        buf: &mut BytesMut,
        response: &JoinGroupResponse,
        version: i16,
    ) -> Result<()> {
        let start_len = buf.len();
        let mut encoder = Encoder::new(buf);

        // Check if this is a flexible/compact version (v9+)
        let flexible = version >= 9;

        tracing::debug!("JoinGroup v{} response encoding START: error={}, gen={}, protocol={:?}, leader={}, member_id={}, members={}",
            version, response.error_code, response.generation_id, response.protocol_name, response.leader, response.member_id, response.members.len());

        // Phase 1: Encode throttle time (v2+)
        Self::encode_throttle_time(&mut encoder, response.throttle_time_ms, version)?;

        // Phase 2: Encode basic fields (error, generation)
        Self::encode_basic_fields(&mut encoder, response, version)?;

        // Phase 3: Encode protocol fields (protocol_type, protocol_name)
        Self::encode_protocol_fields(&mut encoder, response, version, flexible)?;

        // Phase 4: Encode leader
        Self::encode_member_info(&mut encoder, response, version, flexible)?;

        // Phase 5: Encode skip_assignment (v9+)
        Self::encode_skip_assignment(&mut encoder, version)?;

        // Phase 6: Encode member_id and members array
        Self::encode_members(&mut encoder, &response.member_id, &response.members, version, flexible)?;

        // Phase 7: Encode tagged fields (flexible versions)
        if flexible {
            encoder.write_tagged_fields();
            tracing::debug!("  Tagged fields (response level): 0");
        }

        let response_bytes = &buf[start_len..];
        tracing::debug!("JoinGroup v{} response encoded {} bytes: {:02x?}", version, response_bytes.len(), response_bytes);

        Ok(())
    }

    /// Encode throttle time (v2+)
    ///
    /// Complexity: < 5 (simple version check)
    fn encode_throttle_time(
        encoder: &mut Encoder,
        throttle_time_ms: i32,
        version: i16,
    ) -> Result<()> {
        if version >= 2 {
            encoder.write_i32(throttle_time_ms);
            tracing::debug!("  [1] ThrottleTimeMs: {}", throttle_time_ms);
        }
        Ok(())
    }

    /// Encode basic fields (error_code, generation_id)
    ///
    /// Complexity: < 5 (simple field encoding)
    fn encode_basic_fields(
        encoder: &mut Encoder,
        response: &JoinGroupResponse,
        _version: i16,
    ) -> Result<()> {
        // Field 2: ErrorCode (v0+)
        encoder.write_i16(response.error_code);
        tracing::debug!("  [2] ErrorCode: {}", response.error_code);

        // Field 3: GenerationId (v0+)
        encoder.write_i32(response.generation_id);
        tracing::debug!("  [3] GenerationId: {}", response.generation_id);

        Ok(())
    }

    /// Encode protocol fields (protocol_type, protocol_name)
    ///
    /// Complexity: < 15 (version-specific string encoding)
    fn encode_protocol_fields(
        encoder: &mut Encoder,
        response: &JoinGroupResponse,
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        // Field 4: ProtocolType (v7+)
        if version >= 7 {
            if version >= 9 && flexible {
                // v9+ flexible format requires non-nullable strings
                let value = response.protocol_type.as_deref().unwrap_or("");
                encoder.write_compact_string(Some(value));
                tracing::debug!("  [4] ProtocolType (compact, non-null): {:?}", value);
            } else if flexible {
                encoder.write_compact_string(response.protocol_type.as_deref());
                tracing::debug!("  [4] ProtocolType (compact): {:?}", response.protocol_type);
            } else {
                encoder.write_string(response.protocol_type.as_deref());
                tracing::debug!("  [4] ProtocolType: {:?}", response.protocol_type);
            }
        }

        // Field 5: ProtocolName (v0+)
        if version >= 9 && flexible {
            // v9+ flexible format requires non-nullable strings
            let value = response.protocol_name.as_deref().unwrap_or("");
            encoder.write_compact_string(Some(value));
            tracing::debug!("  [5] ProtocolName (compact, non-null): {:?}", value);
        } else if flexible {
            encoder.write_compact_string(response.protocol_name.as_deref());
            tracing::debug!("  [5] ProtocolName (compact): {:?}", response.protocol_name);
        } else {
            encoder.write_string(response.protocol_name.as_deref());
            tracing::debug!("  [5] ProtocolName: {:?}", response.protocol_name);
        }

        Ok(())
    }

    /// Encode member info (leader, member_id)
    ///
    /// Complexity: < 10 (simple string encoding)
    fn encode_member_info(
        encoder: &mut Encoder,
        response: &JoinGroupResponse,
        _version: i16,
        flexible: bool,
    ) -> Result<()> {
        // Field 6: Leader (v0+)
        if flexible {
            encoder.write_compact_string(Some(&response.leader));
            tracing::debug!("  [6] Leader (compact): {}", response.leader);
        } else {
            encoder.write_string(Some(&response.leader));
            tracing::debug!("  [6] Leader: {}", response.leader);
        }

        Ok(())
    }

    /// Encode skip_assignment flag (v9+)
    ///
    /// Complexity: < 5 (simple version check)
    fn encode_skip_assignment(
        encoder: &mut Encoder,
        version: i16,
    ) -> Result<()> {
        if version >= 9 {
            encoder.write_bool(false);
            tracing::debug!("  [7] SkipAssignment: false");
        }
        Ok(())
    }

    /// Encode member_id and members array
    ///
    /// Complexity: < 15 (field + array encoding with member delegation)
    fn encode_members(
        encoder: &mut Encoder,
        member_id: &str,
        members: &[JoinGroupResponseMember],
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        // Field 8: MemberId (v0+) - encoded BEFORE members array
        if flexible {
            encoder.write_compact_string(Some(member_id));
            tracing::debug!("  [8] MemberId (compact): {}", member_id);
        } else {
            encoder.write_string(Some(member_id));
            tracing::debug!("  [8] MemberId: {}", member_id);
        }

        // Field 9: Members array (v0+)
        if flexible {
            encoder.write_compact_array_len(members.len());
            tracing::debug!("  [9] Members array (compact): {} members", members.len());
        } else {
            encoder.write_i32(members.len() as i32);
            tracing::debug!("  [9] Members array: {} members", members.len());
        }

        let before_members = encoder.position();
        tracing::warn!("  Before encoding members: buf_len={}, version={}, flexible={}, member_count={}",
            before_members, version, flexible, members.len());

        // Encode each member
        for (idx, member) in members.iter().enumerate() {
            Self::encode_member(encoder, member, idx, version, flexible)?;
        }

        let after_members = encoder.position();
        tracing::warn!("  After encoding members: buf_len={}, total_member_section_bytes={}",
            after_members, after_members - before_members);

        Ok(())
    }

    /// Encode individual member
    ///
    /// Complexity: < 15 (member field encoding)
    fn encode_member(
        encoder: &mut Encoder,
        member: &JoinGroupResponseMember,
        idx: usize,
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        let member_start = encoder.position();
        tracing::warn!("    Member[{}] START: buf_len={}, id={}, instance_id={:?}",
            idx, member_start, member.member_id, member.group_instance_id);

        // Member ID
        if flexible {
            encoder.write_compact_string(Some(&member.member_id));
        } else {
            encoder.write_string(Some(&member.member_id));
        }
        let after_member_id = encoder.position();
        tracing::warn!("      After member_id: buf_len={}, bytes_written={}",
            after_member_id, after_member_id - member_start);

        // Group Instance ID (v5+)
        if version >= 5 {
            if flexible {
                encoder.write_compact_string(member.group_instance_id.as_deref());
            } else {
                encoder.write_string(member.group_instance_id.as_deref());
            }
            let after_instance_id = encoder.position();
            tracing::warn!("      After group_instance_id: buf_len={}, bytes_written={}",
                after_instance_id, after_instance_id - after_member_id);
        }

        // Member metadata
        if flexible {
            encoder.write_compact_bytes(Some(&member.metadata));
            // Tagged fields at member level
            encoder.write_tagged_fields();
        } else {
            tracing::warn!("      Member[{}] metadata: len={}, first_16_bytes={:02x?}",
                idx, member.metadata.len(), &member.metadata[..member.metadata.len().min(16)]);
            encoder.write_bytes(Some(&member.metadata));
        }

        let member_end = encoder.position();
        let member_bytes = member_end - member_start;
        tracing::warn!("    Member[{}] END: buf_len={}, total_member_bytes={}",
            idx, member_end, member_bytes);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::join_group_types::JoinGroupResponse;

    #[test]
    fn test_encode_join_group_response_v0() {
        let response = JoinGroupResponse {
            throttle_time_ms: 0,
            error_code: 0,
            generation_id: 1,
            protocol_type: None,
            protocol_name: Some("consumer".to_string()),
            leader: "member-1".to_string(),
            member_id: "member-1".to_string(),
            members: vec![],
        };

        let mut buf = BytesMut::new();
        let result = JoinGroupResponseEncoder::encode(&mut buf, &response, 0);
        assert!(result.is_ok());
        assert!(buf.len() > 0);
    }

    #[test]
    fn test_encode_join_group_response_v9_flexible() {
        let response = JoinGroupResponse {
            throttle_time_ms: 100,
            error_code: 0,
            generation_id: 1,
            protocol_type: Some("consumer".to_string()),
            protocol_name: Some("range".to_string()),
            leader: "member-1".to_string(),
            member_id: "member-1".to_string(),
            members: vec![],
        };

        let mut buf = BytesMut::new();
        let result = JoinGroupResponseEncoder::encode(&mut buf, &response, 9);
        assert!(result.is_ok());
        assert!(buf.len() > 0);
    }

    #[test]
    fn test_encode_member() {
        use bytes::Bytes;

        let member = JoinGroupResponseMember {
            member_id: "test-member".to_string(),
            group_instance_id: None,
            metadata: Bytes::from(vec![1, 2, 3, 4]),
        };

        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        let result = JoinGroupResponseEncoder::encode_member(&mut encoder, &member, 0, 0, false);
        assert!(result.is_ok());
        assert!(buf.len() > 0);
    }
}
