//! Controller-based consumer group manager for ingest nodes.
//! 
//! This replaces the local GroupManager with a controller-coordinated implementation
//! that ensures all group state is managed through Raft consensus.

use crate::controller_client::{ControllerClient, JoinGroupResult, SyncGroupResult, HeartbeatResult, 
                               TopicPartitionOffsetData, PartitionOffsetCommitResult};
use crate::consumer_group::{JoinGroupResponse, SyncGroupResponse, HeartbeatResponse, 
                            CommitOffsetsResponse, PartitionError, TopicPartitionOffset,
                            MemberInfo};
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn, error};

/// Controller-based group manager
pub struct ControllerGroupManager {
    controller_client: Arc<Mutex<ControllerClient>>,
    metadata_store: Arc<dyn MetadataStore>,
}

impl ControllerGroupManager {
    /// Create a new controller-based group manager
    pub async fn new(
        controller_addr: String,
        metadata_store: Arc<dyn MetadataStore>,
    ) -> Result<Self> {
        let client = ControllerClient::new(controller_addr).await?;
        
        Ok(Self {
            controller_client: Arc::new(Mutex::new(client)),
            metadata_store,
        })
    }
    
    /// Join a consumer group
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
        _static_member_id: Option<String>, // TODO: Support static membership
    ) -> Result<JoinGroupResponse> {
        let mut client = self.controller_client.lock().await;
        
        let result = client.join_group(
            group_id,
            member_id,
            client_id,
            client_host,
            session_timeout,
            rebalance_timeout,
            protocol_type,
            protocols,
        ).await?;
        
        let members = result.members.into_iter()
            .map(|(member_id, metadata)| MemberInfo {
                member_id,
                metadata,
                owned_partitions: Default::default(), // TODO: Handle KIP-848 owned partitions
            })
            .collect();
        
        Ok(JoinGroupResponse {
            error_code: result.error_code,
            generation_id: result.generation_id,
            protocol: result.protocol,
            leader_id: result.leader_id,
            member_id: result.member_id,
            member_epoch: 0, // TODO: Handle KIP-848 member epoch
            members,
        })
    }
    
    /// Sync group state
    pub async fn sync_group(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        _member_epoch: i32, // TODO: Handle KIP-848 member epoch
        assignments: Option<Vec<(String, Vec<u8>)>>,
    ) -> Result<SyncGroupResponse> {
        let mut client = self.controller_client.lock().await;
        
        let result = client.sync_group(
            group_id,
            generation_id,
            member_id,
            assignments,
        ).await?;
        
        Ok(SyncGroupResponse {
            error_code: result.error_code,
            assignment: result.assignment,
            member_epoch: 0, // TODO: Handle KIP-848 member epoch
        })
    }
    
    /// Send heartbeat
    pub async fn heartbeat(
        &self,
        group_id: String,
        member_id: String,
        generation_id: i32,
        _member_epoch: Option<i32>, // TODO: Handle KIP-848 member epoch
    ) -> Result<HeartbeatResponse> {
        let mut client = self.controller_client.lock().await;
        
        let result = client.heartbeat(
            group_id,
            member_id,
            generation_id,
        ).await?;
        
        Ok(HeartbeatResponse {
            error_code: result.error_code,
        })
    }
    
    /// Leave group
    pub async fn leave_group(
        &self,
        group_id: String,
        member_id: String,
    ) -> Result<()> {
        let mut client = self.controller_client.lock().await;
        
        client.leave_group(group_id, member_id).await?;
        
        Ok(())
    }
    
    /// Commit offsets
    pub async fn commit_offsets(
        &self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        _member_epoch: Option<i32>, // TODO: Handle KIP-848 member epoch
        offsets: Vec<TopicPartitionOffset>,
    ) -> Result<CommitOffsetsResponse> {
        let mut client = self.controller_client.lock().await;
        
        let offset_data = offsets.into_iter()
            .map(|o| TopicPartitionOffsetData {
                topic: o.topic,
                partition: o.partition,
                offset: o.offset,
                metadata: o.metadata,
            })
            .collect();
        
        let results = client.commit_offsets(
            group_id,
            generation_id,
            member_id,
            offset_data,
        ).await?;
        
        let partition_errors = results.into_iter()
            .map(|r| PartitionError {
                topic: r.topic,
                partition: r.partition,
                error_code: r.error_code,
            })
            .collect();
        
        Ok(CommitOffsetsResponse {
            error_code: 0,
            partition_errors,
        })
    }
    
    /// Fetch committed offsets
    pub async fn fetch_offsets(
        &self,
        group_id: String,
        topics: Vec<String>,
    ) -> Result<Vec<TopicPartitionOffset>> {
        let mut client = self.controller_client.lock().await;
        
        let offset_data = client.fetch_offsets(group_id, topics).await?;
        
        Ok(offset_data.into_iter()
            .map(|o| TopicPartitionOffset {
                topic: o.topic,
                partition: o.partition,
                offset: o.offset,
                metadata: o.metadata,
            })
            .collect())
    }
    
    /// Describe a consumer group
    pub async fn describe_group(&self, group_id: String) -> Result<Option<crate::consumer_group::ConsumerGroup>> {
        let mut client = self.controller_client.lock().await;
        
        match client.describe_group(group_id.clone()).await? {
            Some(desc) => {
                // Convert from controller description to local ConsumerGroup
                // This is a simplified conversion - in production you'd want full mapping
                let mut group = crate::consumer_group::ConsumerGroup::new(
                    desc.group_id,
                    desc.protocol_type,
                );
                
                // Parse state
                if let Some(state) = crate::consumer_group::GroupState::from_str(&desc.state) {
                    group.state = state;
                }
                
                Ok(Some(group))
            }
            None => Ok(None),
        }
    }
    
    /// List all consumer groups
    pub async fn list_groups(&self) -> Result<Vec<String>> {
        let mut client = self.controller_client.lock().await;
        
        let groups = client.list_groups().await?;
        
        Ok(groups.into_iter()
            .map(|g| g.group_id)
            .collect())
    }
    
    /// Get or create a consumer group
    pub async fn get_or_create_group(&self, group_id: String, protocol_type: String) -> Result<String> {
        // With controller-based coordination, groups are created on first join
        // So we just return the group_id
        Ok(group_id)
    }
    
    /// Start expiration checker (no-op for controller-based implementation)
    pub fn start_expiration_checker(self: Arc<Self>) {
        // The controller handles member expiration, so this is a no-op
        info!("Controller-based group manager does not need local expiration checker");
    }
}