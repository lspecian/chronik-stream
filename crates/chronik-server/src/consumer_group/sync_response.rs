//! SyncGroup response building
//!
//! Extracted from `sync_group()` to reduce complexity.
//! Handles fallback path and response construction.

use crate::consumer_group::{ConsumerGroup, AssignmentStrategy, SyncGroupResponse};
use crate::consumer_group::assignment::encode_assignment;
use tracing::{info, warn};

/// SyncGroup response builder
///
/// Handles response construction for various scenarios.
pub struct SyncResponseBuilder;

impl SyncResponseBuilder {
    /// Build response from member's existing assignment (fallback path)
    ///
    /// Complexity: < 15 (assignment lookup with incremental rebalance support)
    pub fn build_fallback_response(
        group: &ConsumerGroup,
        member_id: &str,
    ) -> SyncGroupResponse {
        let (assignment, epoch) = if let Some(member) = group.members.get(member_id) {
            let member_assignment = if group.assignment_strategy == AssignmentStrategy::CooperativeSticky {
                // For incremental rebalance, return target assignment if available
                member.target_assignment.as_ref()
                    .unwrap_or(&member.assignment)
            } else {
                &member.assignment
            };

            info!(
                group_id = %group.group_id,
                member_id = %member_id,
                assignment = ?member_assignment,
                state = ?group.state,
                "Returning assignment to member (fallback path)"
            );

            (encode_assignment(member_assignment), member.member_epoch)
        } else {
            warn!(
                group_id = %group.group_id,
                member_id = %member_id,
                "Member not found in group"
            );
            (vec![], 0)
        };

        SyncGroupResponse {
            error_code: 0,
            assignment,
            member_epoch: epoch,
        }
    }

    /// Build error response
    ///
    /// Complexity: < 5 (simple response construction)
    pub fn build_error_response(error_code: i16, member_epoch: i32) -> SyncGroupResponse {
        SyncGroupResponse {
            error_code,
            assignment: vec![],
            member_epoch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consumer_group::GroupMember;
    use std::collections::HashMap;

    #[test]
    fn test_build_fallback_response_member_exists() {
        let mut members = HashMap::new();
        members.insert("member-1".to_string(), GroupMember {
            member_id: "member-1".to_string(),
            member_epoch: 3,
            assignment: HashMap::new(),
            ..Default::default()
        });

        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            members,
            ..Default::default()
        };

        let response = SyncResponseBuilder::build_fallback_response(&group, "member-1");
        assert_eq!(response.error_code, 0);
        assert_eq!(response.member_epoch, 3);
    }

    #[test]
    fn test_build_fallback_response_member_not_found() {
        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            ..Default::default()
        };

        let response = SyncResponseBuilder::build_fallback_response(&group, "member-1");
        assert_eq!(response.error_code, 0);
        assert_eq!(response.member_epoch, 0);
        assert!(response.assignment.is_empty());
    }

    #[test]
    fn test_build_error_response() {
        let response = SyncResponseBuilder::build_error_response(27, 5);
        assert_eq!(response.error_code, 27);
        assert_eq!(response.member_epoch, 5);
        assert!(response.assignment.is_empty());
    }
}
