//! Enhanced protocol handler that integrates with the existing handler infrastructure.

use std::sync::Arc;
use bytes::{Bytes, BytesMut};
use tokio::sync::RwLock;
use tracing::{debug, info, warn, error};

use chronik_common::{Result, Error};
use kafka_protocol::messages::{
    ApiVersionsResponse, MetadataResponse, MetadataResponseBroker,
    MetadataResponseTopic, MetadataResponsePartition,
    ProduceResponse, ProduceResponseData, FetchResponse,
    ListOffsetsResponse, OffsetCommitResponse, OffsetFetchResponse,
    FindCoordinatorResponse, JoinGroupResponse, HeartbeatResponse,
    LeaveGroupResponse, SyncGroupResponse, DescribeGroupsResponse,
    ListGroupsResponse, CreateTopicsResponse, DeleteTopicsResponse,
    BrokerId, GroupId, TopicName, ResponseHeader,
};
use kafka_protocol::error::ErrorCode;
use kafka_protocol::protocol::StrBytes;

use crate::protocol_v2::{
    ProtocolParser, Request, RequestBody, Response, ResponseBody,
};

/// Handler state for managing protocol operations
#[derive(Clone)]
pub struct HandlerState {
    /// Broker ID
    pub broker_id: i32,
    /// Broker host
    pub host: String,
    /// Broker port
    pub port: i32,
    /// Cluster ID
    pub cluster_id: String,
    /// Topic metadata
    pub topics: Arc<RwLock<Vec<TopicMetadata>>>,
    /// Consumer groups
    pub groups: Arc<RwLock<Vec<ConsumerGroup>>>,
}

/// Topic metadata
#[derive(Clone, Debug)]
pub struct TopicMetadata {
    pub name: String,
    pub partitions: Vec<PartitionMetadata>,
    pub is_internal: bool,
}

/// Partition metadata
#[derive(Clone, Debug)]
pub struct PartitionMetadata {
    pub id: i32,
    pub leader: i32,
    pub replicas: Vec<i32>,
    pub isr: Vec<i32>,
    pub offline_replicas: Vec<i32>,
}

/// Consumer group information
#[derive(Clone, Debug)]
pub struct ConsumerGroup {
    pub id: String,
    pub state: String,
    pub protocol_type: String,
    pub protocol: String,
    pub members: Vec<GroupMember>,
}

/// Group member information
#[derive(Clone, Debug)]
pub struct GroupMember {
    pub member_id: String,
    pub client_id: String,
    pub client_host: String,
    pub metadata: Vec<u8>,
    pub assignment: Vec<u8>,
}

/// Enhanced protocol handler with full API support
pub struct ProtocolHandlerV2 {
    parser: Arc<ProtocolParser>,
    state: HandlerState,
}

impl ProtocolHandlerV2 {
    /// Create a new protocol handler
    pub fn new(state: HandlerState) -> Self {
        Self {
            parser: Arc::new(ProtocolParser::new()),
            state,
        }
    }

    /// Handle a raw request and return a response
    pub async fn handle_request(&self, request_bytes: Bytes) -> Result<Bytes> {
        // Parse the request
        let request = self.parser.parse_request(request_bytes)?;
        
        info!(
            "Handling {} request v{} (correlation_id: {})",
            request.api_key as i16, request.api_version, request.header.correlation_id
        );

        // Route to appropriate handler
        let response_body = match &request.body {
            RequestBody::ApiVersions(req) => {
                self.handle_api_versions(req).await?
            }
            RequestBody::Metadata(req) => {
                self.handle_metadata(req).await?
            }
            RequestBody::Produce(req) => {
                self.handle_produce(req).await?
            }
            RequestBody::Fetch(req) => {
                self.handle_fetch(req).await?
            }
            RequestBody::ListOffsets(req) => {
                self.handle_list_offsets(req).await?
            }
            RequestBody::OffsetCommit(req) => {
                self.handle_offset_commit(req).await?
            }
            RequestBody::OffsetFetch(req) => {
                self.handle_offset_fetch(req).await?
            }
            RequestBody::FindCoordinator(req) => {
                self.handle_find_coordinator(req).await?
            }
            RequestBody::JoinGroup(req) => {
                self.handle_join_group(req).await?
            }
            RequestBody::Heartbeat(req) => {
                self.handle_heartbeat(req).await?
            }
            RequestBody::LeaveGroup(req) => {
                self.handle_leave_group(req).await?
            }
            RequestBody::SyncGroup(req) => {
                self.handle_sync_group(req).await?
            }
            RequestBody::DescribeGroups(req) => {
                self.handle_describe_groups(req).await?
            }
            RequestBody::ListGroups(req) => {
                self.handle_list_groups(req).await?
            }
            RequestBody::CreateTopics(req) => {
                self.handle_create_topics(req).await?
            }
            RequestBody::DeleteTopics(req) => {
                self.handle_delete_topics(req).await?
            }
            _ => {
                warn!("Unsupported request type: {:?}", request.api_key);
                return Err(Error::Protocol(format!("Unsupported API: {:?}", request.api_key)));
            }
        };

        // Create response with header
        let response = Response {
            header: ResponseHeader::default()
                .with_correlation_id(request.header.correlation_id),
            body: response_body,
        };

        // Encode response
        self.parser.encode_response(response, request.api_key, request.api_version)
    }

    /// Handle ApiVersions request
    async fn handle_api_versions(
        &self,
        _req: &kafka_protocol::messages::ApiVersionsRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling ApiVersions request");
        
        let response = ApiVersionsResponse {
            error_code: ErrorCode::None.into(),
            api_keys: self.parser.get_api_versions(),
            throttle_time_ms: Some(0),
            supported_features: None,
            finalized_features_epoch: None,
            finalized_features: None,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::ApiVersions(response))
    }

    /// Handle Metadata request
    async fn handle_metadata(
        &self,
        req: &kafka_protocol::messages::MetadataRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling Metadata request for topics: {:?}", req.topics);
        
        let topics = self.state.topics.read().await;
        
        // Filter topics if specific ones requested
        let response_topics: Vec<MetadataResponseTopic> = if let Some(requested) = &req.topics {
            topics.iter()
                .filter(|t| requested.iter().any(|rt| rt.0 == t.name))
                .map(|t| self.topic_to_metadata_response(t))
                .collect()
        } else {
            topics.iter()
                .map(|t| self.topic_to_metadata_response(t))
                .collect()
        };
        
        let response = MetadataResponse {
            throttle_time_ms: Some(0),
            brokers: vec![MetadataResponseBroker {
                node_id: BrokerId(self.state.broker_id),
                host: StrBytes::from_str(&self.state.host),
                port: self.state.port,
                rack: None,
            }],
            cluster_id: Some(StrBytes::from_str(&self.state.cluster_id)),
            controller_id: Some(BrokerId(self.state.broker_id)),
            topics: response_topics,
            cluster_authorized_operations: None,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::Metadata(response))
    }

    /// Convert internal topic metadata to response format
    fn topic_to_metadata_response(&self, topic: &TopicMetadata) -> MetadataResponseTopic {
        MetadataResponseTopic {
            error_code: ErrorCode::None.into(),
            name: TopicName(StrBytes::from_str(&topic.name)),
            topic_id: None,
            is_internal: Some(topic.is_internal),
            partitions: topic.partitions.iter().map(|p| {
                MetadataResponsePartition {
                    error_code: ErrorCode::None.into(),
                    partition_index: p.id,
                    leader_id: Some(BrokerId(p.leader)),
                    leader_epoch: Some(-1),
                    replica_nodes: p.replicas.iter().map(|&id| BrokerId(id)).collect(),
                    isr_nodes: p.isr.iter().map(|&id| BrokerId(id)).collect(),
                    offline_replicas: Some(p.offline_replicas.iter().map(|&id| BrokerId(id)).collect()),
                    unknown_tagged_fields: Default::default(),
                }
            }).collect(),
            topic_authorized_operations: None,
            unknown_tagged_fields: Default::default(),
        }
    }

    /// Handle Produce request
    async fn handle_produce(
        &self,
        req: &kafka_protocol::messages::ProduceRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling Produce request with {} topics", req.topics.len());
        
        // For now, return success for all partitions
        let responses = req.topics.iter().map(|topic| {
            ProduceResponseData {
                name: topic.name.clone(),
                partitions: topic.partitions.iter().map(|p| {
                    kafka_protocol::messages::PartitionProduceResponse {
                        index: p.index,
                        error_code: ErrorCode::None.into(),
                        base_offset: 0, // TODO: Implement proper offset tracking
                        log_append_time_ms: Some(-1),
                        log_start_offset: Some(0),
                        record_errors: None,
                        error_message: None,
                        unknown_tagged_fields: Default::default(),
                    }
                }).collect(),
                unknown_tagged_fields: Default::default(),
            }
        }).collect();
        
        let response = ProduceResponse {
            responses,
            throttle_time_ms: Some(0),
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::Produce(response))
    }

    /// Handle Fetch request
    async fn handle_fetch(
        &self,
        req: &kafka_protocol::messages::FetchRequest,
    ) -> Result<ResponseBody> {
        debug!(
            "Handling Fetch request for {} topics (replica_id: {})",
            req.topics.len(), req.replica_id
        );
        
        // For now, return empty responses
        let responses = req.topics.iter().map(|topic| {
            kafka_protocol::messages::FetchableTopicResponse {
                topic: topic.topic.clone(),
                topic_id: None,
                partitions: topic.partitions.iter().map(|p| {
                    kafka_protocol::messages::PartitionData {
                        partition: p.partition,
                        error_code: ErrorCode::None.into(),
                        high_watermark: 0,
                        last_stable_offset: Some(0),
                        log_start_offset: Some(0),
                        diverging_epoch: None,
                        current_leader: None,
                        snapshot_id: None,
                        aborted_transactions: None,
                        preferred_read_replica: Some(BrokerId(-1)),
                        records: None, // No records for now
                        unknown_tagged_fields: Default::default(),
                    }
                }).collect(),
                unknown_tagged_fields: Default::default(),
            }
        }).collect();
        
        let response = FetchResponse {
            throttle_time_ms: Some(0),
            error_code: Some(ErrorCode::None.into()),
            session_id: Some(req.session_id),
            responses,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::Fetch(response))
    }

    /// Handle ListOffsets request
    async fn handle_list_offsets(
        &self,
        req: &kafka_protocol::messages::ListOffsetsRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling ListOffsets request for {} topics", req.topics.len());
        
        let topics = req.topics.iter().map(|topic| {
            kafka_protocol::messages::ListOffsetsTopicResponse {
                name: topic.name.clone(),
                partitions: topic.partitions.iter().map(|p| {
                    kafka_protocol::messages::ListOffsetsPartitionResponse {
                        partition_index: p.partition_index,
                        error_code: ErrorCode::None.into(),
                        old_style_offsets: None,
                        timestamp: Some(-1),
                        offset: Some(0),
                        leader_epoch: Some(-1),
                        unknown_tagged_fields: Default::default(),
                    }
                }).collect(),
                unknown_tagged_fields: Default::default(),
            }
        }).collect();
        
        let response = ListOffsetsResponse {
            throttle_time_ms: Some(0),
            topics,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::ListOffsets(response))
    }

    /// Handle OffsetCommit request
    async fn handle_offset_commit(
        &self,
        req: &kafka_protocol::messages::OffsetCommitRequest,
    ) -> Result<ResponseBody> {
        debug!(
            "Handling OffsetCommit request for group {} with {} topics",
            req.group_id.0, req.topics.len()
        );
        
        let topics = req.topics.iter().map(|topic| {
            kafka_protocol::messages::OffsetCommitResponseTopic {
                name: topic.name.clone(),
                partitions: topic.partitions.iter().map(|p| {
                    kafka_protocol::messages::OffsetCommitResponsePartition {
                        partition_index: p.partition_index,
                        error_code: ErrorCode::None.into(),
                        unknown_tagged_fields: Default::default(),
                    }
                }).collect(),
                unknown_tagged_fields: Default::default(),
            }
        }).collect();
        
        let response = OffsetCommitResponse {
            throttle_time_ms: Some(0),
            topics,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::OffsetCommit(response))
    }

    /// Handle OffsetFetch request
    async fn handle_offset_fetch(
        &self,
        req: &kafka_protocol::messages::OffsetFetchRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling OffsetFetch request for group {}", req.group_id.0);
        
        let topics = if let Some(requested_topics) = &req.topics {
            requested_topics.iter().map(|topic| {
                kafka_protocol::messages::OffsetFetchResponseTopic {
                    name: topic.name.clone(),
                    partitions: topic.partition_indexes.iter().map(|&idx| {
                        kafka_protocol::messages::OffsetFetchResponsePartition {
                            partition_index: idx,
                            committed_offset: -1, // No offset committed
                            committed_leader_epoch: Some(-1),
                            metadata: Some(StrBytes::from_str("")),
                            error_code: ErrorCode::None.into(),
                            unknown_tagged_fields: Default::default(),
                        }
                    }).collect(),
                    unknown_tagged_fields: Default::default(),
                }
            }).collect()
        } else {
            vec![]
        };
        
        let response = OffsetFetchResponse {
            throttle_time_ms: Some(0),
            topics,
            error_code: Some(ErrorCode::None.into()),
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::OffsetFetch(response))
    }

    /// Handle FindCoordinator request
    async fn handle_find_coordinator(
        &self,
        req: &kafka_protocol::messages::FindCoordinatorRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling FindCoordinator request for key {}", req.key.0);
        
        let response = FindCoordinatorResponse {
            throttle_time_ms: Some(0),
            error_code: ErrorCode::None.into(),
            error_message: None,
            node_id: Some(BrokerId(self.state.broker_id)),
            host: Some(StrBytes::from_str(&self.state.host)),
            port: Some(self.state.port),
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::FindCoordinator(response))
    }

    /// Handle JoinGroup request
    async fn handle_join_group(
        &self,
        req: &kafka_protocol::messages::JoinGroupRequest,
    ) -> Result<ResponseBody> {
        debug!(
            "Handling JoinGroup request for group {} member {}",
            req.group_id.0, req.member_id.0
        );
        
        // Simple response - make this member the leader
        let response = JoinGroupResponse {
            throttle_time_ms: Some(0),
            error_code: ErrorCode::None.into(),
            generation_id: 1,
            protocol_type: req.protocol_type.clone(),
            protocol_name: req.protocols.first()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| StrBytes::from_str("")),
            leader: req.member_id.clone(),
            member_id: req.member_id.clone(),
            members: vec![],
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::JoinGroup(response))
    }

    /// Handle Heartbeat request
    async fn handle_heartbeat(
        &self,
        req: &kafka_protocol::messages::HeartbeatRequest,
    ) -> Result<ResponseBody> {
        debug!(
            "Handling Heartbeat request for group {} member {}",
            req.group_id.0, req.member_id.0
        );
        
        let response = HeartbeatResponse {
            throttle_time_ms: Some(0),
            error_code: ErrorCode::None.into(),
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::Heartbeat(response))
    }

    /// Handle LeaveGroup request
    async fn handle_leave_group(
        &self,
        req: &kafka_protocol::messages::LeaveGroupRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling LeaveGroup request for group {}", req.group_id.0);
        
        let members = req.members.iter().map(|m| {
            kafka_protocol::messages::MemberResponse {
                member_id: m.member_id.clone(),
                group_instance_id: m.group_instance_id.clone(),
                error_code: ErrorCode::None.into(),
                unknown_tagged_fields: Default::default(),
            }
        }).collect();
        
        let response = LeaveGroupResponse {
            throttle_time_ms: Some(0),
            error_code: ErrorCode::None.into(),
            members: Some(members),
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::LeaveGroup(response))
    }

    /// Handle SyncGroup request
    async fn handle_sync_group(
        &self,
        req: &kafka_protocol::messages::SyncGroupRequest,
    ) -> Result<ResponseBody> {
        debug!(
            "Handling SyncGroup request for group {} member {}",
            req.group_id.0, req.member_id.0
        );
        
        // Return empty assignment for now
        let response = SyncGroupResponse {
            throttle_time_ms: Some(0),
            error_code: ErrorCode::None.into(),
            protocol_type: None,
            protocol_name: None,
            assignment: Default::default(),
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::SyncGroup(response))
    }

    /// Handle DescribeGroups request
    async fn handle_describe_groups(
        &self,
        req: &kafka_protocol::messages::DescribeGroupsRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling DescribeGroups request for {} groups", req.groups.len());
        
        let groups = self.state.groups.read().await;
        
        let described_groups = req.groups.iter().map(|group_id| {
            if let Some(group) = groups.iter().find(|g| g.id == group_id.0) {
                kafka_protocol::messages::DescribedGroup {
                    error_code: ErrorCode::None.into(),
                    group_id: group_id.clone(),
                    group_state: StrBytes::from_str(&group.state),
                    protocol_type: StrBytes::from_str(&group.protocol_type),
                    protocol_data: StrBytes::from_str(&group.protocol),
                    members: group.members.iter().map(|m| {
                        kafka_protocol::messages::DescribedGroupMember {
                            member_id: StrBytes::from_str(&m.member_id),
                            group_instance_id: None,
                            client_id: StrBytes::from_str(&m.client_id),
                            client_host: StrBytes::from_str(&m.client_host),
                            member_metadata: Default::default(),
                            member_assignment: Default::default(),
                            unknown_tagged_fields: Default::default(),
                        }
                    }).collect(),
                    authorized_operations: None,
                    unknown_tagged_fields: Default::default(),
                }
            } else {
                kafka_protocol::messages::DescribedGroup {
                    error_code: ErrorCode::GroupIdNotFound.into(),
                    group_id: group_id.clone(),
                    group_state: StrBytes::from_str("Dead"),
                    protocol_type: StrBytes::from_str(""),
                    protocol_data: StrBytes::from_str(""),
                    members: vec![],
                    authorized_operations: None,
                    unknown_tagged_fields: Default::default(),
                }
            }
        }).collect();
        
        let response = DescribeGroupsResponse {
            throttle_time_ms: Some(0),
            groups: described_groups,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::DescribeGroups(response))
    }

    /// Handle ListGroups request
    async fn handle_list_groups(
        &self,
        _req: &kafka_protocol::messages::ListGroupsRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling ListGroups request");
        
        let groups = self.state.groups.read().await;
        
        let listed_groups = groups.iter().map(|g| {
            kafka_protocol::messages::ListedGroup {
                group_id: GroupId(StrBytes::from_str(&g.id)),
                protocol_type: StrBytes::from_str(&g.protocol_type),
                group_state: Some(StrBytes::from_str(&g.state)),
                unknown_tagged_fields: Default::default(),
            }
        }).collect();
        
        let response = ListGroupsResponse {
            throttle_time_ms: Some(0),
            error_code: ErrorCode::None.into(),
            groups: listed_groups,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::ListGroups(response))
    }

    /// Handle CreateTopics request
    async fn handle_create_topics(
        &self,
        req: &kafka_protocol::messages::CreateTopicsRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling CreateTopics request for {} topics", req.topics.len());
        
        let mut topics = self.state.topics.write().await;
        
        let topic_results = req.topics.iter().map(|topic| {
            // Check if topic already exists
            if topics.iter().any(|t| t.name == topic.name.0) {
                kafka_protocol::messages::CreatableTopicResult {
                    name: topic.name.clone(),
                    topic_id: None,
                    error_code: ErrorCode::TopicAlreadyExists.into(),
                    error_message: Some(StrBytes::from_str("Topic already exists")),
                    num_partitions: None,
                    replication_factor: None,
                    configs: None,
                    unknown_tagged_fields: Default::default(),
                }
            } else {
                // Create topic
                let num_partitions = topic.num_partitions.unwrap_or(1);
                let replication_factor = topic.replication_factor.unwrap_or(1);
                
                let mut partitions = Vec::new();
                for i in 0..num_partitions {
                    partitions.push(PartitionMetadata {
                        id: i,
                        leader: self.state.broker_id,
                        replicas: vec![self.state.broker_id],
                        isr: vec![self.state.broker_id],
                        offline_replicas: vec![],
                    });
                }
                
                topics.push(TopicMetadata {
                    name: topic.name.0.clone(),
                    partitions,
                    is_internal: false,
                });
                
                kafka_protocol::messages::CreatableTopicResult {
                    name: topic.name.clone(),
                    topic_id: None,
                    error_code: ErrorCode::None.into(),
                    error_message: None,
                    num_partitions: Some(num_partitions),
                    replication_factor: Some(replication_factor),
                    configs: None,
                    unknown_tagged_fields: Default::default(),
                }
            }
        }).collect();
        
        let response = CreateTopicsResponse {
            throttle_time_ms: Some(0),
            topics: topic_results,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::CreateTopics(response))
    }

    /// Handle DeleteTopics request
    async fn handle_delete_topics(
        &self,
        req: &kafka_protocol::messages::DeleteTopicsRequest,
    ) -> Result<ResponseBody> {
        debug!("Handling DeleteTopics request for {} topics", req.topic_names.len());
        
        let mut topics = self.state.topics.write().await;
        
        let responses = req.topic_names.iter().map(|topic_name| {
            if let Some(pos) = topics.iter().position(|t| t.name == topic_name.0) {
                topics.remove(pos);
                kafka_protocol::messages::DeletableTopicResult {
                    name: Some(topic_name.clone()),
                    topic_id: None,
                    error_code: ErrorCode::None.into(),
                    error_message: None,
                    unknown_tagged_fields: Default::default(),
                }
            } else {
                kafka_protocol::messages::DeletableTopicResult {
                    name: Some(topic_name.clone()),
                    topic_id: None,
                    error_code: ErrorCode::UnknownTopicOrPartition.into(),
                    error_message: Some(StrBytes::from_str("Topic not found")),
                    unknown_tagged_fields: Default::default(),
                }
            }
        }).collect();
        
        let response = DeleteTopicsResponse {
            throttle_time_ms: Some(0),
            responses,
            unknown_tagged_fields: Default::default(),
        };
        
        Ok(ResponseBody::DeleteTopics(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use kafka_protocol::messages::RequestHeader;
    use kafka_protocol::protocol::Encodable;

    #[tokio::test]
    async fn test_handler_api_versions() {
        let state = HandlerState {
            broker_id: 1,
            host: "localhost".to_string(),
            port: 9092,
            cluster_id: "test-cluster".to_string(),
            topics: Arc::new(RwLock::new(vec![])),
            groups: Arc::new(RwLock::new(vec![])),
        };
        
        let handler = ProtocolHandlerV2::new(state);
        
        // Create ApiVersions request
        let mut buf = BytesMut::new();
        
        // Request header
        let header = RequestHeader::default()
            .with_request_api_key(18) // ApiVersions
            .with_request_api_version(3)
            .with_correlation_id(123)
            .with_client_id(Some(StrBytes::from_str("test-client")));
        
        header.encode(&mut buf, 2).unwrap();
        
        // ApiVersions request body
        let req = kafka_protocol::messages::ApiVersionsRequest::default()
            .with_client_software_name(StrBytes::from_str("test"))
            .with_client_software_version(StrBytes::from_str("1.0"));
        
        req.encode(&mut buf, 3).unwrap();
        
        let response = handler.handle_request(buf.freeze()).await.unwrap();
        assert!(!response.is_empty());
    }
}