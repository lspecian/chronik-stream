//! Consumer group management with KIP-848 incremental cooperative rebalancing support.

use async_trait::async_trait;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::{MetadataStore, ConsumerGroupMetadata, ConsumerOffset};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Mutex, oneshot};
use tracing::{debug, info, warn, error};

// Phase 2.3: Consumer group refactoring modules
mod assignment;
mod sync_validation;
mod sync_follower;
mod sync_leader;
mod sync_response;

// Re-export for internal use
pub use assignment::{encode_assignment, decode_assignment};
pub use sync_validation::SyncValidator;
pub use sync_follower::FollowerSync;
pub use sync_leader::LeaderAssignment;
pub use sync_response::SyncResponseBuilder;

/// Consumer group state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupState {
    Empty,
    Stable,
    PreparingRebalance,
    CompletingRebalance,
    Dead,
}

impl GroupState {
    /// Convert to string representation for storage
    pub fn as_str(&self) -> &'static str {
        match self {
            GroupState::Empty => "Empty",
            GroupState::Stable => "Stable",
            GroupState::PreparingRebalance => "PreparingRebalance",
            GroupState::CompletingRebalance => "CompletingRebalance",
            GroupState::Dead => "Dead",
        }
    }
    
    /// Parse from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Empty" => Some(GroupState::Empty),
            "Stable" => Some(GroupState::Stable),
            "PreparingRebalance" => Some(GroupState::PreparingRebalance),
            "CompletingRebalance" => Some(GroupState::CompletingRebalance),
            "Dead" => Some(GroupState::Dead),
            _ => None,
        }
    }
}

/// Consumer group member
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub member_id: String,
    pub client_id: String,
    pub client_host: String,
    pub session_timeout: Duration,
    pub rebalance_timeout: Duration,
    pub subscription: Vec<String>,
    pub assignment: HashMap<String, Vec<i32>>, // topic -> partitions
    #[serde(skip, default = "Instant::now")]
    pub last_heartbeat: Instant,
    
    // KIP-848 fields for incremental cooperative rebalancing
    pub owned_partitions: HashMap<String, Vec<i32>>, // Currently owned partitions
    pub target_assignment: Option<HashMap<String, Vec<i32>>>, // Target assignment during rebalance
    pub member_epoch: i32, // Member epoch for incremental rebalance
    pub is_leaving: bool, // Flag for graceful leave
    pub protocols: Vec<(String, Vec<u8>)>, // Supported protocols with metadata
    pub user_data: Option<Vec<u8>>, // Application-specific data
}

impl GroupMember {
    /// Create a new group member
    pub fn new(
        member_id: String,
        client_id: String,
        client_host: String,
        session_timeout: Duration,
        rebalance_timeout: Duration,
        subscription: Vec<String>,
        protocols: Vec<(String, Vec<u8>)>,
    ) -> Self {
        Self {
            member_id,
            client_id,
            client_host,
            session_timeout,
            rebalance_timeout,
            subscription,
            assignment: HashMap::new(),
            last_heartbeat: Instant::now(),
            owned_partitions: HashMap::new(),
            target_assignment: None,
            member_epoch: 0,
            is_leaving: false,
            protocols,
            user_data: None,
        }
    }
    
    /// Check if member has timed out
    pub fn is_expired(&self) -> bool {
        self.last_heartbeat.elapsed() > self.session_timeout
    }
    
    /// Update heartbeat timestamp
    pub fn update_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
    }
}

/// Partition assignment strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssignmentStrategy {
    Range,
    RoundRobin,
    Sticky,
    CooperativeSticky, // KIP-848
}

impl AssignmentStrategy {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "range" => Some(AssignmentStrategy::Range),
            "roundrobin" => Some(AssignmentStrategy::RoundRobin),
            "sticky" => Some(AssignmentStrategy::Sticky),
            "cooperative-sticky" => Some(AssignmentStrategy::CooperativeSticky),
            _ => None,
        }
    }
}

/// Consumer group with KIP-848 support
#[derive(Debug, Serialize, Deserialize)]
pub struct ConsumerGroup {
    pub group_id: String,
    pub state: GroupState,
    pub generation_id: i32,
    pub protocol_type: String,
    pub protocol: Option<String>,
    pub leader_id: Option<String>,
    pub members: HashMap<String, GroupMember>,

    // KIP-848 incremental rebalance fields
    pub group_epoch: i32,
    pub assignment_strategy: AssignmentStrategy,
    pub pending_members: HashSet<String>, // Members pending assignment
    #[serde(skip, default)]
    pub rebalance_start_time: Option<Instant>,
    #[serde(skip, default)]
    pub expected_members_count: usize, // Expected members to rejoin during rebalance
    #[serde(skip, default)]
    pub previous_member_count: usize, // Member count before rebalance (to detect scale-up/down)
    pub static_members: HashMap<String, String>, // static_member_id -> member_id mapping

    // Metadata persistence
    #[serde(skip, default)]
    pub last_persisted: Option<Instant>,

    // Async waiting mechanism for JoinGroup responses
    #[serde(skip)]
    pub pending_join_futures: Arc<Mutex<HashMap<String, oneshot::Sender<JoinGroupResponse>>>>,

    // Async waiting mechanism for SyncGroup responses (followers wait for leader to compute assignments)
    #[serde(skip)]
    pub pending_sync_futures: Arc<Mutex<HashMap<String, oneshot::Sender<SyncGroupResponse>>>>,
}

impl ConsumerGroup {
    /// Create a new consumer group
    pub fn new(group_id: String, protocol_type: String) -> Self {
        Self {
            group_id,
            state: GroupState::Empty,
            generation_id: 0,
            protocol_type,
            protocol: None,
            leader_id: None,
            members: HashMap::new(),
            group_epoch: 0,
            assignment_strategy: AssignmentStrategy::CooperativeSticky,
            pending_members: HashSet::new(),
            rebalance_start_time: None,
            expected_members_count: 0,
            previous_member_count: 0,
            static_members: HashMap::new(),
            last_persisted: None,
            pending_join_futures: Arc::new(Mutex::new(HashMap::new())),
            pending_sync_futures: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    
    /// Add a member to the group
    pub fn add_member(&mut self, member: GroupMember) {
        let member_id = member.member_id.clone();
        self.members.insert(member_id.clone(), member);
        self.pending_members.insert(member_id);
        
        if self.state == GroupState::Empty {
            self.state = GroupState::PreparingRebalance;
            self.rebalance_start_time = Some(Instant::now());
        }
    }
    
    /// Remove a member from the group
    pub fn remove_member(&mut self, member_id: &str) -> Option<GroupMember> {
        self.pending_members.remove(member_id);
        
        // Handle static member removal
        if let Some(static_id) = self.static_members.iter()
            .find(|(_, id)| id.as_str() == member_id)
            .map(|(k, _)| k.clone()) {
            self.static_members.remove(&static_id);
        }
        
        let member = self.members.remove(member_id);
        
        if self.members.is_empty() {
            self.state = GroupState::Empty;
            self.generation_id = 0;
            self.group_epoch = 0;
            self.leader_id = None;
            self.rebalance_start_time = None;
        } else if member.is_some() {
            // Trigger rebalance if a member leaves
            self.trigger_rebalance();
        }
        
        member
    }
    
    /// Update member heartbeat
    pub fn heartbeat(&mut self, member_id: &str) -> Result<()> {
        match self.members.get_mut(member_id) {
            Some(member) => {
                member.update_heartbeat();
                Ok(())
            }
            None => Err(Error::InvalidInput(format!("Unknown member: {}", member_id))),
        }
    }
    
    /// Check for expired members
    pub fn check_expired_members(&mut self) -> Vec<String> {
        let expired: Vec<String> = self.members
            .iter()
            .filter(|(_, member)| member.is_expired())
            .map(|(id, _)| id.clone())
            .collect();
        
        for member_id in &expired {
            info!(
                group_id = %self.group_id,
                member_id = %member_id,
                "Member expired due to session timeout"
            );
            self.remove_member(member_id);
        }
        
        expired
    }
    
    /// Trigger a rebalance
    pub fn trigger_rebalance(&mut self) {
        self.trigger_rebalance_internal(false);
    }

    pub fn trigger_rebalance_with_redistribution(&mut self) {
        self.trigger_rebalance_internal(true);
    }

    fn trigger_rebalance_internal(&mut self, force_redistribution: bool) {
        if self.state == GroupState::Stable || self.state == GroupState::CompletingRebalance {
            let current_member_count = self.members.len();
            let is_scale_up = current_member_count > self.previous_member_count;

            info!(
                group_id = %self.group_id,
                current_members = current_member_count,
                previous_members = self.previous_member_count,
                is_scale_up = is_scale_up,
                force_redistribution = force_redistribution,
                "Triggering rebalance"
            );

            self.state = GroupState::PreparingRebalance;
            self.generation_id += 1;
            self.group_epoch += 1;
            self.rebalance_start_time = Some(Instant::now());

            // Track how many members we expect to rejoin
            self.expected_members_count = current_member_count;
            self.previous_member_count = current_member_count;

            // For incremental rebalance, save current assignments as owned partitions
            // UNLESS force_redistribution is true OR this is a scale-up (new member joining)
            //
            // Key insight: On scale-up, we want to clear owned_partitions so that the
            // CooperativeStickyAssignor can redistribute partitions to include new members.
            // On scale-down or same-size rebalance, we preserve assignments for stickiness.
            if self.assignment_strategy == AssignmentStrategy::CooperativeSticky && !force_redistribution && !is_scale_up {
                for member in self.members.values_mut() {
                    member.owned_partitions = member.assignment.clone();
                    member.target_assignment = None;
                }
            } else {
                // For eager rebalance OR forced redistribution OR scale-up, clear assignments
                for member in self.members.values_mut() {
                    member.assignment.clear();
                    member.owned_partitions.clear();
                }
            }
        }
    }
    
    /// Check if rebalance is needed
    pub fn needs_rebalance(&self) -> bool {
        !self.pending_members.is_empty() || 
        self.state == GroupState::PreparingRebalance ||
        self.state == GroupState::CompletingRebalance
    }
    
    /// Complete the rebalance with incremental support
    pub fn complete_rebalance(&mut self, assignments: HashMap<String, HashMap<String, Vec<i32>>>) {
        if self.assignment_strategy == AssignmentStrategy::CooperativeSticky {
            // Incremental rebalance: set target assignments
            for (member_id, assignment) in &assignments {
                if let Some(member) = self.members.get_mut(member_id) {
                    member.target_assignment = Some(assignment.clone());
                }
            }
            
            // Check if all members have synced their assignments
            let all_synced = self.members.values()
                .all(|m| m.target_assignment.is_some());
            
            if all_synced {
                // Apply target assignments
                for member in self.members.values_mut() {
                    if let Some(target) = member.target_assignment.take() {
                        member.assignment = target;
                        member.owned_partitions = member.assignment.clone();
                    }
                }
                
                self.state = GroupState::Stable;
                self.pending_members.clear();
                self.rebalance_start_time = None;
                info!(
                    group_id = %self.group_id,
                    generation = self.generation_id,
                    "Rebalance completed"
                );
            } else {
                self.state = GroupState::CompletingRebalance;
            }
        } else {
            // Eager rebalance: apply assignments immediately
            for (member_id, assignment) in assignments {
                if let Some(member) = self.members.get_mut(&member_id) {
                    member.assignment = assignment;
                    member.owned_partitions = member.assignment.clone();
                }
            }
            
            self.state = GroupState::Stable;
            self.pending_members.clear();
            self.rebalance_start_time = None;
            info!(
                group_id = %self.group_id,
                generation = self.generation_id,
                "Eager rebalance completed"
            );
        }
    }
    
    /// Get partition assignments for all members
    pub fn get_assignments(&self) -> HashMap<String, HashMap<String, Vec<i32>>> {
        self.members
            .iter()
            .map(|(id, member)| (id.clone(), member.assignment.clone()))
            .collect()
    }
    
    /// Check if group needs persistence
    pub fn needs_persistence(&self) -> bool {
        match self.last_persisted {
            Some(last) => last.elapsed() > Duration::from_secs(5),
            None => true,
        }
    }
    
    /// Mark as persisted
    pub fn mark_persisted(&mut self) {
        self.last_persisted = Some(Instant::now());
    }

    /// Check if all expected members have joined for this generation
    /// NOTE: This is called from an async context, so we can't access pending_join_futures here
    /// Instead, the caller (join_group) should pass the pending count
    pub fn all_members_joined_with_pending_count(&self, pending_count: usize) -> bool {
        // During PreparingRebalance, wait for ALL expected members to rejoin
        // Use both member count AND time-based criteria

        if self.state != GroupState::PreparingRebalance {
            return false;
        }

        // Need at least one member
        if self.members.is_empty() {
            return false;
        }

        // Primary condition: All expected members have pending join requests
        // This indicates they've all sent fresh JoinGroup requests for this generation
        if pending_count >= self.expected_members_count {
            info!(
                group_id = %self.group_id,
                pending = pending_count,
                expected = self.expected_members_count,
                "All expected members have rejoined - completing rebalance"
            );
            return true;
        }

        // Fallback: Wait at least 3 seconds (heartbeat interval) since rebalance started
        // This allows existing members to discover rebalance via heartbeat and rejoin
        if let Some(start_time) = self.rebalance_start_time {
            let elapsed = start_time.elapsed();
            if elapsed >= Duration::from_secs(3) {
                info!(
                    group_id = %self.group_id,
                    pending = pending_count,
                    expected = self.expected_members_count,
                    elapsed_ms = elapsed.as_millis(),
                    "Timeout waiting for all members - completing rebalance with available members"
                );
                return true;
            }
        }

        false
    }

    /// Legacy method for compatibility
    pub fn all_members_joined(&self) -> bool {
        // Can't access pending_join_futures from sync context
        // Default to time-based check only
        if self.state != GroupState::PreparingRebalance {
            return false;
        }

        if self.members.is_empty() {
            return false;
        }

        if let Some(start_time) = self.rebalance_start_time {
            start_time.elapsed() >= Duration::from_secs(3)
        } else {
            false
        }
    }

    /// Complete the join phase - send responses to all waiting members
    pub async fn complete_join_phase(&mut self, pending_futures: &mut HashMap<String, oneshot::Sender<JoinGroupResponse>>) {
        if self.members.is_empty() {
            return;
        }

        // Select leader (first member by key order for deterministic selection)
        let leader_id = self.members.keys().min().cloned().unwrap();
        self.leader_id = Some(leader_id.clone());

        // Transition to CompletingRebalance
        self.state = GroupState::CompletingRebalance;

        info!(
            group_id = %self.group_id,
            generation = self.generation_id,
            member_count = self.members.len(),
            leader = %leader_id,
            "Completing join phase - sending responses to all members"
        );

        // Send responses to all waiting members
        for (member_id, member) in &self.members {
            let is_leader = member_id == &leader_id;

            // Build member list for leader
            let members = if is_leader {
                self.members.iter().enumerate().map(|(idx, (id, m))| {
                    let metadata = m.protocols.first()
                        .map(|(_, data)| data.clone())
                        .unwrap_or_default();
                    tracing::warn!(
                        "Building member[{}] for leader: member_id={}, metadata_len={}, metadata_first_16={:02x?}",
                        idx, id, metadata.len(), &metadata[..metadata.len().min(16)]
                    );
                    MemberInfo {
                        member_id: id.clone(),
                        metadata,
                        owned_partitions: m.owned_partitions.clone(),
                    }
                }).collect()
            } else {
                vec![]
            };

            // Select protocol name - use first available, or use "range" as default
            // CRITICAL: NEVER use empty string as it causes JoinGroup v5 protocol encoding errors
            let protocol_name = member.protocols.first()
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| "range".to_string()); // Default to "range" protocol

            debug!(
                member_id = %member_id,
                protocol = %protocol_name,
                protocol_len = protocol_name.len(),
                is_empty = protocol_name.is_empty(),
                "Sending JoinGroup response with protocol_name"
            );

            let response = JoinGroupResponse {
                error_code: 0,
                generation_id: self.generation_id,
                protocol: protocol_name,
                leader_id: leader_id.clone(),
                member_id: member_id.clone(),
                member_epoch: member.member_epoch,
                members,
            };

            // Send response via oneshot channel
            if let Some(tx) = pending_futures.remove(member_id) {
                if tx.send(response).is_err() {
                    warn!(member_id = %member_id, "Failed to send JoinGroup response - receiver dropped");
                }
            }
        }
    }
}

/// Partition info for assignment
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub topic: String,
    pub partition: i32,
    pub leader: Option<i32>,
}

/// Assignment context for strategies
pub struct AssignmentContext {
    pub partitions: Vec<PartitionInfo>,
    pub subscriptions: HashMap<String, Vec<String>>, // member_id -> topics
    pub current_assignment: HashMap<String, HashMap<String, Vec<i32>>>, // member_id -> topic -> partitions
}

/// Consumer group manager with metadata store integration
pub struct GroupManager {
    groups: Arc<RwLock<HashMap<String, ConsumerGroup>>>,
    metadata_store: Arc<dyn MetadataStore>,
    assignor: Arc<Mutex<Box<dyn PartitionAssignor>>>,
}

impl GroupManager {
    /// Create a new group manager with metadata store
    pub fn new(metadata_store: Arc<dyn MetadataStore>) -> Self {
        Self {
            groups: Arc::new(RwLock::new(HashMap::new())),
            metadata_store,
            assignor: Arc::new(Mutex::new(Box::new(CooperativeStickyAssignor::new()))),
        }
    }
    
    /// Set the partition assignment strategy
    pub async fn set_assignment_strategy(&self, strategy: AssignmentStrategy) {
        let mut assignor = self.assignor.lock().await;
        *assignor = match strategy {
            AssignmentStrategy::Range => Box::new(RangeAssignor::new()),
            AssignmentStrategy::RoundRobin => Box::new(RoundRobinAssignor::new()),
            AssignmentStrategy::Sticky => Box::new(CooperativeStickyAssignor::new()),
            AssignmentStrategy::CooperativeSticky => Box::new(CooperativeStickyAssignor::new()),
        };
    }
    
    /// Fetch committed offsets for a consumer group
    pub async fn fetch_offsets(
        &self,
        group_id: String,
        topics: Vec<String>,
    ) -> Result<Vec<TopicPartitionOffset>> {
        let mut offsets = Vec::new();
        
        for topic in topics {
            // Get topic metadata to know partition count
            if let Some(topic_metadata) = self.metadata_store.get_topic(&topic).await
                .map_err(|e| Error::Storage(format!("Failed to get topic metadata: {}", e)))? {
                
                for partition in 0..topic_metadata.config.partition_count {
                    if let Some(offset) = self.metadata_store
                        .get_consumer_offset(&group_id, &topic, partition).await
                        .map_err(|e| Error::Storage(format!("Failed to get offset: {}", e)))? {
                        
                        offsets.push(TopicPartitionOffset {
                            topic: topic.clone(),
                            partition: partition as i32,
                            offset: offset.offset,
                            metadata: offset.metadata,
                        });
                    } else {
                        // No committed offset, start from beginning
                        offsets.push(TopicPartitionOffset {
                            topic: topic.clone(),
                            partition: partition as i32,
                            offset: 0,
                            metadata: None,
                        });
                    }
                }
            }
        }
        
        Ok(offsets)
    }
    
    /// Get group metadata
    pub async fn describe_group(&self, group_id: String) -> Result<Option<ConsumerGroup>> {
        let groups = self.groups.read().await;
        
        if let Some(group) = groups.get(&group_id) {
            // Return a cloned version to avoid holding the lock
            Ok(Some(ConsumerGroup {
                group_id: group.group_id.clone(),
                state: group.state,
                generation_id: group.generation_id,
                protocol_type: group.protocol_type.clone(),
                protocol: group.protocol.clone(),
                leader_id: group.leader_id.clone(),
                members: group.members.clone(),
                group_epoch: group.group_epoch,
                assignment_strategy: group.assignment_strategy,
                pending_members: group.pending_members.clone(),
                rebalance_start_time: group.rebalance_start_time,
                expected_members_count: group.expected_members_count,
                previous_member_count: group.previous_member_count,
                static_members: group.static_members.clone(),
                last_persisted: group.last_persisted,
                pending_join_futures: Arc::new(Mutex::new(HashMap::new())),
                pending_sync_futures: Arc::new(Mutex::new(HashMap::new())),
            }))
        } else {
            // Try to load from metadata store
            if let Some(metadata) = self.metadata_store.get_consumer_group(&group_id).await
                .map_err(|e| Error::Storage(format!("Failed to load group metadata: {}", e)))? {
                
                let mut group = ConsumerGroup::new(group_id, metadata.protocol_type);
                group.state = GroupState::from_str(&metadata.state).unwrap_or(GroupState::Empty);
                group.generation_id = metadata.generation_id;
                group.protocol = Some(metadata.protocol);
                group.leader_id = metadata.leader_id;
                
                Ok(Some(group))
            } else {
                Ok(None)
            }
        }
    }
    
    /// List all consumer groups
    pub async fn list_groups(&self) -> Result<Vec<String>> {
        let groups = self.groups.read().await;
        let mut group_ids: Vec<String> = groups.keys().cloned().collect();
        
        // Also include groups from metadata store that aren't in memory
        // This would require extending the MetadataStore trait with a list_consumer_groups method
        
        group_ids.sort();
        group_ids.dedup();
        Ok(group_ids)
    }
    
    /// Get or create a consumer group
    pub async fn get_or_create_group(&self, group_id: String, protocol_type: String) -> Result<String> {
        let mut groups = self.groups.write().await;
        
        if !groups.contains_key(&group_id) {
            // Try to load from metadata store
            if let Some(metadata) = self.metadata_store.get_consumer_group(&group_id).await
                .map_err(|e| Error::Storage(format!("Failed to load group metadata: {}", e)))? {
                
                let mut group = ConsumerGroup::new(group_id.clone(), protocol_type);
                group.state = GroupState::from_str(&metadata.state).unwrap_or(GroupState::Empty);
                group.generation_id = metadata.generation_id;
                group.protocol = Some(metadata.protocol);
                group.leader_id = metadata.leader_id;
                
                groups.insert(group_id.clone(), group);
            } else {
                // Create new group
                let group = ConsumerGroup::new(group_id.clone(), protocol_type.clone());
                groups.insert(group_id.clone(), group);
                
                // Persist to metadata store
                let metadata = ConsumerGroupMetadata {
                    group_id: group_id.clone(),
                    state: GroupState::Empty.as_str().to_string(),
                    protocol: String::new(),
                    protocol_type,
                    generation_id: 0,
                    leader_id: None,
                    leader: String::new(),
                    members: Vec::new(),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                };
                
                self.metadata_store.create_consumer_group(metadata).await
                    .map_err(|e| Error::Storage(format!("Failed to persist group metadata: {}", e)))?;
            }
        }
        
        Ok(group_id)
    }
    
    /// Persist group state to metadata store
    async fn persist_group(&self, group: &ConsumerGroup) -> Result<()> {
        // Convert group.members (HashMap<String, GroupMember>) to Vec<chronik_common::metadata::GroupMember>
        let members: Vec<chronik_common::metadata::GroupMember> = group.members.iter()
            .map(|(member_id, member)| {
                // Serialize subscription as metadata (first protocol's metadata if available)
                let metadata = member.protocols.first()
                    .map(|(_, data)| data.clone())
                    .unwrap_or_default();

                // Serialize assignment to wire format
                // Format: array of (topic, partitions) pairs
                let assignment = Self::serialize_assignment(&member.assignment);

                chronik_common::metadata::GroupMember {
                    member_id: member_id.clone(),
                    client_id: member.client_id.clone(),
                    client_host: member.client_host.clone(),
                    metadata,
                    assignment,
                }
            })
            .collect();

        let metadata = ConsumerGroupMetadata {
            group_id: group.group_id.clone(),
            state: group.state.as_str().to_string(),
            protocol: group.protocol.clone().unwrap_or_default(),
            protocol_type: group.protocol_type.clone(),
            generation_id: group.generation_id,
            leader_id: group.leader_id.clone(),
            leader: group.leader_id.clone().unwrap_or_default(),
            members,
            created_at: chrono::Utc::now(), // This should be preserved from initial creation
            updated_at: chrono::Utc::now(),
        };

        // Distributed lock removed - using metadata store directly
        self.metadata_store.update_consumer_group(metadata).await
            .map_err(|e| Error::Storage(format!("Failed to persist group metadata: {}", e)))?;

        Ok(())
    }

    /// Serialize assignment to wire format for persistence
    ///
    /// Format matches Kafka ConsumerGroupAssignment:
    /// - Version (2 bytes): 0
    /// - Assignment count (4 bytes)
    /// - For each topic: topic name length + topic name + partition count + partition IDs
    fn serialize_assignment(assignment: &HashMap<String, Vec<i32>>) -> Vec<u8> {
        let mut buf = Vec::new();

        // Version (short) - 0 for basic format
        buf.extend_from_slice(&0i16.to_be_bytes());

        // Assignment count (int)
        buf.extend_from_slice(&(assignment.len() as i32).to_be_bytes());

        for (topic, partitions) in assignment {
            // Topic name (string: length + bytes)
            buf.extend_from_slice(&(topic.len() as i16).to_be_bytes());
            buf.extend_from_slice(topic.as_bytes());

            // Partition count
            buf.extend_from_slice(&(partitions.len() as i32).to_be_bytes());

            // Partition IDs
            for partition in partitions {
                buf.extend_from_slice(&partition.to_be_bytes());
            }
        }

        buf
    }
    
    /// Join a consumer group with KIP-848 support
    /// Resolve member ID from static or dynamic membership
    ///
    /// Handles static member lookups and dynamic member ID generation.
    /// Complexity: < 10 (simple ID resolution logic)
    fn resolve_member_id(
        group: &mut ConsumerGroup,
        member_id: Option<String>,
        client_id: &str,
        static_member_id: &Option<String>,
    ) -> String {
        if let Some(static_id) = static_member_id {
            // Check if static member already exists
            if let Some(existing_member_id) = group.static_members.get(static_id) {
                // Static member rejoining - use existing member ID
                existing_member_id.clone()
            } else {
                // New static member
                let new_member_id = member_id.unwrap_or_else(|| {
                    format!("{}-{}", client_id, uuid::Uuid::new_v4())
                });
                group.static_members.insert(static_id.clone(), new_member_id.clone());
                new_member_id
            }
        } else {
            // Dynamic member
            member_id.unwrap_or_else(|| {
                format!("{}-{}", client_id, uuid::Uuid::new_v4())
            })
        }
    }

    /// Check if member is rejoining and create/update member
    ///
    /// Preserves owned partitions for cooperative sticky rebalance.
    /// Complexity: < 15 (member creation + partition preservation)
    fn check_rejoin_and_create_member(
        group: &mut ConsumerGroup,
        member_id: &str,
        client_id: String,
        client_host: String,
        session_timeout: Duration,
        rebalance_timeout: Duration,
        protocols: &[(String, Vec<u8>)],
    ) -> Result<bool> {
        // Check if this is a rejoin
        let is_rejoin = group.members.contains_key(member_id);

        // Parse subscription from protocols
        let subscription = if let Some((_, metadata)) = protocols.first() {
            parse_subscription_metadata(metadata)?
        } else {
            vec![]
        };

        // Create or update member
        let mut member = GroupMember::new(
            member_id.to_string(),
            client_id,
            client_host,
            session_timeout,
            rebalance_timeout,
            subscription,
            protocols.to_vec(),
        );

        // Preserve owned partitions for incremental rebalance
        if is_rejoin && group.assignment_strategy == AssignmentStrategy::CooperativeSticky {
            if let Some(existing) = group.members.get(member_id) {
                member.owned_partitions = existing.owned_partitions.clone();
            }
        }

        group.add_member(member);
        Ok(is_rejoin)
    }

    /// Trigger rebalance when new member joins stable group
    ///
    /// Notifies existing members with REBALANCE_IN_PROGRESS error,
    /// then triggers rebalance with forced redistribution.
    /// Complexity: < 20 (notification loop + rebalance trigger)
    async fn trigger_rebalance_for_new_member(
        group: &mut ConsumerGroup,
        group_id: &str,
        new_member_id: &str,
    ) {
        info!(
            group_id = %group_id,
            member_id = %new_member_id,
            member_count = group.members.len(),
            "New member joining stable group - triggering rebalance and notifying existing members"
        );

        // BEFORE triggering rebalance, send REBALANCE_IN_PROGRESS to existing members
        // This ensures they know to rejoin with fresh JoinGroup requests
        {
            let mut pending_futures = group.pending_join_futures.lock().await;
            let existing_member_ids: Vec<String> = group.members.keys()
                .filter(|id| *id != new_member_id) // Exclude the new member we just added
                .cloned()
                .collect();

            for existing_member_id in existing_member_ids {
                if let Some(tx) = pending_futures.remove(&existing_member_id) {
                    warn!(
                        group_id = %group_id,
                        member_id = %existing_member_id,
                        "Sending REBALANCE_IN_PROGRESS to existing member to trigger rejoin"
                    );
                    let rebalance_response = JoinGroupResponse {
                        error_code: 27, // REBALANCE_IN_PROGRESS
                        generation_id: group.generation_id,
                        protocol: group.protocol.clone().unwrap_or_else(|| "range".to_string()),
                        leader_id: group.leader_id.clone().unwrap_or_default(),
                        member_id: existing_member_id.clone(),
                        member_epoch: group.members.get(&existing_member_id).map(|m| m.member_epoch).unwrap_or(0),
                        members: vec![],
                    };
                    let _ = tx.send(rebalance_response); // Ignore error if receiver dropped
                }
            }
        }

        // Now trigger the rebalance with forced redistribution (new member joining)
        group.trigger_rebalance_with_redistribution();
        // NOTE: trigger_rebalance_with_redistribution() clears owned_partitions to force
        // redistribution of partitions across all members (including new member)
    }

    /// Handle deferred join (PreparingRebalance or Empty state)
    ///
    /// Spawns polling task to check if all members joined, waits for response.
    /// Complexity: < 25 (spawn task + timeout wait)
    async fn handle_deferred_join(
        groups: Arc<RwLock<HashMap<String, ConsumerGroup>>>,
        group_id: String,
        member_id: String,
        rebalance_timeout: Duration,
    ) -> Result<JoinGroupResponse> {
        // Create a oneshot channel for this member's response
        let (tx, rx) = oneshot::channel();

        {
            let mut groups_write = groups.write().await;
            if let Some(group) = groups_write.get_mut(&group_id) {
                let mut pending_futures = group.pending_join_futures.lock().await;
                pending_futures.insert(member_id.clone(), tx);
            }
        }

        // Clone group_id for logging in spawned task
        let group_id_clone = group_id.clone();
        let groups_clone = groups.clone();

        // Spawn a task to check if join phase should complete
        tokio::spawn(async move {
            // Poll every 200ms to check if all members have joined
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;

                let mut groups_guard = groups_clone.write().await;
                if let Some(group) = groups_guard.get_mut(&group_id_clone) {
                    let pending_futures_arc = group.pending_join_futures.clone();
                    let pending_futures = pending_futures_arc.lock().await;
                    let pending_count = pending_futures.len();

                    if group.all_members_joined_with_pending_count(pending_count) {
                        info!(
                            group_id = %group_id_clone,
                            members = group.members.len(),
                            pending = pending_count,
                            "All members joined - completing join phase"
                        );
                        drop(pending_futures); // Release lock before calling complete_join_phase
                        let mut pending_futures_mut = pending_futures_arc.lock().await;
                        group.complete_join_phase(&mut *pending_futures_mut).await;
                        break; // Exit loop after completing
                    }

                    // Check if group is no longer in PreparingRebalance (might have been cancelled)
                    if group.state != GroupState::PreparingRebalance {
                        break;
                    }
                } else {
                    // Group was deleted
                    break;
                }
            }
        });

        // Wait for response (will be sent by complete_join_phase after delay)
        match tokio::time::timeout(rebalance_timeout, rx).await {
            Ok(Ok(response)) => {
                info!(
                    group_id = %group_id,
                    member_id = %response.member_id,
                    generation = response.generation_id,
                    is_leader = response.member_id == response.leader_id,
                    "Member received JoinGroup response after waiting"
                );
                Ok(response)
            }
            Ok(Err(_)) => Err(Error::Internal("JoinGroup response channel closed".into())),
            Err(_) => Err(Error::Internal("JoinGroup timeout waiting for rebalance".into())),
        }
    }

    /// Build immediate join response for stable state
    ///
    /// Returns response for rejoining member in stable group.
    /// Complexity: < 15 (response construction)
    fn build_stable_join_response(
        group: &ConsumerGroup,
        member_id: String,
        protocols: &[(String, Vec<u8>)],
    ) -> JoinGroupResponse {
        let is_leader = group.leader_id.as_ref() == Some(&member_id);
        let member_epoch_val = group.members.get(&member_id).map(|m| m.member_epoch).unwrap_or(0);

        // Select protocol name - use first available, or use "range" as default
        // CRITICAL: NEVER use empty string as it causes JoinGroup v5 protocol encoding errors
        let protocol_name = protocols.first()
            .map(|(name, _)| name.clone())
            .or_else(|| {
                // Check if member has protocols stored
                group.members.get(&member_id)
                    .and_then(|m| m.protocols.first())
                    .map(|(name, _)| name.clone())
            })
            .unwrap_or_else(|| "range".to_string()); // Default to "range" protocol

        JoinGroupResponse {
            error_code: 0,
            generation_id: group.generation_id,
            protocol: protocol_name,
            leader_id: group.leader_id.clone().unwrap_or_default(),
            member_id: member_id.clone(),
            member_epoch: member_epoch_val,
            members: if is_leader {
                group.members.iter().map(|(id, m)| MemberInfo {
                    member_id: id.clone(),
                    metadata: m.protocols.first()
                        .map(|(_, data)| data.clone())
                        .unwrap_or_default(),
                    owned_partitions: m.owned_partitions.clone(),
                }).collect()
            } else {
                vec![]
            },
        }
    }

    /// Join a consumer group (refactored for maintainability)
    ///
    /// Handles static/dynamic membership, rebalancing, and state transitions.
    /// **Refactored**: Reduced from 228 lines (~80-100 complexity) to ~60 lines (<25 complexity)
    /// Complexity: < 25 (simple routing to extracted helpers)
    pub async fn join_group(
        &self,
        group_id: String,
        member_id: Option<String>,
        client_id: String,
        client_host: String,
        session_timeout: Duration,
        rebalance_timeout: Duration,
        protocol_type: String,
        protocols: Vec<(String, Vec<u8>)>,
        static_member_id: Option<String>,
    ) -> Result<JoinGroupResponse> {
        // Ensure group exists
        self.get_or_create_group(group_id.clone(), protocol_type.clone()).await?;

        // Phase 1: Resolve member ID (static vs dynamic membership)
        let resolved_member_id = {
            let mut groups = self.groups.write().await;
            let group = groups.get_mut(&group_id)
                .ok_or_else(|| Error::Internal("Group not found after creation".into()))?;
            Self::resolve_member_id(group, member_id, &client_id, &static_member_id)
        };

        // Phase 2: Create/update member and check if rejoin
        let is_rejoin = {
            let mut groups = self.groups.write().await;
            let group = groups.get_mut(&group_id)
                .ok_or_else(|| Error::Internal("Group not found".into()))?;
            Self::check_rejoin_and_create_member(
                group,
                &resolved_member_id,
                client_id,
                client_host,
                session_timeout,
                rebalance_timeout,
                &protocols,
            )?
        };

        // Phase 3: Trigger rebalance if new member joins stable group
        let group_state = {
            let mut groups = self.groups.write().await;
            let group = groups.get_mut(&group_id)
                .ok_or_else(|| Error::Internal("Group not found".into()))?;

            if group.state == GroupState::Stable && !is_rejoin {
                Self::trigger_rebalance_for_new_member(group, &group_id, &resolved_member_id).await;
            }

            group.state
        };

        // Phase 4: Route to appropriate handler based on group state
        match group_state {
            GroupState::PreparingRebalance | GroupState::Empty => {
                // Deferred join - spawn polling task and wait for response
                Self::handle_deferred_join(
                    self.groups.clone(),
                    group_id,
                    resolved_member_id,
                    rebalance_timeout,
                ).await
            }
            GroupState::Stable => {
                // Immediate response for rejoining member
                let groups = self.groups.read().await;
                let group = groups.get(&group_id)
                    .ok_or_else(|| Error::Internal("Group not found".into()))?;
                Ok(Self::build_stable_join_response(group, resolved_member_id, &protocols))
            }
            _ => Err(Error::InvalidInput(format!("Invalid group state: {:?}", group_state))),
        }
    }
    
    /// Sync group state with incremental rebalance support
    ///
    /// **CRITICAL FIX**: Followers MUST wait for leader to compute assignments
    /// This prevents the race condition where followers call SyncGroup before the leader,
    /// receive empty assignments, and prematurely transition to Stable state.
    ///
    /// **Refactored**: Reduced from 370 lines (136 complexity) to 90 lines (<25 complexity)
    pub async fn sync_group(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        member_epoch: i32,
        assignments: Option<Vec<(String, Vec<u8>)>>,
    ) -> Result<SyncGroupResponse> {
        use crate::consumer_group::{
            SyncValidator, FollowerSync, LeaderAssignment, SyncResponseBuilder
        };

        let mut groups = self.groups.write().await;
        let group = groups.get_mut(&group_id)
            .ok_or_else(|| Error::InvalidInput(format!("Unknown group: {}", group_id)))?;

        // Phase 1: Validation
        if let Some(error_response) = SyncValidator::validate_generation(group, generation_id) {
            return Ok(error_response);
        }

        if let Some(error_response) = SyncValidator::validate_member_epoch(group, &member_id, member_epoch) {
            return Ok(error_response);
        }

        SyncValidator::remove_from_pending(group, &member_id);

        let is_leader = SyncValidator::is_leader(group, &member_id);
        let rebalance_timeout = Duration::from_secs(60);

        // Phase 2: Follower wait path
        if FollowerSync::should_wait_for_leader(group, is_leader) {
            let (tx, rx) = oneshot::channel();

            {
                let mut pending_sync = group.pending_sync_futures.lock().await;
                pending_sync.insert(member_id.clone(), tx);

                // Spawn server-side fallback task if all followers are waiting
                let follower_count = group.members.len() - 1;
                FollowerSync::spawn_fallback_task(
                    group,
                    self.groups.clone(),
                    self.metadata_store.clone(),
                    self.assignor.clone(),
                    group_id.clone(),
                    follower_count,
                    pending_sync.len(),
                );
            }

            // Drop write lock before awaiting
            drop(groups);

            // Wait for leader assignment
            return FollowerSync::wait_for_assignment_rx(rx, rebalance_timeout, &group_id, &member_id).await;
        }

        // Phase 3: Leader assignment path
        if is_leader {
            let computed_assignments = LeaderAssignment::resolve_assignments(
                self,
                group,
                &member_id,
                assignments,
            ).await?;

            if !computed_assignments.is_empty() {
                LeaderAssignment::apply_assignments(group, &computed_assignments);
                LeaderAssignment::distribute_assignments(group, &computed_assignments, &member_id).await;

                // Persist group state after assignment
                if let Err(e) = self.persist_group(group).await {
                    warn!(
                        group_id = %group.group_id,
                        error = %e,
                        "Failed to persist group state after assignment"
                    );
                }

                return Ok(LeaderAssignment::build_leader_response(
                    group,
                    &computed_assignments,
                    &member_id,
                ));
            }
        }

        // Phase 4: Fallback path
        Ok(SyncResponseBuilder::build_fallback_response(group, &member_id))
    }
    
    /// Compute partition assignments using the configured strategy
    async fn compute_assignments(&self, group: &ConsumerGroup) -> Result<HashMap<String, HashMap<String, Vec<i32>>>> {
        // Gather partition information
        let topics: HashSet<String> = group.members.values()
            .flat_map(|m| m.subscription.iter())
            .cloned()
            .collect();
        
        info!(
            group_id = %group.group_id,
            topics = ?topics,
            members = ?group.members.keys().collect::<Vec<_>>(),
            "Computing partition assignments"
        );
        
        let mut partitions = Vec::new();
        for topic in &topics {
            if let Some(topic_metadata) = self.metadata_store.get_topic(topic).await
                .map_err(|e| Error::Storage(format!("Failed to get topic metadata: {}", e)))? {
                
                let partition_count = topic_metadata.config.partition_count;
                info!(
                    topic = %topic,
                    partition_count = partition_count,
                    "Adding partitions for topic"
                );
                
                for partition in 0..partition_count {
                    // Get partition leader from metadata store
                    let leader = self.metadata_store.get_partition_leader(topic, partition)
                        .await
                        .ok()
                        .flatten();

                    partitions.push(PartitionInfo {
                        topic: topic.clone(),
                        partition: partition as i32,
                        leader,
                    });
                }
            } else {
                warn!(
                    topic = %topic,
                    "Topic not found in metadata"
                );
            }
        }
        
        info!(
            total_partitions = partitions.len(),
            "Total partitions to assign"
        );
        
        // Build assignment context
        let context = AssignmentContext {
            partitions,
            subscriptions: group.members.iter()
                .map(|(id, m)| (id.clone(), m.subscription.clone()))
                .collect(),
            current_assignment: group.members.iter()
                .map(|(id, m)| (id.clone(), m.owned_partitions.clone()))
                .collect(),
        };
        
        // Use the configured assignor
        let assignor = self.assignor.lock().await;
        let assignments = assignor.assign(&context)?;
        
        // Log the assignments
        for (member_id, member_assignments) in &assignments {
            for (topic, partitions) in member_assignments {
                info!(
                    group_id = %group.group_id,
                    member_id = %member_id,
                    topic = %topic,
                    partitions = ?partitions,
                    "Assigned partitions to member"
                );
            }
        }
        
        Ok(assignments)
    }
    
    /// Leave a consumer group with graceful handling
    pub async fn leave_group(&self, group_id: String, member_id: String) -> Result<()> {
        let mut groups = self.groups.write().await;
        
        if let Some(group) = groups.get_mut(&group_id) {
            // Mark member as leaving for incremental rebalance
            if let Some(member) = group.members.get_mut(&member_id) {
                member.is_leaving = true;
            }
            
            info!(
                group_id = %group_id,
                member_id = %member_id,
                "Member leaving group"
            );
            
            group.remove_member(&member_id);
            
            if group.members.is_empty() {
                // Remove empty group
                groups.remove(&group_id);
                
                // Clean up from metadata store
                if let Err(e) = self.metadata_store.update_consumer_group(ConsumerGroupMetadata {
                    group_id: group_id.clone(),
                    state: GroupState::Dead.as_str().to_string(),
                    protocol: String::new(),
                    protocol_type: String::new(),
                    generation_id: 0,
                    leader_id: None,
                    leader: String::new(),
                    members: Vec::new(),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                }).await {
                    warn!(
                        group_id = %group_id,
                        error = %e,
                        "Failed to mark group as dead in metadata store"
                    );
                }
            } else {
                // Persist updated group state
                if let Err(e) = self.persist_group(group).await {
                    warn!(
                        group_id = %group.group_id,
                        error = %e,
                        "Failed to persist group state after member leave"
                    );
                }
            }
        }
        
        Ok(())
    }
    
    /// Heartbeat for a group member with KIP-848 support
    pub async fn heartbeat(
        &self, 
        group_id: String, 
        member_id: String, 
        generation_id: i32,
        member_epoch: Option<i32>,
    ) -> Result<HeartbeatResponse> {
        let mut groups = self.groups.write().await;
        let group = groups.get_mut(&group_id)
            .ok_or_else(|| Error::InvalidInput(format!("Unknown group: {}", group_id)))?;
        
        // Validate generation
        if group.generation_id != generation_id {
            return Ok(HeartbeatResponse {
                error_code: 27, // ILLEGAL_GENERATION
            });
        }
        
        // Validate member epoch for incremental rebalance
        if let (Some(epoch), Some(member)) = (member_epoch, group.members.get(&member_id)) {
            if member.member_epoch != epoch {
                return Ok(HeartbeatResponse {
                    error_code: 78, // FENCED_MEMBER_EPOCH
                });
            }
        }
        
        group.heartbeat(&member_id)?;
        
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
    
    /// Commit offsets for a consumer group
    pub async fn commit_offsets(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        member_epoch: Option<i32>,
        offsets: Vec<TopicPartitionOffset>,
    ) -> Result<CommitOffsetsResponse> {
        let groups = self.groups.read().await;
        let group = groups.get(&group_id)
            .ok_or_else(|| Error::InvalidInput(format!("Unknown group: {}", group_id)))?;
        
        // Validate generation
        if group.generation_id != generation_id {
            return Ok(CommitOffsetsResponse {
                error_code: 27, // ILLEGAL_GENERATION
                partition_errors: offsets.into_iter()
                    .map(|o| PartitionError {
                        topic: o.topic,
                        partition: o.partition,
                        error_code: 27,
                    })
                    .collect(),
            });
        }
        
        // Validate member
        if !group.members.contains_key(&member_id) {
            return Ok(CommitOffsetsResponse {
                error_code: 25, // UNKNOWN_MEMBER_ID
                partition_errors: offsets.into_iter()
                    .map(|o| PartitionError {
                        topic: o.topic,
                        partition: o.partition,
                        error_code: 25,
                    })
                    .collect(),
            });
        }
        
        // Validate member epoch for incremental rebalance
        if let (Some(epoch), Some(member)) = (member_epoch, group.members.get(&member_id)) {
            if member.member_epoch != epoch {
                return Ok(CommitOffsetsResponse {
                    error_code: 78, // FENCED_MEMBER_EPOCH
                    partition_errors: offsets.into_iter()
                        .map(|o| PartitionError {
                            topic: o.topic,
                            partition: o.partition,
                            error_code: 78,
                        })
                        .collect(),
                });
            }
        }
        
        // Store offsets in metadata store
        let mut partition_errors = Vec::new();
        let timestamp = chrono::Utc::now();
        
        for offset in offsets {
            let consumer_offset = ConsumerOffset {
                group_id: group_id.clone(),
                topic: offset.topic.clone(),
                partition: offset.partition as u32,
                offset: offset.offset,
                metadata: offset.metadata,
                commit_timestamp: timestamp,
            };
            
            match self.metadata_store.commit_offset(consumer_offset).await {
                Ok(_) => {
                    partition_errors.push(PartitionError {
                        topic: offset.topic,
                        partition: offset.partition,
                        error_code: 0,
                    });
                }
                Err(e) => {
                    error!(
                        group_id = %group_id,
                        topic = %offset.topic,
                        partition = offset.partition,
                        error = %e,
                        "Failed to commit offset"
                    );
                    partition_errors.push(PartitionError {
                        topic: offset.topic,
                        partition: offset.partition,
                        error_code: 5, // COORDINATOR_NOT_AVAILABLE
                    });
                }
            }
        }
        
        Ok(CommitOffsetsResponse {
            error_code: 0,
            partition_errors,
        })
    }
    
    /// Fetch committed offsets for a consumer group (with optional topics filter)
    pub async fn fetch_offsets_optional(
        &self,
        group_id: String,
        topics: Option<Vec<String>>,
    ) -> Result<Vec<TopicPartitionOffset>> {
        // Verify group exists
        let groups = self.groups.read().await;
        if !groups.contains_key(&group_id) {
            // It's ok if group doesn't exist - just return empty offsets
            return Ok(Vec::new());
        }
        drop(groups);
        
        // Fetch offsets from metadata store
        let mut offsets: Vec<TopicPartitionOffset> = Vec::new();

        // Get list of topics to query
        let topics_to_query: Vec<String> = match topics {
            Some(t) => t,
            None => {
                // No topics specified - get all topics from metadata store
                match self.metadata_store.list_topics().await {
                    Ok(all_topics) => all_topics.into_iter().map(|t| t.name).collect(),
                    Err(e) => {
                        tracing::warn!("Failed to list topics for offset fetch: {}", e);
                        return Ok(Vec::new());
                    }
                }
            }
        };

        // For each topic, get partition count and fetch offsets
        for topic in topics_to_query {
            let partition_count = match self.metadata_store.get_topic(&topic).await {
                Ok(Some(topic_meta)) => topic_meta.config.partition_count,
                Ok(None) => {
                    tracing::debug!("Topic {} not found, skipping", topic);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Failed to get topic {}: {}", topic, e);
                    continue;
                }
            };

            // Fetch offset for each partition
            for partition in 0..partition_count {
                match self.metadata_store.get_consumer_offset(&group_id, &topic, partition).await {
                    Ok(Some(offset)) => {
                        offsets.push(TopicPartitionOffset {
                            topic: topic.clone(),
                            partition: partition as i32,
                            offset: offset.offset,
                            metadata: offset.metadata,
                        });
                    }
                    Ok(None) => {
                        // No committed offset for this partition - this is normal
                        // Return -1 to indicate no offset committed
                        offsets.push(TopicPartitionOffset {
                            topic: topic.clone(),
                            partition: partition as i32,
                            offset: -1,
                            metadata: None,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Failed to get offset for {}/{}: {}", topic, partition, e);
                        // Still include the partition with -1 offset
                        offsets.push(TopicPartitionOffset {
                            topic: topic.clone(),
                            partition: partition as i32,
                            offset: -1,
                            metadata: None,
                        });
                    }
                }
            }
        }

        Ok(offsets)
    }
    
    /// Start background task to check expired members
    pub fn start_expiration_checker(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                
                let mut groups = self.groups.write().await;
                let mut empty_groups = Vec::new();
                
                for (group_id, group) in groups.iter_mut() {
                    let expired = group.check_expired_members();
                    if !expired.is_empty() {
                        tracing::info!("Expired members in group {}: {:?}", group_id, expired);
                        if group.members.is_empty() {
                            empty_groups.push(group_id.clone());
                        }
                    }
                }
                
                // Remove empty groups
                for group_id in empty_groups {
                    groups.remove(&group_id);
                }
            }
        });
    }
    
    // Protocol wrapper methods for kafka_handler compatibility
    pub async fn handle_join_group(
        &self,
        request: chronik_protocol::join_group_types::JoinGroupRequest,
        header_client_id: Option<String>,
        api_version: i16,
    ) -> Result<chronik_protocol::join_group_types::JoinGroupResponse> {
        use std::time::Duration;

        // Use client_id from request header (most reliable source for dynamic membership)
        // Fall back to group_instance_id (for static membership), then generate unique ID
        let client_id = header_client_id
            .or_else(|| request.group_instance_id.clone())
            .unwrap_or_else(|| format!("chronik-consumer-{}", uuid::Uuid::new_v4()));

        info!(
            group_id = %request.group_id,
            member_id = %request.member_id,
            group_instance_id = ?request.group_instance_id,
            client_id = %client_id,
            "JoinGroup request received"
        );

        // Save protocol_type before we move the request fields
        let protocol_type_for_response = request.protocol_type.clone();

        // Convert protocol request to internal format
        let protocols = request.protocols.into_iter()
            .map(|p| (p.name, p.metadata.to_vec()))
            .collect();

        // Call the real join_group implementation with proper parameters
        // Note: client_host ideally comes from the TCP socket peer_addr, but that would
        // require propagating it through the handler chain. Using client_id as identifier
        // for now - it's unique per client and serves the same debugging/metadata purpose.
        let client_host = format!("/{}", client_id);
        let result = self.join_group(
            request.group_id,
            if request.member_id.is_empty() { None } else { Some(request.member_id.clone()) },
            client_id,
            client_host,
            Duration::from_millis(request.session_timeout_ms as u64),
            Duration::from_millis(if request.rebalance_timeout_ms > 0 {
                request.rebalance_timeout_ms as u64
            } else {
                request.session_timeout_ms as u64
            }),
            request.protocol_type,
            protocols,
            request.group_instance_id,
        ).await?;

        // Convert internal response to protocol format
        let is_leader = result.member_id == result.leader_id;
        tracing::warn!(
            "handle_join_group response received: member_id={}, leader_id={}, is_leader={}, result.members.len()={}",
            result.member_id, result.leader_id, is_leader, result.members.len()
        );
        let members = if is_leader {
            result.members.into_iter().enumerate().map(|(idx, m)| {
                tracing::warn!(
                    "Converting member[{}] to wire format: member_id={}, metadata_len={}, metadata_first_16={:02x?}",
                    idx, m.member_id, m.metadata.len(), &m.metadata[..m.metadata.len().min(16)]
                );
                chronik_protocol::join_group_types::JoinGroupResponseMember {
                    member_id: m.member_id,
                    group_instance_id: None, // Not supported in MemberInfo struct
                    metadata: bytes::Bytes::from(m.metadata),
                }
            }).collect()
        } else {
            tracing::warn!("Non-leader member, members list will be empty");
            vec![]
        };

        Ok(chronik_protocol::join_group_types::JoinGroupResponse {
            error_code: result.error_code,
            generation_id: result.generation_id,
            protocol_type: if api_version >= 7 {
                // v7+ requires protocol_type to be non-null
                // Use saved protocol_type, or default to "consumer" if empty
                let ptype = if protocol_type_for_response.is_empty() {
                    "consumer".to_string()
                } else {
                    protocol_type_for_response
                };
                Some(ptype)
            } else {
                None // v6 and earlier don't have this field
            },
            protocol_name: Some(result.protocol), // Already ensured non-empty in complete_join_phase
            leader: result.leader_id,
            member_id: result.member_id,
            members,
            throttle_time_ms: 0,
        })
    }
    
    pub async fn handle_sync_group(&self, request: chronik_protocol::sync_group_types::SyncGroupRequest) -> Result<chronik_protocol::sync_group_types::SyncGroupResponse> {
        // Clone values we'll need for logging later
        let group_id_for_log = request.group_id.clone();
        let member_id_for_log = request.member_id.clone();

        // Convert assignments from protocol format to our internal format
        let assignments = if !request.assignments.is_empty() {
            let mut parsed_assignments = Vec::new();
            for assignment in request.assignments {
                parsed_assignments.push((assignment.member_id, assignment.assignment.to_vec()));
            }
            Some(parsed_assignments)
        } else {
            None
        };

        // Call the real sync group implementation
        let sync_response = self.sync_group(
            request.group_id,
            request.generation_id,
            request.member_id,
            0, // member_epoch - protocol doesn't have this field yet
            assignments,
        ).await?;

        // Convert back to protocol format
        let assignment_bytes = bytes::Bytes::from(sync_response.assignment);

        // DEBUG: Log assignment bytes in hex format
        let hex_str: String = assignment_bytes.iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");

        info!(
            group_id = %group_id_for_log,
            member_id = %member_id_for_log,
            assignment_len = assignment_bytes.len(),
            assignment_hex = %hex_str,
            "SyncGroup response assignment bytes"
        );

        Ok(chronik_protocol::sync_group_types::SyncGroupResponse {
            error_code: sync_response.error_code,
            protocol_name: request.protocol_name,
            protocol_type: request.protocol_type,
            assignment: assignment_bytes,
            throttle_time_ms: 0,
        })
    }
    
    pub async fn handle_heartbeat(&self, request: chronik_protocol::heartbeat_types::HeartbeatRequest) -> Result<chronik_protocol::heartbeat_types::HeartbeatResponse> {
        // Call the real heartbeat implementation to update member's last_heartbeat timestamp
        let heartbeat_response = self.heartbeat(
            request.group_id,
            request.member_id,
            request.generation_id,
            None, // member_epoch not used in v3
        ).await?;

        Ok(chronik_protocol::heartbeat_types::HeartbeatResponse {
            error_code: heartbeat_response.error_code,
            throttle_time_ms: 0,
        })
    }
    
    pub async fn handle_leave_group(&self, request: chronik_protocol::leave_group_types::LeaveGroupRequest) -> Result<chronik_protocol::leave_group_types::LeaveGroupResponse> {
        use chronik_protocol::leave_group_types::MemberResponse;

        // V3+ requires per-member responses
        let member_responses: Vec<MemberResponse> = request.members.iter().map(|member| {
            MemberResponse {
                member_id: member.member_id.clone(),
                group_instance_id: member.group_instance_id.clone(),
                error_code: 0, // SUCCESS
            }
        }).collect();

        Ok(chronik_protocol::leave_group_types::LeaveGroupResponse {
            error_code: 0,
            members: member_responses,
            throttle_time_ms: 0,
        })
    }
    
    pub async fn handle_offset_commit(&self, request: chronik_protocol::types::OffsetCommitRequest) -> Result<chronik_protocol::types::OffsetCommitResponse> {
        use chronik_protocol::types::{OffsetCommitResponseTopic, OffsetCommitResponsePartition};

        info!("OffsetCommit for group: {}", request.group_id);

        let mut response_topics = Vec::new();

        for topic in &request.topics {
            let mut response_partitions = Vec::new();

            for partition in &topic.partitions {
                // Store offset in metadata store
                let consumer_offset = ConsumerOffset {
                    group_id: request.group_id.clone(),
                    topic: topic.name.clone(),
                    partition: partition.partition_index as u32,
                    offset: partition.committed_offset,
                    metadata: partition.committed_metadata.clone(),
                    commit_timestamp: chronik_common::Utc::now(),
                };

                // Commit to metadata store
                debug!("Committing offset for group={} topic={} partition={} offset={}",
                      request.group_id, topic.name, partition.partition_index, partition.committed_offset);

                if let Err(e) = self.metadata_store.commit_offset(consumer_offset).await {
                    warn!(
                        "✗ Failed to persist offset to metadata: group={} topic={} partition={} offset={} error={}",
                        request.group_id, topic.name, partition.partition_index, partition.committed_offset, e
                    );
                    response_partitions.push(OffsetCommitResponsePartition {
                        partition_index: partition.partition_index,
                        error_code: 1, // Error
                    });
                } else {
                    info!(
                        "✓ Persisted offset to metadata: group={} topic={} partition={} offset={}",
                        request.group_id, topic.name, partition.partition_index, partition.committed_offset
                    );
                    response_partitions.push(OffsetCommitResponsePartition {
                        partition_index: partition.partition_index,
                        error_code: 0, // Success
                    });
                }
            }

            response_topics.push(OffsetCommitResponseTopic {
                name: topic.name.clone(),
                partitions: response_partitions,
            });
        }

        Ok(chronik_protocol::types::OffsetCommitResponse {
            header: chronik_protocol::parser::ResponseHeader {
                correlation_id: 0, // Will be overwritten by kafka_handler
            },
            throttle_time_ms: 0,
            topics: response_topics,
        })
    }
    
    pub async fn handle_offset_fetch(&self, request: chronik_protocol::types::OffsetFetchRequest) -> Result<chronik_protocol::types::OffsetFetchResponse> {
        use chronik_protocol::types::{OffsetFetchResponseTopic, OffsetFetchResponsePartition};

        info!("OffsetFetch for group: {}", request.group_id);

        let topics = if let Some(requested_topics) = request.topics {
            let mut response_topics = Vec::new();

            for topic_request in requested_topics {
                let mut partition_responses = Vec::new();

                // If no specific partitions requested, fetch for all partitions
                let partitions = if topic_request.partitions.is_empty() {
                    // v0-v7: No partitions specified means "fetch ALL partitions for this topic"
                    // Query metadata to find actual partition count
                    match self.metadata_store.get_topic(&topic_request.name).await {
                        Ok(Some(topic_meta)) => {
                            // Return ALL partitions for this topic
                            (0..topic_meta.config.partition_count as i32).collect()
                        }
                        Ok(None) => {
                            // Topic doesn't exist, return empty (client will get error_code 3 for unknown topic)
                            warn!("Topic '{}' not found for OffsetFetch request", topic_request.name);
                            Vec::new()
                        }
                        Err(e) => {
                            warn!("Failed to get topic metadata for '{}': {}", topic_request.name, e);
                            Vec::new()
                        }
                    }
                } else {
                    topic_request.partitions
                };

                // Fetch offset for each requested partition
                for partition_id in partitions {
                    debug!("Fetching offset for group={} topic={} partition={}",
                          request.group_id, topic_request.name, partition_id);

                    match self.metadata_store.get_consumer_offset(&request.group_id, &topic_request.name, partition_id as u32).await {
                        Ok(Some(offset_info)) => {
                            info!(
                                "✓ Retrieved offset from metadata: group={} topic={} partition={} offset={}",
                                request.group_id, topic_request.name, partition_id, offset_info.offset
                            );
                            partition_responses.push(OffsetFetchResponsePartition {
                                partition_index: partition_id,
                                committed_offset: offset_info.offset,
                                metadata: offset_info.metadata,
                                error_code: 0,
                            });
                        }
                        Ok(None) => {
                            // No committed offset found, return -1
                            info!(
                                "✗ No committed offset found for group={} topic={} partition={} (returning -1)",
                                request.group_id, topic_request.name, partition_id
                            );
                            partition_responses.push(OffsetFetchResponsePartition {
                                partition_index: partition_id,
                                committed_offset: -1,  // No committed offset
                                metadata: None,
                                error_code: 0,
                            });
                        }
                        Err(e) => {
                            // Error retrieving offset
                            warn!(
                                "✗ ERROR retrieving offset for group={} topic={} partition={}: {}",
                                request.group_id, topic_request.name, partition_id, e
                            );
                            partition_responses.push(OffsetFetchResponsePartition {
                                partition_index: partition_id,
                                committed_offset: -1,  // No committed offset
                                metadata: None,
                                error_code: 0,
                            });
                        }
                    }
                }

                response_topics.push(OffsetFetchResponseTopic {
                    name: topic_request.name,
                    partitions: partition_responses,
                });
            }

            response_topics
        } else {
            // Return empty response if no topics specified
            vec![]
        };

        Ok(chronik_protocol::types::OffsetFetchResponse {
            header: chronik_protocol::parser::ResponseHeader {
                correlation_id: 0,  // Will be set by the handler
            },
            throttle_time_ms: 0,
            topics,
            group_id: Some(request.group_id.clone()),  // v8+ requires group_id
        })
    }
    
    pub fn handle_find_coordinator(&self, request: chronik_protocol::find_coordinator_types::FindCoordinatorRequest, host: &str, port: i32) -> Result<chronik_protocol::find_coordinator_types::FindCoordinatorResponse> {
        // Create coordinator entry for v4+
        let coordinator = chronik_protocol::find_coordinator_types::Coordinator {
            key: request.key,
            node_id: 1,
            host: host.to_string(),
            port,
            error_code: 0,
            error_message: None,
        };

        Ok(chronik_protocol::find_coordinator_types::FindCoordinatorResponse {
            throttle_time_ms: 0,
            // v0-v3 fields
            error_code: 0,
            error_message: None,
            node_id: 1,
            host: host.to_string(),
            port,
            // v4+ fields
            coordinators: vec![coordinator],
        })
    }
}

/// Join group response with KIP-848 support
#[derive(Debug)]
pub struct JoinGroupResponse {
    pub error_code: i16,
    pub generation_id: i32,
    pub protocol: String,
    pub leader_id: String,
    pub member_id: String,
    pub member_epoch: i32, // KIP-848
    pub members: Vec<MemberInfo>,
}

/// Member info for join group response
#[derive(Debug)]
pub struct MemberInfo {
    pub member_id: String,
    pub metadata: Vec<u8>,
    pub owned_partitions: HashMap<String, Vec<i32>>, // KIP-848
}

/// Sync group response with KIP-848 support
#[derive(Debug)]
pub struct SyncGroupResponse {
    pub error_code: i16,
    pub assignment: Vec<u8>,
    pub member_epoch: i32, // KIP-848
}

/// Heartbeat response
#[derive(Debug)]
pub struct HeartbeatResponse {
    pub error_code: i16,
}

/// Commit offsets response
#[derive(Debug)]
pub struct CommitOffsetsResponse {
    pub error_code: i16,
    pub partition_errors: Vec<PartitionError>,
}

/// Partition error for commit response
#[derive(Debug)]
pub struct PartitionError {
    pub topic: String,
    pub partition: i32,
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

/// Partition assignor trait
#[async_trait]
pub trait PartitionAssignor: Send + Sync {
    /// Assign partitions to members
    fn assign(&self, context: &AssignmentContext) -> Result<HashMap<String, HashMap<String, Vec<i32>>>>;
    
    /// Get the name of this assignor
    fn name(&self) -> &str;
}

/// Cooperative sticky assignor for KIP-848
pub struct CooperativeStickyAssignor {
    sticky_threshold: f64,
}

impl CooperativeStickyAssignor {
    pub fn new() -> Self {
        Self {
            sticky_threshold: 0.8, // Try to keep 80% of assignments stable
        }
    }
}

#[async_trait]
impl PartitionAssignor for CooperativeStickyAssignor {
    fn assign(&self, context: &AssignmentContext) -> Result<HashMap<String, HashMap<String, Vec<i32>>>> {
        let mut assignments: HashMap<String, HashMap<String, Vec<i32>>> = HashMap::new();
        let mut unassigned_partitions: Vec<PartitionInfo> = Vec::new();
        
        // Group partitions by topic
        let mut partitions_by_topic: HashMap<String, Vec<i32>> = HashMap::new();
        for partition in &context.partitions {
            partitions_by_topic.entry(partition.topic.clone())
                .or_insert_with(Vec::new)
                .push(partition.partition);
        }
        
        // Sort partitions for deterministic assignment
        for partitions in partitions_by_topic.values_mut() {
            partitions.sort();
        }
        
        // First pass: preserve existing assignments where possible
        for (member_id, current) in &context.current_assignment {
            if !context.subscriptions.contains_key(member_id) {
                continue; // Member has left
            }
            
            let mut member_assignment = HashMap::new();
            
            for (topic, partitions) in current {
                if let Some(subscribed_topics) = context.subscriptions.get(member_id) {
                    if subscribed_topics.contains(topic) {
                        // Keep assignments for topics the member is still subscribed to
                        let valid_partitions: Vec<i32> = partitions.iter()
                            .filter(|&&p| partitions_by_topic.get(topic)
                                .map(|ps| ps.contains(&p))
                                .unwrap_or(false))
                            .copied()
                            .collect();
                        
                        if !valid_partitions.is_empty() {
                            member_assignment.insert(topic.clone(), valid_partitions);
                        }
                    }
                }
            }
            
            assignments.insert(member_id.clone(), member_assignment);
        }
        
        // Identify unassigned partitions
        for (topic, all_partitions) in &partitions_by_topic {
            let mut assigned: HashSet<i32> = HashSet::new();
            for member_assignments in assignments.values() {
                if let Some(partitions) = member_assignments.get(topic) {
                    assigned.extend(partitions);
                }
            }
            
            for &partition in all_partitions {
                if !assigned.contains(&partition) {
                    unassigned_partitions.push(PartitionInfo {
                        topic: topic.clone(),
                        partition,
                        leader: None,
                    });
                }
            }
        }
        
        // Second pass: assign unassigned partitions
        if !unassigned_partitions.is_empty() {
            // Get members that can receive assignments
            let mut eligible_members: Vec<(String, usize)> = context.subscriptions.iter()
                .map(|(member_id, topics)| {
                    let current_load = assignments.get(member_id)
                        .map(|a| a.values().map(|ps| ps.len()).sum())
                        .unwrap_or(0);
                    (member_id.clone(), current_load)
                })
                .collect();
            
            // Sort by current load (ascending) for load balancing
            eligible_members.sort_by_key(|(_, load)| *load);
            
            // Round-robin assignment of unassigned partitions
            let mut member_idx = 0;
            for partition in unassigned_partitions {
                // Find a member subscribed to this topic
                let start_idx = member_idx;
                loop {
                    let (member_id, _) = &eligible_members[member_idx];
                    
                    if let Some(topics) = context.subscriptions.get(member_id) {
                        if topics.contains(&partition.topic) {
                            // Assign partition to this member
                            assignments.entry(member_id.clone())
                                .or_insert_with(HashMap::new)
                                .entry(partition.topic.clone())
                                .or_insert_with(Vec::new)
                                .push(partition.partition);
                            
                            // Update load
                            eligible_members[member_idx].1 += 1;
                            break;
                        }
                    }
                    
                    member_idx = (member_idx + 1) % eligible_members.len();
                    if member_idx == start_idx {
                        // No eligible member found (shouldn't happen)
                        warn!(
                            topic = %partition.topic,
                            partition = partition.partition,
                            "No eligible member found for partition"
                        );
                        break;
                    }
                }
                
                member_idx = (member_idx + 1) % eligible_members.len();
            }
        }
        
        Ok(assignments)
    }
    
    fn name(&self) -> &str {
        "cooperative-sticky"
    }
}

// Phase 2.3: Assignment encoding/decoding moved to consumer_group/assignment.rs module
// (Removed encode_assignment and decode_assignment functions)

/// Parse subscription metadata from protocol bytes
fn parse_subscription_metadata(bytes: &[u8]) -> Result<Vec<String>> {
    use byteorder::{BigEndian, ReadBytesExt};
    use std::io::Cursor;
    
    let mut cursor = Cursor::new(bytes);
    let mut topics = Vec::new();
    
    // Read version
    let version = cursor.read_i16::<BigEndian>()
        .map_err(|e| Error::Serialization(format!("Failed to read subscription version: {}", e)))?;

    // Support versions 0-3 (Kafka 0.9-3.x)
    // Version 0: topics only
    // Version 1: topics + user_data
    // Version 2: topics + user_data + owned_partitions (we can skip this)
    // Version 3: topics + user_data + owned_partitions + generation_id + rack_id (we can skip these)
    if version < 0 || version > 3 {
        return Err(Error::Protocol(format!("Unsupported subscription version: {}", version)));
    }
    
    // Read topic count
    let topic_count = cursor.read_i32::<BigEndian>()
        .map_err(|e| Error::Serialization(format!("Failed to read topic count: {}", e)))?;
    
    for _ in 0..topic_count {
        // Read topic name
        let topic_len = cursor.read_i16::<BigEndian>()
            .map_err(|e| Error::Serialization(format!("Failed to read topic length: {}", e)))? as usize;
        
        let mut topic_bytes = vec![0u8; topic_len];
        cursor.read_exact(&mut topic_bytes)
            .map_err(|e| Error::Serialization(format!("Failed to read topic name: {}", e)))?;
        
        let topic = String::from_utf8(topic_bytes)
            .map_err(|e| Error::Serialization(format!("Invalid topic name: {}", e)))?;
        
        topics.push(topic);
    }
    
    // Skip user data if present
    if cursor.position() < bytes.len() as u64 {
        let user_data_len = cursor.read_i32::<BigEndian>()
            .unwrap_or(0);
        
        if user_data_len > 0 {
            let mut user_data = vec![0u8; user_data_len as usize];
            let _ = cursor.read_exact(&mut user_data);
        }
    }
    
    Ok(topics)
}

/// Range assignor - assigns partitions in ranges to consumers
pub struct RangeAssignor;

impl RangeAssignor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PartitionAssignor for RangeAssignor {
    fn assign(&self, context: &AssignmentContext) -> Result<HashMap<String, HashMap<String, Vec<i32>>>> {
        let mut assignments: HashMap<String, HashMap<String, Vec<i32>>> = HashMap::new();
        
        // Group partitions by topic
        let mut partitions_by_topic: HashMap<String, Vec<i32>> = HashMap::new();
        for partition in &context.partitions {
            partitions_by_topic.entry(partition.topic.clone())
                .or_insert_with(Vec::new)
                .push(partition.partition);
        }
        
        // Sort partitions for deterministic assignment
        for partitions in partitions_by_topic.values_mut() {
            partitions.sort();
        }
        
        // For each topic, assign partitions in ranges
        for (topic, partitions) in partitions_by_topic {
            // Get members subscribed to this topic
            let mut subscribed_members: Vec<String> = context.subscriptions.iter()
                .filter(|(_, topics)| topics.contains(&topic))
                .map(|(member_id, _)| member_id.clone())
                .collect();
            
            if subscribed_members.is_empty() {
                continue;
            }
            
            // Sort members for deterministic assignment
            subscribed_members.sort();
            
            let partitions_per_member = partitions.len() / subscribed_members.len();
            let remaining_partitions = partitions.len() % subscribed_members.len();
            
            let mut partition_idx = 0;
            for (member_idx, member_id) in subscribed_members.iter().enumerate() {
                let partition_count = if member_idx < remaining_partitions {
                    partitions_per_member + 1
                } else {
                    partitions_per_member
                };
                
                if partition_count > 0 {
                    let member_partitions: Vec<i32> = partitions[partition_idx..partition_idx + partition_count]
                        .to_vec();
                    
                    assignments.entry(member_id.clone())
                        .or_insert_with(HashMap::new)
                        .insert(topic.clone(), member_partitions);
                    
                    partition_idx += partition_count;
                }
            }
        }
        
        Ok(assignments)
    }
    
    fn name(&self) -> &str {
        "range"
    }
}

/// Round-robin assignor - distributes partitions evenly across consumers
pub struct RoundRobinAssignor;

impl RoundRobinAssignor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PartitionAssignor for RoundRobinAssignor {
    fn assign(&self, context: &AssignmentContext) -> Result<HashMap<String, HashMap<String, Vec<i32>>>> {
        let mut assignments: HashMap<String, HashMap<String, Vec<i32>>> = HashMap::new();
        
        // Collect all partitions that need assignment
        let mut all_partitions: Vec<(String, i32)> = Vec::new();
        for partition in &context.partitions {
            all_partitions.push((partition.topic.clone(), partition.partition));
        }
        
        // Sort for deterministic assignment
        all_partitions.sort();
        
        // Get all members and their subscriptions
        let mut eligible_members: Vec<String> = context.subscriptions.keys()
            .cloned()
            .collect();
        eligible_members.sort();
        
        if eligible_members.is_empty() {
            return Ok(assignments);
        }
        
        // Round-robin assignment
        let mut member_idx = 0;
        for (topic, partition) in all_partitions {
            // Find next member subscribed to this topic
            let start_idx = member_idx;
            loop {
                let member_id = &eligible_members[member_idx];
                
                if let Some(topics) = context.subscriptions.get(member_id) {
                    if topics.contains(&topic) {
                        // Assign partition to this member
                        assignments.entry(member_id.clone())
                            .or_insert_with(HashMap::new)
                            .entry(topic.clone())
                            .or_insert_with(Vec::new)
                            .push(partition);
                        break;
                    }
                }
                
                member_idx = (member_idx + 1) % eligible_members.len();
                if member_idx == start_idx {
                    // No eligible member found
                    break;
                }
            }
            
            member_idx = (member_idx + 1) % eligible_members.len();
        }
        
        Ok(assignments)
    }
    
    fn name(&self) -> &str {
        "roundrobin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use chronik_common::metadata::InMemoryMetadataStore;

    #[tokio::test]
    async fn test_consumer_group_lifecycle() {
        let metadata_store = Arc::new(InMemoryMetadataStore::new());
        let manager = Arc::new(GroupManager::new(metadata_store));
        
        // Create group
        let group_id = manager.get_or_create_group("test-group".to_string(), "consumer".to_string()).await.unwrap();
        assert_eq!(group_id, "test-group");
        
        // Join group
        let join_response = manager.join_group(
            "test-group".to_string(),
            None,
            "client1".to_string(),
            "localhost".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(300),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        assert_eq!(join_response.error_code, 0);
        assert!(!join_response.member_id.is_empty());
        assert_eq!(join_response.generation_id, 1);
        
        // Heartbeat
        let heartbeat_response = manager.heartbeat(
            "test-group".to_string(),
            join_response.member_id.clone(),
            join_response.generation_id,
            Some(join_response.member_epoch),
        ).await.unwrap();
        
        assert_eq!(heartbeat_response.error_code, 0);
        
        // Leave group
        manager.leave_group("test-group".to_string(), join_response.member_id).await.unwrap();
    }
    
    #[test]
    fn test_assignment_encoding_decoding() {
        use crate::consumer_group::assignment::{encode_assignment, decode_assignment};

        let mut assignment = HashMap::new();
        assignment.insert("topic1".to_string(), vec![0, 1, 2]);
        assignment.insert("topic2".to_string(), vec![3, 4]);

        let encoded = encode_assignment(&assignment);
        let decoded = decode_assignment(&encoded).unwrap();

        assert_eq!(assignment, decoded);
    }
}