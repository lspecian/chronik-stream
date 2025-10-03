//! Consumer Group Coordinator
//!
//! This module implements complete consumer group coordination logic for Kafka compatibility,
//! including proper state transitions, rebalancing, and member management.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_common::metadata::traits::ConsumerGroupMetadata;
use chronik_common::Utc;
use tracing::{info, debug, warn, error};

/// Consumer group states as defined by Kafka protocol
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupState {
    /// Group has no members
    Empty,
    /// Group is preparing to rebalance
    PreparingRebalance,
    /// Group is completing rebalance with leader assignment
    CompletingRebalance,
    /// Group is stable with all members assigned
    Stable,
    /// Group is dead/removed
    Dead,
}

impl GroupState {
    pub fn as_str(&self) -> &str {
        match self {
            GroupState::Empty => "Empty",
            GroupState::PreparingRebalance => "PreparingRebalance",
            GroupState::CompletingRebalance => "CompletingRebalance",
            GroupState::Stable => "Stable",
            GroupState::Dead => "Dead",
        }
    }
}

/// Member metadata for consumer group
#[derive(Debug, Clone)]
pub struct MemberMetadata {
    pub member_id: String,
    pub group_instance_id: Option<String>,
    pub client_id: String,
    pub client_host: String,
    pub session_timeout_ms: i32,
    pub rebalance_timeout_ms: i32,
    pub protocol_type: String,
    pub protocols: Vec<ProtocolMetadata>,
    pub assignment: Option<Vec<u8>>,
    pub last_heartbeat: Instant,
    pub is_leaving: bool,
    pub is_new: bool,
}

/// Protocol metadata for a member
#[derive(Debug, Clone)]
pub struct ProtocolMetadata {
    pub name: String,
    pub metadata: Vec<u8>,
}

/// Consumer group information
#[derive(Debug, Clone)]
pub struct ConsumerGroupInfo {
    pub group_id: String,
    pub state: GroupState,
    pub generation_id: i32,
    pub protocol_type: Option<String>,
    pub protocol: Option<String>,
    pub leader: Option<String>,
    pub members: HashMap<String, MemberMetadata>,
    pub pending_members: HashSet<String>,
    pub rebalance_timeout: Option<Instant>,
    pub offsets: HashMap<TopicPartition, OffsetMetadata>,
}

/// Topic partition identifier
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

/// Offset metadata
#[derive(Debug, Clone)]
pub struct OffsetMetadata {
    pub offset: i64,
    pub leader_epoch: Option<i32>,
    pub metadata: Option<String>,
    pub commit_timestamp: i64,
    pub expire_timestamp: Option<i64>,
}

/// Join group result
#[derive(Debug)]
pub struct JoinGroupResult {
    pub member_id: String,
    pub generation_id: i32,
    pub protocol: Option<String>,
    pub leader_id: String,
    pub members: Vec<JoinGroupMember>,
    pub error_code: i16,
    pub throttle_time_ms: i32,
}

/// Join group member info
#[derive(Debug)]
pub struct JoinGroupMember {
    pub member_id: String,
    pub group_instance_id: Option<String>,
    pub metadata: Vec<u8>,
}

/// Sync group result
#[derive(Debug)]
pub struct SyncGroupResult {
    pub assignment: Vec<u8>,
    pub error_code: i16,
    pub throttle_time_ms: i32,
}

/// Heartbeat result
#[derive(Debug)]
pub struct HeartbeatResult {
    pub error_code: i16,
    pub throttle_time_ms: i32,
}

/// Consumer Group Coordinator manages all consumer groups
pub struct ConsumerGroupCoordinator {
    groups: Arc<RwLock<HashMap<String, ConsumerGroupInfo>>>,
    /// Session timeout for detecting dead members
    session_timeout: Duration,
    /// Rebalance timeout for completing rebalances
    max_rebalance_timeout: Duration,
    /// Optional metadata store for persistence and topic validation
    metadata_store: Option<Arc<dyn MetadataStore>>,
}

impl ConsumerGroupCoordinator {
    pub fn new() -> Self {
        Self {
            groups: Arc::new(RwLock::new(HashMap::new())),
            session_timeout: Duration::from_secs(30),
            max_rebalance_timeout: Duration::from_secs(60),
            metadata_store: None,
        }
    }

    /// Create a coordinator with metadata store for persistence
    pub fn with_metadata_store(metadata_store: Arc<dyn MetadataStore>) -> Self {
        Self {
            groups: Arc::new(RwLock::new(HashMap::new())),
            session_timeout: Duration::from_secs(30),
            max_rebalance_timeout: Duration::from_secs(60),
            metadata_store: Some(metadata_store),
        }
    }

    /// Handle member joining a group
    pub async fn join_group(
        &self,
        group_id: String,
        member_id: Option<String>,
        group_instance_id: Option<String>,
        client_id: String,
        client_host: String,
        protocol_type: String,
        protocols: Vec<ProtocolMetadata>,
        session_timeout_ms: i32,
        rebalance_timeout_ms: i32,
    ) -> Result<JoinGroupResult> {
        let mut groups = self.groups.write().await;

        // Generate member ID if new member
        let member_id = member_id.filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("{}-{}", client_id, uuid::Uuid::new_v4()));

        let is_new_group = !groups.contains_key(&group_id);
        let group = groups.entry(group_id.clone()).or_insert_with(|| {
            info!("Creating new consumer group: {}", group_id);
            ConsumerGroupInfo {
                group_id: group_id.clone(),
                state: GroupState::Empty,
                generation_id: 0,
                protocol_type: None,
                protocol: None,
                leader: None,
                members: HashMap::new(),
                pending_members: HashSet::new(),
                rebalance_timeout: None,
                offsets: HashMap::new(),
            }
        });

        // Check protocol type compatibility
        if let Some(ref existing_protocol) = group.protocol_type {
            if existing_protocol != &protocol_type {
                return Ok(JoinGroupResult {
                    member_id,
                    generation_id: -1,
                    protocol: None,
                    leader_id: String::new(),
                    members: Vec::new(),
                    error_code: 23, // INCONSISTENT_GROUP_PROTOCOL
                    throttle_time_ms: 0,
                });
            }
        }

        // Trigger rebalance if needed
        let needs_rebalance = match group.state {
            GroupState::Empty => true,
            GroupState::Stable => {
                // New member joining stable group
                !group.members.contains_key(&member_id) ||
                // Member protocol changed
                group.members.get(&member_id)
                    .map(|m| m.protocols.len() != protocols.len())
                    .unwrap_or(false)
            },
            GroupState::PreparingRebalance | GroupState::CompletingRebalance => false,
            GroupState::Dead => {
                return Ok(JoinGroupResult {
                    member_id,
                    generation_id: -1,
                    protocol: None,
                    leader_id: String::new(),
                    members: Vec::new(),
                    error_code: 25, // COORDINATOR_NOT_AVAILABLE
                    throttle_time_ms: 0,
                });
            }
        };

        if needs_rebalance {
            info!("Triggering rebalance for group {}", group_id);
            group.state = GroupState::PreparingRebalance;
            group.generation_id += 1;
            group.protocol_type = Some(protocol_type.clone());
            group.pending_members.clear();

            // Set rebalance timeout
            let timeout = Duration::from_millis(rebalance_timeout_ms as u64);
            group.rebalance_timeout = Some(Instant::now() + timeout);
        }

        // Add/update member
        let member = MemberMetadata {
            member_id: member_id.clone(),
            group_instance_id,
            client_id,
            client_host,
            session_timeout_ms,
            rebalance_timeout_ms,
            protocol_type,
            protocols: protocols.clone(),
            assignment: None,
            last_heartbeat: Instant::now(),
            is_leaving: false,
            is_new: !group.members.contains_key(&member_id),
        };

        group.members.insert(member_id.clone(), member);
        group.pending_members.insert(member_id.clone());

        // Check if all members have joined
        let all_joined = group.state == GroupState::PreparingRebalance &&
            group.pending_members.len() == group.members.len();

        if all_joined {
            // Transition to CompletingRebalance
            info!("All members joined, completing rebalance for group {}", group_id);
            group.state = GroupState::CompletingRebalance;

            // Select protocol (use first common protocol)
            let selected_protocol = self.select_protocol(&group.members);
            group.protocol = selected_protocol.clone();

            // Select leader (first member)
            let leader_id = group.members.keys().next().cloned().unwrap_or_default();
            group.leader = Some(leader_id.clone());

            // Prepare member list for leader
            let members = if member_id == leader_id {
                group.members.values().map(|m| JoinGroupMember {
                    member_id: m.member_id.clone(),
                    group_instance_id: m.group_instance_id.clone(),
                    metadata: m.protocols.first()
                        .map(|p| p.metadata.clone())
                        .unwrap_or_default(),
                }).collect()
            } else {
                Vec::new()
            };

            Ok(JoinGroupResult {
                member_id,
                generation_id: group.generation_id,
                protocol: selected_protocol,
                leader_id,
                members,
                error_code: 0,
                throttle_time_ms: 0,
            })
        } else if group.state == GroupState::PreparingRebalance {
            // Still waiting for members
            debug!("Waiting for more members to join group {}", group_id);
            Ok(JoinGroupResult {
                member_id,
                generation_id: -1,
                protocol: None,
                leader_id: String::new(),
                members: Vec::new(),
                error_code: 27, // REBALANCE_IN_PROGRESS
                throttle_time_ms: 0,
            })
        } else {
            // Group is stable, return current state
            let result = JoinGroupResult {
                member_id,
                generation_id: group.generation_id,
                protocol: group.protocol.clone(),
                leader_id: group.leader.clone().unwrap_or_default(),
                members: Vec::new(),
                error_code: 0,
                throttle_time_ms: 0,
            };

            // Persist the group state if it's new
            if is_new_group {
                let group_clone = group.clone();
                drop(groups); // Release the lock before async persist
                if let Err(e) = self.persist_group_state(&group_clone).await {
                    warn!("Failed to persist consumer group state: {}", e);
                }
            }

            Ok(result)
        }
    }

    /// Handle sync group request
    pub async fn sync_group(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        group_instance_id: Option<String>,
        assignments: HashMap<String, Vec<u8>>,
    ) -> Result<SyncGroupResult> {
        let mut groups = self.groups.write().await;

        let group = groups.get_mut(&group_id)
            .ok_or_else(|| Error::Protocol(format!("Group {} not found", group_id)))?;

        // Validate generation
        if generation_id != group.generation_id {
            return Ok(SyncGroupResult {
                assignment: Vec::new(),
                error_code: 22, // ILLEGAL_GENERATION
                throttle_time_ms: 0,
            });
        }

        // Validate member
        if !group.members.contains_key(&member_id) {
            return Ok(SyncGroupResult {
                assignment: Vec::new(),
                error_code: 25, // UNKNOWN_MEMBER_ID
                throttle_time_ms: 0,
            });
        }

        // If leader, distribute assignments
        if Some(&member_id) == group.leader.as_ref() && !assignments.is_empty() {
            debug!("Leader {} distributing assignments for group {}", member_id, group_id);
            for (mid, assignment) in assignments {
                if let Some(member) = group.members.get_mut(&mid) {
                    member.assignment = Some(assignment);
                }
            }

            // Transition to Stable
            group.state = GroupState::Stable;
            group.pending_members.clear();
            info!("Group {} is now stable with {} members", group_id, group.members.len());
        }

        // Get member's assignment
        let assignment = group.members.get(&member_id)
            .and_then(|m| m.assignment.clone())
            .unwrap_or_default();

        Ok(SyncGroupResult {
            assignment,
            error_code: 0,
            throttle_time_ms: 0,
        })
    }

    /// Handle heartbeat
    pub async fn heartbeat(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        group_instance_id: Option<String>,
    ) -> Result<HeartbeatResult> {
        let mut groups = self.groups.write().await;

        let group = groups.get_mut(&group_id)
            .ok_or_else(|| Error::Protocol(format!("Group {} not found", group_id)))?;

        // Check generation
        if generation_id != group.generation_id {
            return Ok(HeartbeatResult {
                error_code: 22, // ILLEGAL_GENERATION
                throttle_time_ms: 0,
            });
        }

        // Update heartbeat time
        if let Some(member) = group.members.get_mut(&member_id) {
            member.last_heartbeat = Instant::now();

            // Check if rebalance is needed
            if group.state == GroupState::PreparingRebalance {
                return Ok(HeartbeatResult {
                    error_code: 27, // REBALANCE_IN_PROGRESS
                    throttle_time_ms: 0,
                });
            }

            Ok(HeartbeatResult {
                error_code: 0,
                throttle_time_ms: 0,
            })
        } else {
            Ok(HeartbeatResult {
                error_code: 25, // UNKNOWN_MEMBER_ID
                throttle_time_ms: 0,
            })
        }
    }

    /// Handle leave group
    pub async fn leave_group(
        &self,
        group_id: String,
        members: Vec<(String, Option<String>)>,
    ) -> Result<Vec<i16>> {
        let mut groups = self.groups.write().await;

        let group = groups.get_mut(&group_id)
            .ok_or_else(|| Error::Protocol(format!("Group {} not found", group_id)))?;

        let mut error_codes = Vec::new();

        for (member_id, _group_instance_id) in members {
            if group.members.remove(&member_id).is_some() {
                info!("Member {} left group {}", member_id, group_id);
                error_codes.push(0);

                // Trigger rebalance if group was stable
                if group.state == GroupState::Stable && !group.members.is_empty() {
                    group.state = GroupState::PreparingRebalance;
                    group.generation_id += 1;
                }
            } else {
                error_codes.push(25); // UNKNOWN_MEMBER_ID
            }
        }

        // If no members left, reset group
        if group.members.is_empty() {
            info!("Group {} is now empty", group_id);
            group.state = GroupState::Empty;
            group.generation_id = 0;
            group.leader = None;
            group.protocol = None;
        }

        Ok(error_codes)
    }

    /// Commit offsets for a consumer group
    pub async fn commit_offsets(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        retention_time_ms: Option<i64>,
        offsets: Vec<(TopicPartition, i64, Option<i32>, Option<String>)>,
    ) -> Result<Vec<i16>> {
        let mut groups = self.groups.write().await;

        let group = groups.get_mut(&group_id)
            .ok_or_else(|| Error::Protocol(format!("Group {} not found", group_id)))?;

        // Validate generation for non-simple consumers
        if generation_id >= 0 && generation_id != group.generation_id {
            return Ok(vec![22; offsets.len()]); // ILLEGAL_GENERATION
        }

        // Validate member for non-simple consumers
        if !member_id.is_empty() && !group.members.contains_key(&member_id) {
            return Ok(vec![25; offsets.len()]); // UNKNOWN_MEMBER_ID
        }

        let mut error_codes = Vec::new();
        let commit_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        for (tp, offset, leader_epoch, metadata) in offsets {
            group.offsets.insert(tp, OffsetMetadata {
                offset,
                leader_epoch,
                metadata,
                commit_timestamp,
                expire_timestamp: retention_time_ms.map(|ms| commit_timestamp + ms),
            });
            error_codes.push(0);
        }

        debug!("Committed {} offsets for group {}", error_codes.len(), group_id);
        Ok(error_codes)
    }

    /// Fetch committed offsets for a consumer group
    pub async fn fetch_offsets(
        &self,
        group_id: String,
        topics: Option<Vec<TopicPartition>>,
        require_stable: bool,
    ) -> Result<Vec<(TopicPartition, Option<OffsetMetadata>, i16)>> {
        let groups = self.groups.read().await;

        let group = groups.get(&group_id)
            .ok_or_else(|| Error::Protocol(format!("Group {} not found", group_id)))?;

        let mut results = Vec::new();

        if let Some(topics) = topics {
            // Fetch specific topic partitions
            for tp in topics {
                if let Some(offset_metadata) = group.offsets.get(&tp) {
                    results.push((tp, Some(offset_metadata.clone()), 0));
                } else {
                    results.push((tp, None, 0));
                }
            }
        } else {
            // Fetch all offsets
            for (tp, offset_metadata) in &group.offsets {
                results.push((tp.clone(), Some(offset_metadata.clone()), 0));
            }
        }

        Ok(results)
    }

    /// Select common protocol for group members
    fn select_protocol(&self, members: &HashMap<String, MemberMetadata>) -> Option<String> {
        if members.is_empty() {
            return None;
        }

        // Find first common protocol across all members
        let first_member = members.values().next()?;
        for protocol in &first_member.protocols {
            let protocol_name = &protocol.name;
            let all_support = members.values().all(|m|
                m.protocols.iter().any(|p| &p.name == protocol_name)
            );
            if all_support {
                return Some(protocol_name.clone());
            }
        }

        None
    }

    /// Check for expired sessions and trigger rebalances
    pub async fn check_session_timeouts(&self) {
        let mut groups = self.groups.write().await;
        let now = Instant::now();

        for group in groups.values_mut() {
            let mut expired_members = Vec::new();

            for (member_id, member) in &group.members {
                let timeout = Duration::from_millis(member.session_timeout_ms as u64);
                if now.duration_since(member.last_heartbeat) > timeout {
                    expired_members.push(member_id.clone());
                }
            }

            if !expired_members.is_empty() {
                info!("Removing {} expired members from group {}",
                      expired_members.len(), group.group_id);

                for member_id in expired_members {
                    group.members.remove(&member_id);
                }

                // Trigger rebalance if needed
                if group.state == GroupState::Stable && !group.members.is_empty() {
                    group.state = GroupState::PreparingRebalance;
                    group.generation_id += 1;
                } else if group.members.is_empty() {
                    group.state = GroupState::Empty;
                    group.generation_id = 0;
                    group.leader = None;
                }
            }
        }
    }

    /// Get group state for monitoring
    pub async fn describe_group(&self, group_id: &str) -> Option<ConsumerGroupInfo> {
        let groups = self.groups.read().await;
        groups.get(group_id).cloned()
    }

    /// List all groups
    pub async fn list_groups(&self) -> Vec<(String, String)> {
        let groups = self.groups.read().await;
        groups.values()
            .map(|g| (g.group_id.clone(), g.state.as_str().to_string()))
            .collect()
    }

    /// Persist group state to metadata store
    async fn persist_group_state(&self, group: &ConsumerGroupInfo) -> Result<()> {
        if let Some(ref metadata_store) = self.metadata_store {
            let group_metadata = ConsumerGroupMetadata {
                group_id: group.group_id.clone(),
                generation_id: group.generation_id,
                protocol_type: group.protocol_type.clone().unwrap_or_else(|| "consumer".to_string()),
                protocol: group.protocol.clone().unwrap_or_else(|| "range".to_string()),
                leader_id: group.leader.clone(),
                leader: group.leader.clone().unwrap_or_default(),
                state: group.state.as_str().to_string(),
                members: group.members.iter().map(|(id, member)| {
                    chronik_common::metadata::traits::GroupMember {
                        member_id: id.clone(),
                        client_id: member.client_id.clone(),
                        client_host: member.client_host.clone(),
                        metadata: Vec::new(), // TODO: Serialize protocols data
                        assignment: member.assignment.clone().unwrap_or_default(),
                    }
                }).collect(),
                created_at: Utc::now(),
                updated_at: Utc::now()
            };

            if group.generation_id == 1 && group.state == GroupState::Empty {
                metadata_store.create_consumer_group(group_metadata).await
                    .map_err(|e| Error::Internal(format!("Failed to create group: {}", e)))?;
            } else {
                metadata_store.update_consumer_group(group_metadata).await
                    .map_err(|e| Error::Internal(format!("Failed to update group: {}", e)))?;
            }
        }
        Ok(())
    }

    /// Validate topics exist in metadata store
    async fn validate_topics(&self, topics: &[String]) -> Result<()> {
        if let Some(ref metadata_store) = self.metadata_store {
            let existing_topics = metadata_store.list_topics().await
                .map_err(|e| Error::Internal(format!("Failed to list topics: {}", e)))?;

            let existing_names: HashSet<String> = existing_topics.into_iter()
                .map(|t| t.name)
                .collect();

            for topic in topics {
                if !existing_names.contains(topic) {
                    return Err(Error::Protocol(format!("Topic {} does not exist", topic)));
                }
            }
        }
        Ok(())
    }

    /// Load group state from metadata store on startup
    pub async fn load_from_metadata_store(&self) -> Result<()> {
        if let Some(ref metadata_store) = self.metadata_store {
            let groups = metadata_store.list_consumer_groups().await
                .map_err(|e| Error::Internal(format!("Failed to list groups: {}", e)))?;

            let mut groups_map = self.groups.write().await;
            for group_meta in groups {
                let state = match group_meta.state.as_str() {
                    "Empty" => GroupState::Empty,
                    "PreparingRebalance" => GroupState::PreparingRebalance,
                    "CompletingRebalance" => GroupState::CompletingRebalance,
                    "Stable" => GroupState::Stable,
                    "Dead" => GroupState::Dead,
                    _ => GroupState::Empty,
                };

                let group_info = ConsumerGroupInfo {
                    group_id: group_meta.group_id.clone(),
                    state,
                    generation_id: group_meta.generation_id,
                    protocol_type: Some(group_meta.protocol_type),
                    protocol: Some(group_meta.protocol),
                    leader: group_meta.leader_id,
                    members: HashMap::new(), // Members will rejoin
                    pending_members: HashSet::new(),
                    rebalance_timeout: None,
                    offsets: HashMap::new(), // Offsets loaded separately
                };

                groups_map.insert(group_meta.group_id, group_info);
            }

            info!("Loaded {} consumer groups from metadata store", groups_map.len());
        }
        Ok(())
    }

    /// Persist offset commit to metadata store
    pub async fn commit_offset(
        &self,
        group_id: &str,
        topic: &str,
        partition: i32,
        offset: i64,
        metadata: Option<String>,
    ) -> Result<()> {
        if let Some(ref metadata_store) = self.metadata_store {
            use chronik_common::metadata::ConsumerOffset;

            let consumer_offset = ConsumerOffset {
                group_id: group_id.to_string(),
                topic: topic.to_string(),
                partition: partition as u32,
                offset,
                metadata: metadata.clone(),
                commit_timestamp: Utc::now(),
            };

            metadata_store.commit_offset(consumer_offset).await
                .map_err(|e| Error::Internal(format!("Failed to commit offset: {}", e)))?;
        }

        // Also update in-memory state
        let mut groups = self.groups.write().await;
        if let Some(group) = groups.get_mut(group_id) {
            let tp = TopicPartition {
                topic: topic.to_string(),
                partition,
            };

            group.offsets.insert(tp, OffsetMetadata {
                offset,
                leader_epoch: None,
                metadata,
                commit_timestamp: Utc::now().timestamp_millis(),
                expire_timestamp: None,
            });
        }

        Ok(())
    }

    /// Get committed offset from metadata store
    pub async fn get_offset(
        &self,
        group_id: &str,
        topic: &str,
        partition: i32,
    ) -> Option<i64> {
        // Try memory first
        let groups = self.groups.read().await;
        if let Some(group) = groups.get(group_id) {
            let tp = TopicPartition {
                topic: topic.to_string(),
                partition,
            };
            if let Some(offset_meta) = group.offsets.get(&tp) {
                return Some(offset_meta.offset);
            }
        }
        drop(groups);

        // Try metadata store
        if let Some(ref metadata_store) = self.metadata_store {
            if let Ok(Some(offset)) = metadata_store.get_consumer_offset(group_id, topic, partition as u32).await {
                return Some(offset.offset);
            }
        }

        None
    }
}

impl Default for ConsumerGroupCoordinator {
    fn default() -> Self {
        Self::new()
    }
}