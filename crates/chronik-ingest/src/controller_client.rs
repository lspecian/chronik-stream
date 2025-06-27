//! Client for communicating with the controller service.

use chronik_common::{Result, Error};
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use tracing::{info, warn, error};

// Include the generated proto code from controller
pub mod proto {
    tonic::include_proto!("chronik.controller");
}

use proto::controller_service_client::ControllerServiceClient;
use proto::*;

/// Controller client for ingest nodes
#[derive(Clone)]
pub struct ControllerClient {
    client: ControllerServiceClient<Channel>,
    controller_addr: String,
}

impl ControllerClient {
    /// Create a new controller client
    pub async fn new(controller_addr: String) -> Result<Self> {
        info!("Connecting to controller at {}", controller_addr);
        
        let endpoint = Endpoint::from_shared(controller_addr.clone())
            .map_err(|e| Error::Network(format!("Invalid controller address: {}", e)))?
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(10));
        
        let channel = endpoint.connect().await
            .map_err(|e| Error::Network(format!("Failed to connect to controller: {}", e)))?;
        
        let client = ControllerServiceClient::new(channel);
        
        Ok(Self {
            client,
            controller_addr,
        })
    }
    
    /// Join a consumer group
    pub async fn join_group(
        &mut self,
        group_id: String,
        member_id: Option<String>,
        client_id: String,
        client_host: String,
        session_timeout: Duration,
        rebalance_timeout: Duration,
        protocol_type: String,
        protocols: Vec<(String, Vec<u8>)>,
    ) -> Result<JoinGroupResult> {
        let request = Request::new(JoinGroupRequest {
            group_id,
            member_id: member_id.unwrap_or_default(),
            client_id,
            client_host,
            session_timeout_ms: session_timeout.as_millis() as i32,
            rebalance_timeout_ms: rebalance_timeout.as_millis() as i32,
            protocol_type,
            protocols: protocols.into_iter()
                .map(|(name, metadata)| GroupProtocol { name, metadata })
                .collect(),
        });
        
        let response = self.client.join_group(request).await
            .map_err(|e| Error::Network(format!("Join group RPC failed: {}", e)))?
            .into_inner();
        
        let members = response.members.into_iter()
            .map(|m| (m.member_id, m.metadata))
            .collect();
        
        Ok(JoinGroupResult {
            error_code: response.error_code as i16,
            generation_id: response.generation_id,
            protocol: response.protocol,
            leader_id: response.leader_id,
            member_id: response.member_id,
            members,
        })
    }
    
    /// Sync group assignments
    pub async fn sync_group(
        &mut self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        assignments: Option<Vec<(String, Vec<u8>)>>,
    ) -> Result<SyncGroupResult> {
        let request = Request::new(SyncGroupRequest {
            group_id,
            generation_id,
            member_id,
            assignments: assignments.unwrap_or_default().into_iter()
                .map(|(member_id, assignment)| GroupAssignment { member_id, assignment })
                .collect(),
        });
        
        let response = self.client.sync_group(request).await
            .map_err(|e| Error::Network(format!("Sync group RPC failed: {}", e)))?
            .into_inner();
        
        Ok(SyncGroupResult {
            error_code: response.error_code as i16,
            assignment: response.assignment,
        })
    }
    
    /// Send heartbeat
    pub async fn heartbeat(
        &mut self,
        group_id: String,
        member_id: String,
        generation_id: i32,
    ) -> Result<HeartbeatResult> {
        let request = Request::new(HeartbeatRequest {
            group_id,
            member_id,
            generation_id,
        });
        
        let response = self.client.heartbeat(request).await
            .map_err(|e| Error::Network(format!("Heartbeat RPC failed: {}", e)))?
            .into_inner();
        
        Ok(HeartbeatResult {
            error_code: response.error_code as i16,
        })
    }
    
    /// Leave group
    pub async fn leave_group(
        &mut self,
        group_id: String,
        member_id: String,
    ) -> Result<()> {
        let request = Request::new(LeaveGroupRequest {
            group_id,
            member_id,
        });
        
        let response = self.client.leave_group(request).await
            .map_err(|e| Error::Network(format!("Leave group RPC failed: {}", e)))?
            .into_inner();
        
        if response.error_code != 0 {
            warn!("Leave group returned error code: {}", response.error_code);
        }
        
        Ok(())
    }
    
    /// Commit offsets
    pub async fn commit_offsets(
        &mut self,
        group_id: String,
        generation_id: i32,
        member_id: String,
        offsets: Vec<TopicPartitionOffsetData>,
    ) -> Result<Vec<PartitionOffsetCommitResult>> {
        let request = Request::new(CommitOffsetsRequest {
            group_id,
            generation_id,
            member_id,
            offsets: offsets.into_iter()
                .map(|o| TopicPartitionOffset {
                    topic: o.topic,
                    partition: o.partition,
                    offset: o.offset,
                    metadata: o.metadata.unwrap_or_default(),
                })
                .collect(),
        });
        
        let response = self.client.commit_offsets(request).await
            .map_err(|e| Error::Network(format!("Commit offsets RPC failed: {}", e)))?
            .into_inner();
        
        Ok(response.results.into_iter()
            .map(|r| PartitionOffsetCommitResult {
                topic: r.topic,
                partition: r.partition,
                error_code: r.error_code as i16,
            })
            .collect())
    }
    
    /// Fetch offsets
    pub async fn fetch_offsets(
        &mut self,
        group_id: String,
        topics: Vec<String>,
    ) -> Result<Vec<TopicPartitionOffsetData>> {
        let request = Request::new(FetchOffsetsRequest {
            group_id,
            topics,
        });
        
        let response = self.client.fetch_offsets(request).await
            .map_err(|e| Error::Network(format!("Fetch offsets RPC failed: {}", e)))?
            .into_inner();
        
        Ok(response.offsets.into_iter()
            .map(|o| TopicPartitionOffsetData {
                topic: o.topic,
                partition: o.partition,
                offset: o.offset,
                metadata: if o.metadata.is_empty() { None } else { Some(o.metadata) },
            })
            .collect())
    }
    
    /// List consumer groups
    pub async fn list_groups(&mut self) -> Result<Vec<GroupListInfo>> {
        let request = Request::new(ListGroupsRequest {});
        
        let response = self.client.list_groups(request).await
            .map_err(|e| Error::Network(format!("List groups RPC failed: {}", e)))?
            .into_inner();
        
        Ok(response.groups.into_iter()
            .map(|g| GroupListInfo {
                group_id: g.group_id,
                protocol_type: g.protocol_type,
                state: g.state,
            })
            .collect())
    }
    
    /// Describe a consumer group
    pub async fn describe_group(&mut self, group_id: String) -> Result<Option<GroupDescription>> {
        let request = Request::new(DescribeGroupRequest { group_id });
        
        let response = self.client.describe_group(request).await
            .map_err(|e| Error::Network(format!("Describe group RPC failed: {}", e)))?
            .into_inner();
        
        if response.error_code != 0 {
            return Ok(None);
        }
        
        Ok(Some(GroupDescription {
            group_id: response.group_id,
            state: response.state,
            protocol_type: response.protocol_type,
            protocol: response.protocol,
            members: response.members.into_iter()
                .map(|m| GroupMemberDescription {
                    member_id: m.member_id,
                    client_id: m.client_id,
                    client_host: m.client_host,
                    metadata: m.metadata,
                    assignment: m.assignment,
                })
                .collect(),
        }))
    }
}

/// Join group result
#[derive(Debug)]
pub struct JoinGroupResult {
    pub error_code: i16,
    pub generation_id: i32,
    pub protocol: String,
    pub leader_id: String,
    pub member_id: String,
    pub members: Vec<(String, Vec<u8>)>,
}

/// Sync group result
#[derive(Debug)]
pub struct SyncGroupResult {
    pub error_code: i16,
    pub assignment: Vec<u8>,
}

/// Heartbeat result
#[derive(Debug)]
pub struct HeartbeatResult {
    pub error_code: i16,
}

/// Topic partition offset data
#[derive(Debug)]
pub struct TopicPartitionOffsetData {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub metadata: Option<String>,
}

/// Partition offset commit result
#[derive(Debug)]
pub struct PartitionOffsetCommitResult {
    pub topic: String,
    pub partition: i32,
    pub error_code: i16,
}

/// Group list info
#[derive(Debug)]
pub struct GroupListInfo {
    pub group_id: String,
    pub protocol_type: String,
    pub state: String,
}

/// Group description
#[derive(Debug)]
pub struct GroupDescription {
    pub group_id: String,
    pub state: String,
    pub protocol_type: String,
    pub protocol: String,
    pub members: Vec<GroupMemberDescription>,
}

/// Group member description
#[derive(Debug)]
pub struct GroupMemberDescription {
    pub member_id: String,
    pub client_id: String,
    pub client_host: String,
    pub metadata: Vec<u8>,
    pub assignment: Vec<u8>,
}