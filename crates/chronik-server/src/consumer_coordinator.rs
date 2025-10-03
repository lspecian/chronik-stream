use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Consumer group member information
#[derive(Debug, Clone)]
pub struct GroupMember {
    pub member_id: String,
    pub client_id: String,
    pub client_host: String,
    pub session_timeout: i32,
    pub rebalance_timeout: i32,
    pub protocols: Vec<GroupProtocol>,
    pub assignments: Vec<u8>,
    pub heartbeat_timestamp: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct GroupProtocol {
    pub name: String,
    pub metadata: Vec<u8>,
}

/// Consumer group state
#[derive(Debug, Clone, PartialEq)]
pub enum GroupState {
    Empty,
    PreparingRebalance,
    CompletingRebalance,
    Stable,
    Dead,
}

/// Consumer group information
#[derive(Debug, Clone)]
pub struct ConsumerGroup {
    pub group_id: String,
    pub state: GroupState,
    pub generation_id: i32,
    pub leader_id: Option<String>,
    pub protocol_type: Option<String>,
    pub protocol: Option<String>,
    pub members: HashMap<String, GroupMember>,
}

impl ConsumerGroup {
    pub fn new(group_id: String) -> Self {
        Self {
            group_id,
            state: GroupState::Empty,
            generation_id: 0,
            leader_id: None,
            protocol_type: None,
            protocol: None,
            members: HashMap::new(),
        }
    }
}

/// Consumer coordinator manages consumer groups
pub struct ConsumerCoordinator {
    /// Map of group_id to ConsumerGroup
    groups: Arc<RwLock<HashMap<String, ConsumerGroup>>>,
    /// Node ID of this broker
    node_id: i32,
}

impl ConsumerCoordinator {
    pub fn new(node_id: i32) -> Self {
        info!("Initializing consumer coordinator for node {}", node_id);
        Self {
            groups: Arc::new(RwLock::new(HashMap::new())),
            node_id,
        }
    }

    /// Check if this broker is the coordinator for a given group
    pub async fn is_coordinator_for(&self, group_id: &str) -> bool {
        // For now, this broker handles all groups
        // In a real implementation, this would use consistent hashing
        true
    }

    /// Get or create a consumer group
    pub async fn get_or_create_group(&self, group_id: String) -> ConsumerGroup {
        let mut groups = self.groups.write().await;
        groups.entry(group_id.clone())
            .or_insert_with(|| {
                info!("Creating new consumer group: {}", group_id);
                ConsumerGroup::new(group_id.clone())
            })
            .clone()
    }

    /// Join a consumer group
    pub async fn join_group(
        &self,
        group_id: String,
        member_id: Option<String>,
        client_id: String,
        client_host: String,
        protocol_type: String,
        protocols: Vec<GroupProtocol>,
        session_timeout: i32,
        rebalance_timeout: i32,
    ) -> Result<(String, i32, String, Option<String>, Vec<u8>), String> {
        let mut groups = self.groups.write().await;
        let group = groups.entry(group_id.clone())
            .or_insert_with(|| ConsumerGroup::new(group_id.clone()));

        // Generate member ID if not provided
        let member_id = member_id.unwrap_or_else(|| {
            format!("{}-{}-{}", client_id, uuid::Uuid::new_v4(), std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis())
        });

        debug!("Member {} joining group {}", member_id, group_id);

        // Add member to group
        let member = GroupMember {
            member_id: member_id.clone(),
            client_id,
            client_host,
            session_timeout,
            rebalance_timeout,
            protocols,
            assignments: Vec::new(),
            heartbeat_timestamp: std::time::Instant::now(),
        };

        group.members.insert(member_id.clone(), member);

        // Update group state
        if group.state == GroupState::Empty {
            group.state = GroupState::PreparingRebalance;
            group.generation_id += 1;
            group.protocol_type = Some(protocol_type);
            group.leader_id = Some(member_id.clone());
        }

        // Return join response
        let generation_id = group.generation_id;
        let protocol = group.protocol.clone().unwrap_or_else(|| "roundrobin".to_string());
        let leader_id = group.leader_id.clone();
        let member_assignment = if Some(&member_id) == leader_id.as_ref() {
            // Leader gets all member metadata
            let mut members_metadata = Vec::new();
            for (mid, member) in &group.members {
                members_metadata.extend_from_slice(mid.as_bytes());
                // Add member metadata here
            }
            members_metadata
        } else {
            Vec::new()
        };

        Ok((member_id, generation_id, protocol, leader_id, member_assignment))
    }

    /// Sync group (complete rebalance)
    pub async fn sync_group(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        assignments: HashMap<String, Vec<u8>>,
    ) -> Result<Vec<u8>, String> {
        let mut groups = self.groups.write().await;
        let group = groups.get_mut(&group_id)
            .ok_or_else(|| "Group not found".to_string())?;

        // Verify generation
        if group.generation_id != generation_id {
            return Err("Invalid generation".to_string());
        }

        // Store assignments
        for (mid, assignment) in assignments {
            if let Some(member) = group.members.get_mut(&mid) {
                member.assignments = assignment;
            }
        }

        // Update group state
        group.state = GroupState::Stable;

        // Return member's assignment
        group.members.get(&member_id)
            .map(|m| m.assignments.clone())
            .ok_or_else(|| "Member not found".to_string())
    }

    /// Handle heartbeat
    pub async fn heartbeat(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
    ) -> Result<(), String> {
        let mut groups = self.groups.write().await;
        let group = groups.get_mut(&group_id)
            .ok_or_else(|| "Group not found".to_string())?;

        // Verify generation
        if group.generation_id != generation_id {
            return Err("Invalid generation".to_string());
        }

        // Update heartbeat timestamp
        if let Some(member) = group.members.get_mut(&member_id) {
            member.heartbeat_timestamp = std::time::Instant::now();
            Ok(())
        } else {
            Err("Member not found".to_string())
        }
    }

    /// Leave group
    pub async fn leave_group(
        &self,
        group_id: String,
        member_id: String,
    ) -> Result<(), String> {
        let mut groups = self.groups.write().await;
        if let Some(group) = groups.get_mut(&group_id) {
            group.members.remove(&member_id);

            // Update group state if empty
            if group.members.is_empty() {
                group.state = GroupState::Empty;
                group.generation_id = 0;
                group.leader_id = None;
            } else {
                // Trigger rebalance
                group.state = GroupState::PreparingRebalance;
                group.generation_id += 1;
            }
            Ok(())
        } else {
            Err("Group not found".to_string())
        }
    }

    /// Get coordinator info for debugging
    pub async fn get_info(&self) -> String {
        let groups = self.groups.read().await;
        format!("Consumer Coordinator - Node: {}, Groups: {}", self.node_id, groups.len())
    }
}