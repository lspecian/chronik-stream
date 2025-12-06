//! Leader SyncGroup assignment logic
//!
//! Extracted from `sync_group()` to reduce complexity.
//! Handles leader assignment computation, distribution, and persistence.

use crate::consumer_group::{ConsumerGroup, GroupState, SyncGroupResponse, GroupManager};
use crate::consumer_group::assignment::{decode_assignment, encode_assignment};
use chronik_common::Result;
use std::collections::HashMap;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

/// Leader assignment coordinator
///
/// Handles leader assignment computation and distribution to followers.
pub struct LeaderAssignment;

impl LeaderAssignment {
    /// Determine assignment source (client-provided or server-side)
    ///
    /// Complexity: < 15 (decision tree for assignment source)
    pub async fn resolve_assignments(
        manager: &GroupManager,
        group: &ConsumerGroup,
        member_id: &str,
        client_assignments: Option<Vec<(String, Vec<u8>)>>,
    ) -> Result<HashMap<String, HashMap<String, Vec<i32>>>> {
        if let Some(assignments) = client_assignments {
            // CRITICAL FIX: Check if assignments cover ALL members, not just "not empty"
            if !assignments.is_empty() && assignments.len() == group.members.len() {
                // Client provided COMPLETE assignments for all members - use them!
                info!(
                    group_id = %group.group_id,
                    member_id = %member_id,
                    assignment_count = assignments.len(),
                    member_count = group.members.len(),
                    "Using client-provided partition assignments from leader (complete)"
                );

                let mut parsed = HashMap::new();
                for (member_id, assignment_bytes) in assignments {
                    let assignment = decode_assignment(&assignment_bytes)?;
                    parsed.insert(member_id, assignment);
                }
                return Ok(parsed);
            } else {
                // Client sent incomplete/empty assignments - compute server-side
                warn!(
                    group_id = %group.group_id,
                    member_id = %member_id,
                    state = ?group.state,
                    assignment_count = assignments.len(),
                    member_count = group.members.len(),
                    "Leader sent incomplete assignments ({} assignments for {} members) - falling back to server-side computation",
                    assignments.len(),
                    group.members.len()
                );
            }
        } else {
            // No assignments provided - compute server-side
            info!(
                group_id = %group.group_id,
                member_id = %member_id,
                state = ?group.state,
                member_count = group.members.len(),
                "No client assignments provided - computing server-side"
            );
        }

        manager.compute_assignments(group).await
    }

    /// Apply assignments to group members
    ///
    /// Complexity: < 10 (simple assignment application)
    pub fn apply_assignments(
        group: &mut ConsumerGroup,
        computed_assignments: &HashMap<String, HashMap<String, Vec<i32>>>,
    ) {
        info!(
            group_id = %group.group_id,
            assignments = ?computed_assignments.keys().collect::<Vec<_>>(),
            "Leader computed assignments for all members"
        );

        // Apply assignments to all members
        for (mid, assignment) in computed_assignments {
            if let Some(member) = group.members.get_mut(mid) {
                member.assignment = assignment.clone();
            }
        }

        // Transition to Stable state
        group.state = GroupState::Stable;
        group.rebalance_start_time = None;

        info!(
            group_id = %group.group_id,
            generation = group.generation_id,
            "Rebalance completed - transitioning to Stable"
        );
    }

    /// Send assignments to waiting followers
    ///
    /// Complexity: < 20 (loop through followers and send responses)
    pub async fn distribute_assignments(
        group: &ConsumerGroup,
        computed_assignments: &HashMap<String, HashMap<String, Vec<i32>>>,
        leader_member_id: &str,
    ) {
        let pending_sync_arc = group.pending_sync_futures.clone();
        let mut pending_sync = pending_sync_arc.lock().await;

        for (mid, assignment) in computed_assignments {
            if mid != leader_member_id {  // Don't send to leader (we return directly below)
                if let Some(tx) = pending_sync.remove(mid) {
                    let member_assignment = encode_assignment(assignment);
                    let member_epoch = group.members.get(mid)
                        .map(|m| m.member_epoch)
                        .unwrap_or(0);

                    // Log assignment details for monitoring
                    for (topic, partitions) in assignment {
                        debug!(
                            group_id = %group.group_id,
                            member_id = %mid,
                            topic = %topic,
                            partitions = ?partitions,
                            "Sending partition assignment to follower"
                        );
                    }

                    let response = SyncGroupResponse {
                        error_code: 0,
                        assignment: member_assignment,
                        member_epoch,
                    };

                    if tx.send(response).is_err() {
                        warn!(
                            member_id = %mid,
                            "Failed to send SyncGroup response - receiver dropped"
                        );
                    } else {
                        info!(
                            member_id = %mid,
                            assignment_size = assignment.len(),
                            "Sent assignment to follower"
                        );
                    }
                }
            }
        }
    }

    /// Build leader's SyncGroup response
    ///
    /// Complexity: < 10 (simple response building)
    pub fn build_leader_response(
        group: &ConsumerGroup,
        computed_assignments: &HashMap<String, HashMap<String, Vec<i32>>>,
        member_id: &str,
    ) -> SyncGroupResponse {
        // Return leader's assignment
        let leader_assignment = computed_assignments.get(member_id)
            .cloned()
            .unwrap_or_default();
        let encoded_assignment = encode_assignment(&leader_assignment);
        let leader_epoch = group.members.get(member_id)
            .map(|m| m.member_epoch)
            .unwrap_or(0);

        // Log leader's assignment details for monitoring
        for (topic, partitions) in &leader_assignment {
            debug!(
                group_id = %group.group_id,
                member_id = %member_id,
                topic = %topic,
                partitions = ?partitions,
                "Leader's own partition assignment"
            );
        }

        info!(
            group_id = %group.group_id,
            member_id = %member_id,
            assignment_size = encoded_assignment.len(),
            "Leader returning own assignment"
        );

        SyncGroupResponse {
            error_code: 0,
            assignment: encoded_assignment,
            member_epoch: leader_epoch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_assignments_transitions_to_stable() {
        use crate::consumer_group::GroupMember;
        use std::collections::HashMap;

        let mut group = ConsumerGroup {
            group_id: "test-group".to_string(),
            generation_id: 5,
            state: GroupState::CompletingRebalance,
            members: {
                let mut members = HashMap::new();
                members.insert("member-1".to_string(), GroupMember::default());
                members
            },
            ..Default::default()
        };

        let mut assignments = HashMap::new();
        assignments.insert("member-1".to_string(), HashMap::new());

        LeaderAssignment::apply_assignments(&mut group, &assignments);

        assert_eq!(group.state, GroupState::Stable);
        assert!(group.rebalance_start_time.is_none());
    }
}
