//! Consumer group coordinator for managing consumer groups with Raft consensus.
//! 
//! This module provides the controller-side logic for consumer group management,
//! ensuring that all group state changes are replicated through Raft for fault tolerance.

use crate::raft::{RaftHandle, Proposal};
use crate::raft::state_machine::{ConsumerGroup, GroupState, ConsumerMember, ControllerState};
use chronik_common::{Result, Error};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::info;

/// Group coordinator configuration
#[derive(Debug, Clone)]
pub struct GroupCoordinatorConfig {
    /// Session timeout in milliseconds
    pub session_timeout_ms: i32,
    /// Rebalance timeout in milliseconds 
    pub rebalance_timeout_ms: i32,
    /// Minimum session timeout in milliseconds
    pub min_session_timeout_ms: i32,
    /// Maximum session timeout in milliseconds
    pub max_session_timeout_ms: i32,
    /// Initial rebalance delay in milliseconds
    pub group_initial_rebalance_delay_ms: i32,
    /// Maximum group size
    pub group_max_size: i32,
    /// Maximum members per group
    pub max_members_per_group: usize,
}

impl Default for GroupCoordinatorConfig {
    fn default() -> Self {
        Self {
            session_timeout_ms: 30000,
            rebalance_timeout_ms: 60000,
            min_session_timeout_ms: 6000,
            max_session_timeout_ms: 300000,
            group_initial_rebalance_delay_ms: 3000,
            group_max_size: 1000,
            max_members_per_group: 1000,
        }
    }
}

/// Consumer group coordinator
pub struct GroupCoordinator {
    config: GroupCoordinatorConfig,
    raft_handle: RaftHandle,
    state: Arc<RwLock<ControllerState>>,
}

impl GroupCoordinator {
    /// Create a new group coordinator
    pub fn new(
        config: GroupCoordinatorConfig,
        raft_handle: RaftHandle,
        state: Arc<RwLock<ControllerState>>,
    ) -> Self {
        Self {
            config,
            raft_handle,
            state,
        }
    }
    
    /// Handle join group request from an ingest node
    pub async fn join_group(
        &self,
        group_id: String,
        member_id: Option<String>,
        client_id: String,
        client_host: String,
        session_timeout: i32,
        _rebalance_timeout: i32,
        protocol_type: String,
        protocols: Vec<(String, Vec<u8>)>,
    ) -> Result<JoinGroupResponse> {
        // Generate member ID if not provided
        let member_id = member_id.unwrap_or_else(|| {
            format!("{}-{}", client_id, uuid::Uuid::new_v4())
        });
        
        // Get current state
        let current_state = self.state.read().await;
        let mut group = current_state.consumer_groups.get(&group_id).cloned();
        drop(current_state);
        
        // Create group if it doesn't exist
        if group.is_none() {
            let new_group = ConsumerGroup {
                group_id: group_id.clone(),
                state: GroupState::Empty,
                protocol_type: protocol_type.clone(),
                generation_id: 0,
                leader: None,
                members: Vec::new(),
                topics: Vec::new(),
            };
            
            // Propose group creation through Raft
            self.raft_handle.propose(Proposal::CreateConsumerGroup(new_group.clone())).await?;
            group = Some(new_group);
        }
        
        let mut group = group.unwrap();
        
        // Validate protocol type
        if group.protocol_type != protocol_type {
            return Err(Error::Protocol("Protocol type mismatch".into()));
        }
        
        // Check member limit
        if group.members.len() >= self.config.max_members_per_group {
            return Err(Error::InvalidInput("Group has reached maximum member limit".into()));
        }
        
        // Add member
        let member = ConsumerMember {
            member_id: member_id.clone(),
            client_id,
            host: client_host,
            session_timeout_ms: session_timeout as u32,
        };
        
        // Remove existing member if present, then add the new one
        group.members.retain(|m| m.member_id != member_id);
        group.members.push(member);
        
        // Update group state
        if group.state == GroupState::Empty {
            group.state = GroupState::PreparingRebalance;
            group.generation_id += 1;
        } else if group.state == GroupState::Stable {
            group.state = GroupState::PreparingRebalance;
            group.generation_id += 1;
        }
        
        // Select leader if needed
        if group.leader.is_none() || !group.members.iter().any(|m| Some(&m.member_id) == group.leader.as_ref()) {
            // Choose member with lowest ID as leader
            if let Some(member) = group.members.iter().min_by_key(|m| &m.member_id) {
                group.leader = Some(member.member_id.clone());
            }
        }
        
        // Propose group update through Raft
        self.raft_handle.propose(Proposal::UpdateConsumerGroup(group.clone())).await?;
        
        // Build response
        let is_leader = Some(&member_id) == group.leader.as_ref();
        let response = JoinGroupResponse {
            error_code: 0,
            generation_id: group.generation_id,
            protocol: protocols.first().map(|(name, _)| name.clone()).unwrap_or_default(),
            leader_id: group.leader.clone().unwrap_or_default(),
            member_id: member_id.clone(),
            members: if is_leader {
                // Leader gets all member metadata
                group.members.iter().map(|member| {
                    (member.member_id.clone(), encode_member_metadata(member))
                }).collect()
            } else {
                HashMap::new()
            },
        };
        
        info!(
            group_id = %group_id,
            member_id = %member_id,
            generation = group.generation_id,
            is_leader = is_leader,
            "Member joined group"
        );
        
        Ok(response)
    }
    
    /// Handle sync group request
    pub async fn sync_group(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        assignments: Option<HashMap<String, Vec<u8>>>,
    ) -> Result<SyncGroupResponse> {
        let state = self.state.read().await;
        let group = state.consumer_groups.get(&group_id)
            .ok_or_else(|| Error::NotFound(format!("Group {} not found", group_id)))?;
        
        // Validate generation
        if group.generation_id != generation_id {
            return Ok(SyncGroupResponse {
                error_code: 27, // ILLEGAL_GENERATION
                assignment: vec![],
            });
        }
        
        // Validate member
        if !group.members.iter().any(|m| m.member_id == member_id) {
            return Ok(SyncGroupResponse {
                error_code: 25, // UNKNOWN_MEMBER_ID
                assignment: vec![],
            });
        }
        
        // If this is the leader, store assignments
        if Some(&member_id) == group.leader.as_ref() && assignments.is_some() {
            // In a real implementation, we would:
            // 1. Parse and validate assignments
            // 2. Store them in the state
            // 3. Make them available to members
            // For now, we'll just acknowledge
            
            if group.state == GroupState::PreparingRebalance {
                let mut updated_group = group.clone();
                updated_group.state = GroupState::Stable;
                drop(state);
                
                // Propose state update
                self.raft_handle.propose(Proposal::UpdateConsumerGroup(updated_group)).await?;
            }
        }
        
        // Return assignment for the member
        let assignment = assignments.as_ref()
            .and_then(|a| a.get(&member_id))
            .cloned()
            .unwrap_or_default();
        
        Ok(SyncGroupResponse {
            error_code: 0,
            assignment,
        })
    }
    
    /// Handle heartbeat request
    pub async fn heartbeat(
        &self,
        group_id: String,
        member_id: String,
        generation_id: i32,
    ) -> Result<HeartbeatResponse> {
        let state = self.state.read().await;
        let group = state.consumer_groups.get(&group_id)
            .ok_or_else(|| Error::NotFound(format!("Group {} not found", group_id)))?;
        
        // Validate generation
        if group.generation_id != generation_id {
            return Ok(HeartbeatResponse {
                error_code: 27, // ILLEGAL_GENERATION
            });
        }
        
        // Validate member
        if !group.members.iter().any(|m| m.member_id == member_id) {
            return Ok(HeartbeatResponse {
                error_code: 25, // UNKNOWN_MEMBER_ID
            });
        }
        
        // Check if rebalance is in progress
        let error_code = if group.state == GroupState::PreparingRebalance || 
                          group.state == GroupState::CompletingRebalance {
            25 // REBALANCE_IN_PROGRESS
        } else {
            0
        };
        
        Ok(HeartbeatResponse {
            error_code,
        })
    }
    
    /// Handle leave group request
    pub async fn leave_group(
        &self,
        group_id: String,
        member_id: String,
    ) -> Result<()> {
        let state = self.state.read().await;
        if let Some(group) = state.consumer_groups.get(&group_id) {
            let mut updated_group = group.clone();
            drop(state);
            
            // Remove member
            let member_exists = updated_group.members.iter().any(|m| m.member_id == member_id);
            if member_exists {
                updated_group.members.retain(|m| m.member_id != member_id);
                info!(
                    group_id = %group_id,
                    member_id = %member_id,
                    "Member leaving group"
                );
                
                // Update state
                if updated_group.members.is_empty() {
                    updated_group.state = GroupState::Empty;
                    updated_group.generation_id = 0;
                    updated_group.leader = None;
                } else {
                    // Trigger rebalance
                    if updated_group.state == GroupState::Stable {
                        updated_group.state = GroupState::PreparingRebalance;
                        updated_group.generation_id += 1;
                    }
                    
                    // Update leader if needed
                    if Some(&member_id) == updated_group.leader.as_ref() {
                        updated_group.leader = updated_group.members.iter()
                            .min_by_key(|m| &m.member_id)
                            .map(|m| m.member_id.clone());
                    }
                }
                
                // Propose update
                self.raft_handle.propose(Proposal::UpdateConsumerGroup(updated_group)).await?;
            }
        }
        
        Ok(())
    }
    
    /// Fetch consumer group metadata
    pub async fn describe_group(&self, group_id: String) -> Result<Option<ConsumerGroup>> {
        let state = self.state.read().await;
        Ok(state.consumer_groups.get(&group_id).cloned())
    }
    
    /// List all consumer groups
    pub async fn list_groups(&self) -> Result<Vec<String>> {
        let state = self.state.read().await;
        Ok(state.consumer_groups.keys().cloned().collect())
    }
    
    /// Handle offset commit request
    pub async fn commit_offsets(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        offsets: Vec<TopicPartitionOffset>,
    ) -> Result<Vec<PartitionOffsetResult>> {
        let state = self.state.read().await;
        let group = state.consumer_groups.get(&group_id)
            .ok_or_else(|| Error::NotFound(format!("Group {} not found", group_id)))?;
        
        // Validate generation
        if group.generation_id != generation_id {
            return Ok(offsets.into_iter().map(|o| PartitionOffsetResult {
                topic: o.topic,
                partition: o.partition,
                error_code: 27, // ILLEGAL_GENERATION
            }).collect());
        }
        
        // Validate member
        if !group.members.iter().any(|m| m.member_id == member_id) {
            return Ok(offsets.into_iter().map(|o| PartitionOffsetResult {
                topic: o.topic,
                partition: o.partition,
                error_code: 25, // UNKNOWN_MEMBER_ID
            }).collect());
        }
        
        drop(state);
        
        // Store offsets (in a real implementation, this would be persisted)
        // For now, we'll just acknowledge success
        let results = offsets.into_iter().map(|o| PartitionOffsetResult {
            topic: o.topic,
            partition: o.partition,
            error_code: 0,
        }).collect();
        
        Ok(results)
    }
    
    /// Fetch committed offsets
    pub async fn fetch_offsets(
        &self,
        _group_id: String,
        topics: Vec<String>,
    ) -> Result<Vec<TopicPartitionOffset>> {
        // In a real implementation, we would fetch from persistent storage
        // For now, return empty offsets
        let mut offsets = Vec::new();
        
        for topic in topics {
            // Would normally query metadata store for partition count and offsets
            // For demonstration, assume each topic has 1 partition starting at 0
            offsets.push(TopicPartitionOffset {
                topic,
                partition: 0,
                offset: 0,
                metadata: None,
            });
        }
        
        Ok(offsets)
    }
}

/// Join group response
#[derive(Debug)]
pub struct JoinGroupResponse {
    pub error_code: i16,
    pub generation_id: i32,
    pub protocol: String,
    pub leader_id: String,
    pub member_id: String,
    pub members: HashMap<String, Vec<u8>>,
}

/// Sync group response
#[derive(Debug)]
pub struct SyncGroupResponse {
    pub error_code: i16,
    pub assignment: Vec<u8>,
}

/// Heartbeat response
#[derive(Debug)]
pub struct HeartbeatResponse {
    pub error_code: i16,
}

/// Topic partition offset
#[derive(Debug)]
pub struct TopicPartitionOffset {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub metadata: Option<String>,
}

/// Partition offset result
#[derive(Debug)]
pub struct PartitionOffsetResult {
    pub topic: String,
    pub partition: i32,
    pub error_code: i16,
}

/// Parse subscriptions from protocol metadata
fn parse_subscriptions(_protocols: &[(String, Vec<u8>)]) -> Result<Vec<String>> {
    // In a real implementation, we would parse the protocol metadata
    // to extract subscription information. For now, return empty.
    Ok(vec![])
}

/// Encode member metadata for response
fn encode_member_metadata(_member: &ConsumerMember) -> Vec<u8> {
    // In a real implementation, we would encode the member metadata
    // according to the protocol specification
    vec![]
}

/// Get current timestamp in milliseconds
fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}