//! Request handler for Kafka protocol requests.

use crate::indexer::IndexRecord;
use crate::consumer_group::GroupManager;
use crate::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use chronik_common::Result;
use chronik_protocol::{
    Request, Response, RequestBody,
    ProduceRequest, ProduceResponse, ProduceResponseTopic, ProduceResponsePartition,
    FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition,
    MetadataRequest, MetadataResponse, MetadataBroker, MetadataTopic, MetadataPartition,
    JoinGroupRequest, JoinGroupResponse, JoinGroupMember,
    SyncGroupRequest, SyncGroupResponse,
    HeartbeatRequest, HeartbeatResponse,
    LeaveGroupRequest, LeaveGroupResponse,
    OffsetCommitRequest, OffsetCommitResponse, OffsetCommitResponseTopic, OffsetCommitResponsePartition,
    OffsetFetchRequest, OffsetFetchResponse, OffsetFetchResponseTopic, OffsetFetchResponsePartition,
    ApiVersionsRequest, ApiVersionsResponse, ApiVersionsResponseKey,
    parser::ResponseHeader,
};
use chronik_storage::{SegmentReader, object_store::storage::ObjectStore};
use chronik_common::metadata::traits::MetadataStore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};

/// Handler configuration
#[derive(Debug, Clone)]
pub struct HandlerConfig {
    /// Node ID
    pub node_id: i32,
    /// Listen host
    pub host: String,
    /// Listen port
    pub port: i32,
    /// Controller addresses
    pub controller_addrs: Vec<String>,
}

/// Request handler state
struct HandlerState {
    /// Topic metadata cache
    topic_metadata: HashMap<String, TopicMetadata>,
    /// Producer state by client
    producer_state: HashMap<String, ProducerState>,
}

/// Topic metadata
#[derive(Debug, Clone)]
struct TopicMetadata {
    partition_count: i32,
    replication_factor: i32,
    partition_leaders: HashMap<i32, i32>,
}

/// Producer state
#[derive(Debug)]
struct ProducerState {
    client_id: String,
    last_sequence: HashMap<(String, i32), i32>,
}

/// Request handler
pub struct RequestHandler {
    config: HandlerConfig,
    state: Arc<RwLock<HandlerState>>,
    produce_handler: Arc<ProduceHandler>,
    segment_reader: Arc<SegmentReader>,
    group_manager: Arc<GroupManager>,
}

impl RequestHandler {
    /// Create a new request handler
    pub async fn new(
        config: HandlerConfig,
        storage: Arc<dyn ObjectStore>,
        metadata_store: Arc<dyn MetadataStore>,
        segment_reader: Arc<SegmentReader>,
    ) -> Result<Self> {
        let state = Arc::new(RwLock::new(HandlerState {
            topic_metadata: HashMap::new(),
            producer_state: HashMap::new(),
        }));
        
        let group_manager = Arc::new(GroupManager::new());
        
        // Create produce handler configuration
        let produce_config = ProduceHandlerConfig {
            node_id: config.node_id,
            enable_indexing: true,
            enable_idempotence: true,
            enable_transactions: true,
            ..Default::default()
        };
        
        // Create produce handler
        let produce_handler = Arc::new(
            ProduceHandler::new(produce_config, storage, metadata_store).await?
        );
        
        Ok(Self {
            config,
            state,
            produce_handler,
            segment_reader,
            group_manager,
        })
    }
    
    /// Handle a request
    pub async fn handle_request(&self, request: Request) -> Result<Response> {
        let correlation_id = request.header.correlation_id;
        
        match request.body {
            RequestBody::Produce(produce) => self.handle_produce(produce, correlation_id).await,
            RequestBody::Fetch(fetch) => self.handle_fetch(fetch, correlation_id).await,
            RequestBody::Metadata(metadata) => self.handle_metadata(metadata, correlation_id).await,
            RequestBody::JoinGroup(join) => self.handle_join_group(join, correlation_id).await,
            RequestBody::SyncGroup(sync) => self.handle_sync_group(sync, correlation_id).await,
            RequestBody::Heartbeat(heartbeat) => self.handle_heartbeat(heartbeat, correlation_id).await,
            RequestBody::LeaveGroup(leave) => self.handle_leave_group(leave, correlation_id).await,
            RequestBody::OffsetCommit(commit) => self.handle_offset_commit(commit, correlation_id).await,
            RequestBody::OffsetFetch(fetch) => self.handle_offset_fetch(fetch, correlation_id).await,
            RequestBody::ApiVersions(api_versions) => self.handle_api_versions(api_versions, correlation_id).await,
        }
    }
    
    /// Handle produce request
    async fn handle_produce(&self, request: ProduceRequest, correlation_id: i32) -> Result<Response> {
        // Delegate to the produce handler
        let response = self.produce_handler.handle_produce(request, correlation_id).await?;
        Ok(Response::Produce(response))
    }
    
    /// Handle fetch request
    async fn handle_fetch(&self, request: FetchRequest, correlation_id: i32) -> Result<Response> {
        let mut response_topics = Vec::new();
        
        for topic_request in request.topics {
            let mut response_partitions = Vec::new();
            
            for partition_request in topic_request.partitions {
                // Fetch from storage
                let fetch_result = self.segment_reader.fetch(
                    &topic_request.name,
                    partition_request.partition,
                    partition_request.fetch_offset,
                    partition_request.partition_max_bytes,
                ).await?;
                
                // Encode records
                let records_bytes = if fetch_result.records.is_empty() {
                    vec![]
                } else {
                    encode_records(&fetch_result.records)?
                };
                
                response_partitions.push(FetchResponsePartition {
                    partition: partition_request.partition,
                    error_code: fetch_result.error_code,
                    high_watermark: fetch_result.high_watermark,
                    last_stable_offset: fetch_result.high_watermark,
                    log_start_offset: fetch_result.log_start_offset,
                    aborted: None,
                    preferred_read_replica: -1,
                    records: records_bytes,
                });
            }
            
            response_topics.push(FetchResponseTopic {
                name: topic_request.name,
                partitions: response_partitions,
            });
        }
        
        Ok(Response::Fetch(FetchResponse {
            header: ResponseHeader {
                correlation_id,
            },
            throttle_time_ms: 0,
            topics: response_topics,
        }))
    }
    
    /// Handle metadata request
    async fn handle_metadata(&self, request: MetadataRequest, correlation_id: i32) -> Result<Response> {
        let brokers = vec![
            MetadataBroker {
                node_id: self.config.node_id,
                host: self.config.host.clone(),
                port: self.config.port,
                rack: None,
            }
        ];
        
        let mut topics = Vec::new();
        
        // If specific topics requested, return metadata for those
        if let Some(topic_names) = request.topics {
            for topic_name in topic_names {
                // For now, return a simple response
                topics.push(MetadataTopic {
                    error_code: 0,
                    name: topic_name.clone(),
                    is_internal: false,
                    partitions: vec![
                        MetadataPartition {
                            error_code: 0,
                            partition_index: 0,
                            leader_id: self.config.node_id,
                            leader_epoch: 0,
                            replica_nodes: vec![self.config.node_id],
                            isr_nodes: vec![self.config.node_id],
                            offline_replicas: vec![],
                        }
                    ],
                });
            }
        }
        
        Ok(Response::Metadata(MetadataResponse {
            correlation_id,
            throttle_time_ms: 0,
            brokers,
            cluster_id: Some("chronik-stream".to_string()),
            controller_id: 1,
            topics,
        }))
    }
    
    /// Handle join group request
    async fn handle_join_group(&self, request: JoinGroupRequest, correlation_id: i32) -> Result<Response> {
        let protocols: Vec<(String, Vec<u8>)> = request.protocols
            .into_iter()
            .map(|p| (p.name, p.metadata))
            .collect();
        
        let response = self.group_manager.join_group(
            request.group_id,
            if request.member_id.is_empty() { None } else { Some(request.member_id) },
            "chronik-client".to_string(), // TODO: Get from request header
            "127.0.0.1".to_string(), // TODO: Get from connection
            Duration::from_millis(request.session_timeout as u64),
            Duration::from_millis(request.rebalance_timeout as u64),
            request.protocol_type,
            protocols,
        ).await?;
        
        Ok(Response::JoinGroup(JoinGroupResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: response.error_code,
            generation_id: response.generation_id,
            protocol: response.protocol,
            leader_id: response.leader_id,
            member_id: response.member_id,
            members: response.members.into_iter().map(|m| JoinGroupMember {
                member_id: m.member_id,
                metadata: m.metadata,
            }).collect(),
        }))
    }
    
    /// Handle sync group request
    async fn handle_sync_group(&self, request: SyncGroupRequest, correlation_id: i32) -> Result<Response> {
        let assignments = if request.assignments.is_empty() {
            None
        } else {
            Some(request.assignments.into_iter()
                .map(|a| (a.member_id, a.assignment))
                .collect())
        };
        
        let response = self.group_manager.sync_group(
            request.group_id,
            request.generation_id,
            request.member_id,
            assignments,
        ).await?;
        
        Ok(Response::SyncGroup(SyncGroupResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: response.error_code,
            assignment: response.assignment,
        }))
    }
    
    /// Handle heartbeat request
    async fn handle_heartbeat(&self, request: HeartbeatRequest, correlation_id: i32) -> Result<Response> {
        let response = self.group_manager.heartbeat(
            request.group_id,
            request.member_id,
            request.generation_id,
        ).await?;
        
        Ok(Response::Heartbeat(HeartbeatResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: response.error_code,
        }))
    }
    
    /// Handle leave group request
    async fn handle_leave_group(&self, request: LeaveGroupRequest, correlation_id: i32) -> Result<Response> {
        self.group_manager.leave_group(
            request.group_id,
            request.member_id,
        ).await?;
        
        Ok(Response::LeaveGroup(LeaveGroupResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: 0,
        }))
    }
    
    /// Handle offset commit request
    async fn handle_offset_commit(&self, request: OffsetCommitRequest, correlation_id: i32) -> Result<Response> {
        use crate::consumer_group::TopicPartitionOffset;
        
        let mut offsets = Vec::new();
        for topic in &request.topics {
            for partition in &topic.partitions {
                offsets.push(TopicPartitionOffset {
                    topic: topic.name.clone(),
                    partition: partition.partition_index,
                    offset: partition.committed_offset,
                    metadata: partition.committed_metadata.clone(),
                });
            }
        }
        
        self.group_manager.commit_offsets(
            request.group_id,
            request.generation_id,
            request.member_id,
            offsets,
        ).await?;
        
        // Build response
        let response_topics = request.topics.into_iter().map(|topic| {
            OffsetCommitResponseTopic {
                name: topic.name,
                partitions: topic.partitions.into_iter().map(|p| {
                    OffsetCommitResponsePartition {
                        partition_index: p.partition_index,
                        error_code: 0,
                    }
                }).collect(),
            }
        }).collect();
        
        Ok(Response::OffsetCommit(OffsetCommitResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
        }))
    }
    
    /// Handle offset fetch request
    async fn handle_offset_fetch(&self, request: OffsetFetchRequest, correlation_id: i32) -> Result<Response> {
        // TODO: Implement actual offset fetching from metastore
        let response_topics = if let Some(topics) = request.topics {
            topics.into_iter().map(|topic| {
                OffsetFetchResponseTopic {
                    name: topic,
                    partitions: vec![
                        OffsetFetchResponsePartition {
                            partition_index: 0,
                            committed_offset: 0,
                            metadata: None,
                            error_code: 0,
                        }
                    ],
                }
            }).collect()
        } else {
            vec![]
        };
        
        Ok(Response::OffsetFetch(OffsetFetchResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
        }))
    }
    
    /// Handle API versions request
    async fn handle_api_versions(&self, _request: ApiVersionsRequest, correlation_id: i32) -> Result<Response> {
        use chronik_protocol::parser::ApiKey;
        
        // Define supported API versions
        let api_keys = vec![
            ApiVersionsResponseKey {
                api_key: ApiKey::Produce as i16,
                min_version: 0,
                max_version: 8,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::Fetch as i16,
                min_version: 0,
                max_version: 11,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::Metadata as i16,
                min_version: 0,
                max_version: 9,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::OffsetCommit as i16,
                min_version: 0,
                max_version: 8,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::OffsetFetch as i16,
                min_version: 0,
                max_version: 7,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::JoinGroup as i16,
                min_version: 0,
                max_version: 7,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::Heartbeat as i16,
                min_version: 0,
                max_version: 4,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::LeaveGroup as i16,
                min_version: 0,
                max_version: 4,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::SyncGroup as i16,
                min_version: 0,
                max_version: 5,
            },
            ApiVersionsResponseKey {
                api_key: ApiKey::ApiVersions as i16,
                min_version: 0,
                max_version: 3,
            },
        ];
        
        let response = ApiVersionsResponse {
            header: ResponseHeader { correlation_id },
            error_code: 0,
            api_keys,
            throttle_time_ms: 0,
        };
        
        Ok(Response::ApiVersions(response))
    }
}

/// Simple record structure
struct SimpleRecord {
    timestamp: i64,
    key: Option<Vec<u8>>,
    value: Vec<u8>,
    headers: HashMap<String, Vec<u8>>,
}

/// Decode records from bytes (simplified)
fn decode_records(data: &[u8]) -> Result<Vec<SimpleRecord>> {
    // TODO: Implement proper Kafka record decoding
    // For now, return a simple record
    if data.is_empty() {
        return Ok(vec![]);
    }
    
    Ok(vec![SimpleRecord {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
        key: None,
        value: data.to_vec(),
        headers: HashMap::new(),
    }])
}

/// Encode records to bytes (simplified Kafka format)
fn encode_records(records: &[chronik_storage::Record]) -> Result<Vec<u8>> {
    use chronik_storage::RecordBatch;
    
    // Convert to RecordBatch and encode
    let batch = RecordBatch {
        records: records.to_vec(),
    };
    
    batch.encode()
}