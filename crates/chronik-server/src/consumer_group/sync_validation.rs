//! SyncGroup validation
//!
//! Extracted from `sync_group()` to reduce complexity.
//! Handles generation ID and member epoch validation.

use crate::consumer_group::{ConsumerGroup, SyncGroupResponse};
use chronik_common::Result;

/// SyncGroup validator
///
/// Handles validation logic for SyncGroup requests.
pub struct SyncValidator;

impl SyncValidator {
    /// Validate generation ID
    ///
    /// Complexity: < 5 (simple generation check)
    pub fn validate_generation(
        group: &ConsumerGroup,
        generation_id: i32,
    ) -> Option<SyncGroupResponse> {
        if group.generation_id != generation_id {
            return Some(SyncGroupResponse {
                error_code: 27, // ILLEGAL_GENERATION
                assignment: vec![],
                member_epoch: 0,
            });
        }
        None
    }

    /// Validate member epoch for incremental rebalance
    ///
    /// Complexity: < 10 (epoch check with member lookup)
    pub fn validate_member_epoch(
        group: &ConsumerGroup,
        member_id: &str,
        member_epoch: i32,
    ) -> Option<SyncGroupResponse> {
        if let Some(member) = group.members.get(member_id) {
            if member.member_epoch != member_epoch {
                return Some(SyncGroupResponse {
                    error_code: 78, // FENCED_MEMBER_EPOCH
                    assignment: vec![],
                    member_epoch: member.member_epoch,
                });
            }
        }
        None
    }

    /// Remove member from pending set
    ///
    /// Complexity: < 5 (simple set removal)
    pub fn remove_from_pending(group: &mut ConsumerGroup, member_id: &str) {
        group.pending_members.remove(member_id);
    }

    /// Check if member is the group leader
    ///
    /// Complexity: < 5 (simple comparison)
    pub fn is_leader(group: &ConsumerGroup, member_id: &str) -> bool {
        Some(member_id) == group.leader_id.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consumer_group::ConsumerGroup;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_validate_generation_success() {
        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            generation_id: 5,
            ..Default::default()
        };

        let result = SyncValidator::validate_generation(&group, 5);
        assert!(result.is_none()); // Valid generation
    }

    #[test]
    fn test_validate_generation_failure() {
        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            generation_id: 5,
            ..Default::default()
        };

        let result = SyncValidator::validate_generation(&group, 3);
        assert!(result.is_some());
        assert_eq!(result.unwrap().error_code, 27); // ILLEGAL_GENERATION
    }

    #[test]
    fn test_is_leader() {
        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            leader_id: Some("member-1".to_string()),
            ..Default::default()
        };

        assert!(SyncValidator::is_leader(&group, "member-1"));
        assert!(!SyncValidator::is_leader(&group, "member-2"));
    }
}
