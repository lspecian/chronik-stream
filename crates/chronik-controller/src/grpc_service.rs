//! gRPC service implementation for the controller.

use crate::group_coordinator::{GroupCoordinator, GroupCoordinatorConfig};
use crate::raft::{RaftHandle, Proposal};
use crate::raft::state_machine::{ControllerState, TopicConfig, BrokerInfo};
use chronik_common::{Result, Error};
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

// TODO: Include the generated proto code when protoc is available
// pub mod proto {
//     tonic::include_proto!("chronik.controller");
// }
// 
// use proto::controller_service_server::{ControllerService, ControllerServiceServer};
// use proto::*;

/// Controller gRPC service implementation
pub struct ControllerGrpcService {
    group_coordinator: Arc<GroupCoordinator>,
    raft_handle: RaftHandle,
    state: Arc<RwLock<ControllerState>>,
}

impl ControllerGrpcService {
    /// Create a new gRPC service
    pub fn new(
        raft_handle: RaftHandle,
        state: Arc<RwLock<ControllerState>>,
    ) -> Self {
        let config = GroupCoordinatorConfig::default();
        let group_coordinator = Arc::new(GroupCoordinator::new(
            config,
            raft_handle.clone(),
            state.clone(),
        ));
        
        Self {
            group_coordinator,
            raft_handle,
            state,
        }
    }
    
    // TODO: Create the tonic service when proto is available
    // pub fn into_service(self) -> ControllerServiceServer<Self> {
    //     ControllerServiceServer::new(self)
    // }
}

// TODO: Implement gRPC service when proto is available
/*
#[tonic::async_trait]
impl ControllerService for ControllerGrpcService {
    /// Create a new topic
    async fn create_topic(
        &self,
        request: Request<CreateTopicRequest>,
    ) -> Result<Response<CreateTopicResponse>, Status> {
        let req = request.into_inner();
        
        info!("Creating topic: {}", req.name);
        
        let topic_config = TopicConfig {
            name: req.name.clone(),
            partition_count: req.partition_count,
            replication_factor: req.replication_factor,
            config: req.configs,
            created_at: std::time::SystemTime::now(),
            version: 1,
        };
        
        // Propose through Raft
        match self.raft_handle.propose(Proposal::CreateTopic(topic_config)).await {
            Ok(_) => Ok(Response::new(CreateTopicResponse {
                success: true,
                error: String::new(),
            })),
            Err(e) => Ok(Response::new(CreateTopicResponse {
                success: false,
                error: e.to_string(),
            })),
        }
    }
    
    /// Delete a topic
    async fn delete_topic(
        &self,
        request: Request<DeleteTopicRequest>,
    ) -> Result<Response<DeleteTopicResponse>, Status> {
        let req = request.into_inner();
        
        info!("Deleting topic: {}", req.name);
        
        match self.raft_handle.propose(Proposal::DeleteTopic(req.name)).await {
            Ok(_) => Ok(Response::new(DeleteTopicResponse {
                success: true,
                error: String::new(),
            })),
            Err(e) => Ok(Response::new(DeleteTopicResponse {
                success: false,
                error: e.to_string(),
            })),
        }
    }
    
    /// List all topics
    async fn list_topics(
        &self,
        _request: Request<ListTopicsRequest>,
    ) -> Result<Response<ListTopicsResponse>, Status> {
        let state = self.state.read().await;
        
        let topics: Vec<TopicInfo> = state.topics.values()
            .map(|t| TopicInfo {
                name: t.name.clone(),
                partition_count: t.partition_count,
                replication_factor: t.replication_factor,
                configs: t.config.clone(),
            })
            .collect();
        
        Ok(Response::new(ListTopicsResponse { topics }))
    }
    
    /// Join a consumer group
    async fn join_group(
        &self,
        request: Request<JoinGroupRequest>,
    ) -> Result<Response<JoinGroupResponse>, Status> {
        let req = request.into_inner();
        
        let protocols: Vec<(String, Vec<u8>)> = req.protocols.into_iter()
            .map(|p| (p.name, p.metadata))
            .collect();
        
        match self.group_coordinator.join_group(
            req.group_id,
            if req.member_id.is_empty() { None } else { Some(req.member_id) },
            req.client_id,
            req.client_host,
            req.session_timeout_ms,
            req.rebalance_timeout_ms,
            req.protocol_type,
            protocols,
        ).await {
            Ok(resp) => {
                let members = resp.members.into_iter()
                    .map(|(id, metadata)| GroupMember {
                        member_id: id,
                        metadata,
                    })
                    .collect();
                
                Ok(Response::new(JoinGroupResponse {
                    error_code: resp.error_code as i32,
                    generation_id: resp.generation_id,
                    protocol: resp.protocol,
                    leader_id: resp.leader_id,
                    member_id: resp.member_id,
                    members,
                }))
            }
            Err(e) => {
                warn!("Join group failed: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }
    
    /// Sync group assignments
    async fn sync_group(
        &self,
        request: Request<SyncGroupRequest>,
    ) -> Result<Response<SyncGroupResponse>, Status> {
        let req = request.into_inner();
        
        let assignments = if req.assignments.is_empty() {
            None
        } else {
            Some(req.assignments.into_iter()
                .map(|a| (a.member_id, a.assignment))
                .collect())
        };
        
        match self.group_coordinator.sync_group(
            req.group_id,
            req.generation_id,
            req.member_id,
            assignments,
        ).await {
            Ok(resp) => Ok(Response::new(SyncGroupResponse {
                error_code: resp.error_code as i32,
                assignment: resp.assignment,
            })),
            Err(e) => {
                warn!("Sync group failed: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }
    
    /// Send heartbeat for a group member
    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        
        match self.group_coordinator.heartbeat(
            req.group_id,
            req.member_id,
            req.generation_id,
        ).await {
            Ok(resp) => Ok(Response::new(HeartbeatResponse {
                error_code: resp.error_code as i32,
            })),
            Err(e) => {
                warn!("Heartbeat failed: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }
    
    /// Leave a consumer group
    async fn leave_group(
        &self,
        request: Request<LeaveGroupRequest>,
    ) -> Result<Response<LeaveGroupResponse>, Status> {
        let req = request.into_inner();
        
        match self.group_coordinator.leave_group(req.group_id, req.member_id).await {
            Ok(_) => Ok(Response::new(LeaveGroupResponse {
                error_code: 0,
            })),
            Err(e) => {
                warn!("Leave group failed: {}", e);
                Ok(Response::new(LeaveGroupResponse {
                    error_code: 5, // COORDINATOR_NOT_AVAILABLE
                }))
            }
        }
    }
    
    /// Describe a consumer group
    async fn describe_group(
        &self,
        request: Request<DescribeGroupRequest>,
    ) -> Result<Response<DescribeGroupResponse>, Status> {
        let req = request.into_inner();
        
        match self.group_coordinator.describe_group(req.group_id.clone()).await {
            Ok(Some(group)) => {
                let members = group.members.iter()
                    .map(|member| GroupMemberInfo {
                        member_id: member.member_id.clone(),
                        client_id: member.client_id.clone(),
                        client_host: member.host.clone(),
                        metadata: vec![], // TODO: Encode metadata
                        assignment: vec![], // TODO: Include assignment
                    })
                    .collect();
                
                Ok(Response::new(DescribeGroupResponse {
                    error_code: 0,
                    group_id: group.group_id,
                    state: format!("{:?}", group.state),
                    protocol_type: group.protocol_type,
                    protocol: group.leader.unwrap_or_default(),
                    members,
                }))
            }
            Ok(None) => Ok(Response::new(DescribeGroupResponse {
                error_code: 15, // GROUP_COORDINATOR_NOT_AVAILABLE
                group_id: req.group_id,
                state: String::new(),
                protocol_type: String::new(),
                protocol: String::new(),
                members: vec![],
            })),
            Err(e) => {
                warn!("Describe group failed: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }
    
    /// List all consumer groups
    async fn list_groups(
        &self,
        _request: Request<ListGroupsRequest>,
    ) -> Result<Response<ListGroupsResponse>, Status> {
        match self.group_coordinator.list_groups().await {
            Ok(group_ids) => {
                let state = self.state.read().await;
                let groups = group_ids.into_iter()
                    .filter_map(|id| {
                        state.consumer_groups.get(&id).map(|g| GroupInfo {
                            group_id: g.group_id.clone(),
                            protocol_type: g.protocol_type.clone(),
                            state: format!("{:?}", g.state),
                        })
                    })
                    .collect();
                
                Ok(Response::new(ListGroupsResponse { groups }))
            }
            Err(e) => {
                warn!("List groups failed: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }
    
    /// Commit offsets for a consumer group
    async fn commit_offsets(
        &self,
        request: Request<CommitOffsetsRequest>,
    ) -> Result<Response<CommitOffsetsResponse>, Status> {
        let req = request.into_inner();
        
        let offsets = req.offsets.into_iter()
            .map(|o| crate::group_coordinator::TopicPartitionOffset {
                topic: o.topic,
                partition: o.partition,
                offset: o.offset,
                metadata: if o.metadata.is_empty() { None } else { Some(o.metadata) },
            })
            .collect();
        
        match self.group_coordinator.commit_offsets(
            req.group_id,
            req.generation_id,
            req.member_id,
            offsets,
        ).await {
            Ok(results) => {
                let results = results.into_iter()
                    .map(|r| PartitionOffsetResult {
                        topic: r.topic,
                        partition: r.partition,
                        error_code: r.error_code as i32,
                    })
                    .collect();
                
                Ok(Response::new(CommitOffsetsResponse { results }))
            }
            Err(e) => {
                warn!("Commit offsets failed: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }
    
    /// Fetch committed offsets
    async fn fetch_offsets(
        &self,
        request: Request<FetchOffsetsRequest>,
    ) -> Result<Response<FetchOffsetsResponse>, Status> {
        let req = request.into_inner();
        
        match self.group_coordinator.fetch_offsets(req.group_id, req.topics).await {
            Ok(offsets) => {
                let offsets = offsets.into_iter()
                    .map(|o| TopicPartitionOffset {
                        topic: o.topic,
                        partition: o.partition,
                        offset: o.offset,
                        metadata: o.metadata.unwrap_or_default(),
                    })
                    .collect();
                
                Ok(Response::new(FetchOffsetsResponse { offsets }))
            }
            Err(e) => {
                warn!("Fetch offsets failed: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
    }
    
    /// Register a broker
    async fn register_broker(
        &self,
        request: Request<RegisterBrokerRequest>,
    ) -> Result<Response<RegisterBrokerResponse>, Status> {
        let req = request.into_inner();
        
        info!("Registering broker: {}", req.broker_id);
        
        let broker_info = BrokerInfo {
            id: req.broker_id,
            address: format!("{}:{}", req.host, req.port).parse().unwrap(),
            rack: if req.rack.is_empty() { None } else { Some(req.rack) },
            status: Some("online".to_string()),
            version: Some("1.0.0".to_string()),
            metadata_version: Some(1),
        };
        
        match self.raft_handle.propose(Proposal::RegisterBroker(broker_info)).await {
            Ok(_) => Ok(Response::new(RegisterBrokerResponse {
                success: true,
                error: String::new(),
            })),
            Err(e) => Ok(Response::new(RegisterBrokerResponse {
                success: false,
                error: e.to_string(),
            })),
        }
    }
    
    /// Unregister a broker
    async fn unregister_broker(
        &self,
        request: Request<UnregisterBrokerRequest>,
    ) -> Result<Response<UnregisterBrokerResponse>, Status> {
        let req = request.into_inner();
        
        info!("Unregistering broker: {}", req.broker_id);
        
        match self.raft_handle.propose(Proposal::UnregisterBroker(req.broker_id)).await {
            Ok(_) => Ok(Response::new(UnregisterBrokerResponse {
                success: true,
                error: String::new(),
            })),
            Err(e) => Ok(Response::new(UnregisterBrokerResponse {
                success: false,
                error: e.to_string(),
            })),
        }
    }
    
    /// List all registered brokers
    async fn list_brokers(
        &self,
        _request: Request<ListBrokersRequest>,
    ) -> Result<Response<ListBrokersResponse>, Status> {
        let state = self.state.read().await;
        
        let brokers: Vec<proto::BrokerInfo> = state.brokers.values()
            .map(|b| proto::BrokerInfo {
                broker_id: b.id,
                host: b.address.ip().to_string(),
                port: b.address.port() as i32,
                rack: b.rack.clone().unwrap_or_default(),
            })
            .collect();
        
        Ok(Response::new(ListBrokersResponse { brokers }))
    }
}
*/