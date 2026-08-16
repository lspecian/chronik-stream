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

            // A follower reaching this path after the leader completed the
            // rebalance reads working state that the NEXT rebalance clears. If
            // it lost that race the field is empty, so fall back to the record
            // kept for this generation — the assignment the leader actually
            // made. Without this the member is told it owns nothing, and since
            // the group is already Stable nothing ever corrects it: its
            // partitions simply go unconsumed.
            let member_assignment = if member_assignment.is_empty()
                && group.completed_generation == group.generation_id
            {
                match group.completed_assignments.get(member_id) {
                    Some(recorded) if !recorded.is_empty() => {
                        warn!(
                            group_id = %group.group_id,
                            member_id = %member_id,
                            generation = group.generation_id,
                            assignment = ?recorded,
                            "Member's working assignment was cleared by a concurrent rebalance; \
                             serving the assignment recorded for this generation"
                        );
                        recorded
                    }
                    _ => member_assignment,
                }
            } else {
                member_assignment
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

    /// A follower whose working assignment was cleared by a concurrent
    /// rebalance still gets the partitions the leader assigned it.
    ///
    /// Reproduces the 3-member case where consumer-2 was assigned partitions
    /// 2 and 3, its SyncGroup arrived after the leader had finished, and it was
    /// told it owned nothing — leaving those partitions unconsumed with the
    /// group Stable and nothing to trigger a correction.
    #[test]
    fn test_fallback_serves_recorded_assignment_when_member_state_was_cleared() {
        let mut members = HashMap::new();
        members.insert("member-2".to_string(), GroupMember {
            member_id: "member-2".to_string(),
            member_epoch: 1,
            assignment: HashMap::new(), // cleared by trigger_rebalance
            ..Default::default()
        });

        let mut recorded = HashMap::new();
        recorded.insert(
            "member-2".to_string(),
            HashMap::from([("t".to_string(), vec![2, 3])]),
        );

        let group = ConsumerGroup {
            group_id: "g".to_string(),
            generation_id: 3,
            members,
            assignment_strategy: AssignmentStrategy::Range,
            completed_assignments: recorded,
            completed_generation: 3,
            ..Default::default()
        };

        let response = SyncResponseBuilder::build_fallback_response(&group, "member-2");
        assert_eq!(response.error_code, 0);
        assert!(
            !response.assignment.is_empty(),
            "member was told it owns nothing despite an assignment recorded for generation 3"
        );
    }

    /// A record from an older generation must never be served.
    #[test]
    fn test_fallback_ignores_stale_generation_record() {
        let mut members = HashMap::new();
        members.insert("member-2".to_string(), GroupMember {
            member_id: "member-2".to_string(),
            assignment: HashMap::new(),
            ..Default::default()
        });

        let mut recorded = HashMap::new();
        recorded.insert(
            "member-2".to_string(),
            HashMap::from([("t".to_string(), vec![2, 3])]),
        );

        let group = ConsumerGroup {
            group_id: "g".to_string(),
            generation_id: 4,     // group has moved on
            members,
            assignment_strategy: AssignmentStrategy::Range,
            completed_assignments: recorded,
            completed_generation: 3, // record is from the previous generation
            ..Default::default()
        };

        let response = SyncResponseBuilder::build_fallback_response(&group, "member-2");
        assert_eq!(
            response.assignment,
            encode_assignment(&HashMap::new()),
            "a stale generation's assignment must not be served"
        );
    }

    #[test]
    fn test_build_error_response() {
        let response = SyncResponseBuilder::build_error_response(27, 5);
        assert_eq!(response.error_code, 27);
        assert_eq!(response.member_epoch, 5);
        assert!(response.assignment.is_empty());
    }
}
