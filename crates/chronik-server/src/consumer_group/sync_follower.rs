//! Follower SyncGroup wait logic
//!
//! Extracted from `sync_group()` to reduce complexity.
//! Handles follower wait path with server-side assignment fallback (kafka-python workaround).

use crate::consumer_group::{ConsumerGroup, GroupState, SyncGroupResponse, GroupManager};
use crate::consumer_group::assignment::{encode_assignment};
use chronik_common::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tracing::{info, warn};

/// Follower sync coordinator
///
/// Handles follower wait logic with automatic server-side fallback.
pub struct FollowerSync;

impl FollowerSync {
    /// Check if should enter follower wait mode
    ///
    /// Complexity: < 5 (simple state check)
    pub fn should_wait_for_leader(group: &ConsumerGroup, is_leader: bool) -> bool {
        !is_leader && (group.state == GroupState::CompletingRebalance || group.state == GroupState::PreparingRebalance)
    }

    /// Spawn server-side assignment fallback task
    ///
    /// Complexity: < 25 (spawns background task with assignment computation)
    pub fn spawn_fallback_task(
        group: &ConsumerGroup,
        groups_clone: Arc<RwLock<std::collections::HashMap<String, ConsumerGroup>>>,
        metadata_store_clone: Arc<dyn chronik_common::metadata::MetadataStore>,
        assignor_clone: Arc<tokio::sync::Mutex<Box<dyn super::PartitionAssignor>>>,
        group_id: String,
        follower_count: usize,
        pending_count: usize,
    ) {
        if pending_count != follower_count || follower_count == 0 {
            return; // Not all followers waiting yet
        }

        warn!(
            group_id = %group_id,
            pending_count = pending_count,
            follower_count = follower_count,
            "All followers waiting for SyncGroup from leader - activating fallback timer"
        );

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;

            // Check if we still need server-side assignment
            let mut groups_write = groups_clone.write().await;
            if let Some(group) = groups_write.get_mut(&group_id) {
                // Only proceed if still in CompletingRebalance and followers still waiting
                if group.state == GroupState::CompletingRebalance {
                    let pending_sync_arc = group.pending_sync_futures.clone();
                    let pending_count = pending_sync_arc.lock().await.len();

                    if pending_count > 0 {
                        warn!(
                            group_id = %group_id,
                            pending_count = pending_count,
                            "Leader failed to send SyncGroup after 2s - computing assignments server-side (kafka-python workaround)"
                        );

                        // Compute assignments server-side
                        let manager = GroupManager {
                            groups: groups_clone.clone(),
                            metadata_store: metadata_store_clone.clone(),
                            assignor: assignor_clone.clone(),
                        };

                        match manager.compute_assignments(group).await {
                            Ok(computed_assignments) => {
                                Self::apply_fallback_assignments(
                                    group,
                                    computed_assignments,
                                    pending_sync_arc,
                                    &group_id,
                                ).await;
                            }
                            Err(e) => {
                                tracing::error!(
                                    group_id = %group_id,
                                    error = %e,
                                    "Failed to compute server-side assignments"
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    /// Apply fallback assignments and notify followers
    ///
    /// Complexity: < 20 (applies assignments and sends responses)
    async fn apply_fallback_assignments(
        group: &mut ConsumerGroup,
        computed_assignments: std::collections::HashMap<String, std::collections::HashMap<String, Vec<i32>>>,
        pending_sync_arc: Arc<tokio::sync::Mutex<std::collections::HashMap<String, oneshot::Sender<SyncGroupResponse>>>>,
        group_id: &str,
    ) {
        // Apply assignments to group members
        for (mid, assignment) in &computed_assignments {
            if let Some(member) = group.members.get_mut(mid) {
                member.assignment = assignment.clone();
            }
        }

        // Transition to Stable
        group.state = GroupState::Stable;
        group.rebalance_start_time = None;

        info!(
            group_id = %group_id,
            generation = group.generation_id,
            assignment_count = computed_assignments.len(),
            "Server-side assignment completed - transitioning to Stable"
        );

        // Send assignments to all waiting followers
        let mut pending_sync = pending_sync_arc.lock().await;
        for (mid, assignment) in &computed_assignments {
            if let Some(tx) = pending_sync.remove(mid) {
                let member_assignment = encode_assignment(assignment);
                let member_epoch = group.members.get(mid)
                    .map(|m| m.member_epoch)
                    .unwrap_or(0);

                let response = SyncGroupResponse {
                    error_code: 0,
                    assignment: member_assignment,
                    member_epoch,
                };

                let _ = tx.send(response);
            }
        }
    }

    /// Wait for leader assignment with timeout (using RX channel)
    ///
    /// Complexity: < 10 (oneshot channel wait with timeout)
    pub async fn wait_for_assignment_rx(
        rx: oneshot::Receiver<SyncGroupResponse>,
        rebalance_timeout: Duration,
        group_id: &str,
        member_id: &str,
    ) -> Result<SyncGroupResponse> {
        // Wait for leader to send assignment
        match tokio::time::timeout(rebalance_timeout, rx).await {
            Ok(Ok(response)) => {
                info!(
                    group_id = %group_id,
                    member_id = %member_id,
                    assignment_size = response.assignment.len(),
                    "Follower received assignment from leader"
                );
                Ok(response)
            }
            Ok(Err(_)) => {
                Err(chronik_common::Error::Internal("SyncGroup response channel closed".into()))
            }
            Err(_) => {
                Err(chronik_common::Error::Internal("SyncGroup timeout waiting for leader".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_wait_for_leader_true() {
        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            state: GroupState::CompletingRebalance,
            ..Default::default()
        };

        assert!(FollowerSync::should_wait_for_leader(&group, false));
    }

    #[test]
    fn test_should_wait_for_leader_false_when_leader() {
        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            state: GroupState::CompletingRebalance,
            ..Default::default()
        };

        assert!(!FollowerSync::should_wait_for_leader(&group, true));
    }

    #[test]
    fn test_should_wait_for_leader_false_when_stable() {
        let group = ConsumerGroup {
            group_id: "test-group".to_string(),
            state: GroupState::Stable,
            ..Default::default()
        };

        assert!(!FollowerSync::should_wait_for_leader(&group, false));
    }
}
