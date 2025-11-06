//! Request handler for Kafka protocol requests.

use crate::consumer_group::GroupManager;
use crate::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use crate::offset_storage::{OffsetStorage, OffsetStorageConfig};
use crate::coordinator_manager::{CoordinatorManager, CoordinatorConfig, CoordinatorType};
use chronik_common::Result;
use chronik_auth::{AuthMiddleware, AuthError, SaslMechanism, SaslAuthResult, Resource, Operation};
use chronik_protocol::{
    Request, Response, RequestBody,
    ProduceRequest,
    FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition,
    MetadataRequest, MetadataResponse, MetadataBroker, MetadataTopic, MetadataPartition,
    JoinGroupRequest, JoinGroupResponse, JoinGroupMember,
    SyncGroupRequest, SyncGroupResponse,
    HeartbeatRequest, HeartbeatResponse,
    LeaveGroupRequest, LeaveGroupResponse,
    OffsetCommitRequest, OffsetCommitResponse, OffsetCommitResponseTopic, OffsetCommitResponsePartition,
    OffsetFetchRequest, OffsetFetchResponse, OffsetFetchResponseTopic, OffsetFetchResponsePartition,
    ApiVersionsRequest, ApiVersionsResponse, ApiVersionsResponseKey,
    SaslHandshakeRequest, SaslHandshakeResponse, SaslAuthenticateRequest, SaslAuthenticateResponse,
    parser::ResponseHeader,
};
use chronik_storage::{SegmentReader, object_store::storage::ObjectStore};
use chronik_common::metadata::traits::MetadataStore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

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
    auth_middleware: Arc<AuthMiddleware>,
    offset_storage: Arc<OffsetStorage>,
    coordinator_manager: Arc<CoordinatorManager>,
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
        
        let group_manager = Arc::new(GroupManager::new(metadata_store.clone()));
        
        // Start group expiration checker
        group_manager.clone().start_expiration_checker();
        
        // Create offset storage
        let offset_storage_config = OffsetStorageConfig::default();
        let offset_storage = Arc::new(OffsetStorage::new(
            metadata_store.clone(),
            offset_storage_config,
        ));
        
        // Start offset cleanup task
        // as the metadata store already has the connection
        offset_storage.start_cleanup_task(None).await;
        
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
            ProduceHandler::new(produce_config, storage, metadata_store.clone()).await?
        );
        
        // Initialize auth middleware
        let auth_middleware = Arc::new(AuthMiddleware::new(false)); // Don't allow anonymous
        auth_middleware.init_defaults().await
            .map_err(|e| chronik_common::Error::Internal(format!("Failed to init auth: {:?}", e)))?;
        
        // Create coordinator manager
        let coordinator_config = CoordinatorConfig::default();
        let coordinator_manager = Arc::new(CoordinatorManager::new(
            metadata_store.clone(),
            coordinator_config,
            config.node_id,
        ));
        
        // Initialize coordinator manager
        coordinator_manager.init().await?;
        
        Ok(Self {
            config,
            state,
            produce_handler,
            segment_reader,
            group_manager,
            auth_middleware,
            offset_storage,
            coordinator_manager,
        })
    }
    
    /// Handle a request
    pub async fn handle_request(&self, request: Request, session_id: &str) -> Result<Response> {
        let correlation_id = request.header.correlation_id;
        
        // Handle SASL-related requests without auth check
        match &request.body {
            RequestBody::SaslHandshake(_) | RequestBody::SaslAuthenticate(_) | RequestBody::ApiVersions(_) => {
                // These don't require authentication
            }
            _ => {
                // All other requests require authentication - check session
                let ctx = self.auth_middleware.get_or_create_context(session_id).await;
                if !ctx.is_authenticated() {
                    return Err(chronik_common::Error::Unauthorized("Not authenticated".to_string()));
                }
            }
        }
        
        match request.body {
            RequestBody::Produce(produce) => self.handle_produce(produce, correlation_id, session_id).await,
            RequestBody::Fetch(fetch) => self.handle_fetch(fetch, correlation_id, session_id).await,
            RequestBody::Metadata(metadata) => self.handle_metadata(metadata, correlation_id).await,
            RequestBody::JoinGroup(join) => self.handle_join_group(join, correlation_id, session_id).await,
            RequestBody::SyncGroup(sync) => self.handle_sync_group(sync, correlation_id, session_id).await,
            RequestBody::Heartbeat(heartbeat) => self.handle_heartbeat(heartbeat, correlation_id, session_id).await,
            RequestBody::LeaveGroup(leave) => self.handle_leave_group(leave, correlation_id, session_id).await,
            RequestBody::OffsetCommit(commit) => self.handle_offset_commit(commit, correlation_id, session_id).await,
            RequestBody::OffsetFetch(fetch) => self.handle_offset_fetch(fetch, correlation_id, session_id).await,
            RequestBody::ApiVersions(api_versions) => self.handle_api_versions(api_versions, correlation_id).await,
            RequestBody::SaslHandshake(handshake) => self.handle_sasl_handshake(handshake, correlation_id, session_id).await,
            RequestBody::SaslAuthenticate(auth) => self.handle_sasl_authenticate(auth, correlation_id, session_id).await,
            RequestBody::DescribeConfigs(_) => {
                // Not implemented in this handler - should be handled by protocol handler
                Err(chronik_common::Error::Protocol("DescribeConfigs should be handled by protocol handler".to_string()))
            },
        }
    }
    
    /// Handle produce request
    async fn handle_produce(&self, request: ProduceRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check write permission for each topic
        for topic_data in &request.topics {
            self.auth_middleware.check_topic_write(session_id, &topic_data.name).await
                .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for topic {}: {:?}", topic_data.name, e)))?;
        }
        
        // Delegate to the produce handler
        let response = self.produce_handler.handle_produce(request, correlation_id).await?;
        Ok(Response::Produce(response))
    }
    
    /// Handle fetch request
    async fn handle_fetch(&self, request: FetchRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check read permission for each topic
        for topic_request in &request.topics {
            self.auth_middleware.check_topic_read(session_id, &topic_request.name).await
                .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for topic {}: {:?}", topic_request.name, e)))?;
        }
        
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
            error_code: 0,
            session_id: 0,
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
            throttle_time_ms: 0,
            brokers,
            cluster_id: Some("chronik-stream".to_string()),
            controller_id: 1,
            topics,
            cluster_authorized_operations: None,
        }))
    }
    
    /// Handle join group request
    async fn handle_join_group(&self, request: JoinGroupRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check consumer group permission
        self.auth_middleware.check_consumer_group(session_id, &request.group_id).await
            .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for group {}: {:?}", request.group_id, e)))?;
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
            None, // TODO: Add static_member_id support from KIP-345
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
    async fn handle_sync_group(&self, request: SyncGroupRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check consumer group permission
        self.auth_middleware.check_consumer_group(session_id, &request.group_id).await
            .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for group {}: {:?}", request.group_id, e)))?;
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
            0, // TODO: Add member_epoch from KIP-848
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
    async fn handle_heartbeat(&self, request: HeartbeatRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check consumer group permission
        self.auth_middleware.check_consumer_group(session_id, &request.group_id).await
            .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for group {}: {:?}", request.group_id, e)))?;
        let response = self.group_manager.heartbeat(
            request.group_id,
            request.member_id,
            request.generation_id,
            None, // TODO: Add member_epoch from KIP-848
        ).await?;
        
        Ok(Response::Heartbeat(HeartbeatResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: response.error_code,
        }))
    }
    
    /// Handle leave group request
    async fn handle_leave_group(&self, request: LeaveGroupRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check consumer group permission
        self.auth_middleware.check_consumer_group(session_id, &request.group_id).await
            .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for group {}: {:?}", request.group_id, e)))?;
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
    async fn handle_offset_commit(&self, request: OffsetCommitRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check consumer group permission
        self.auth_middleware.check_consumer_group(session_id, &request.group_id).await
            .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for group {}: {:?}", request.group_id, e)))?;
        
        // Validate group ID
        if request.group_id.is_empty() {
            return Ok(Response::OffsetCommit(OffsetCommitResponse {
                header: ResponseHeader { correlation_id },
                throttle_time_ms: 0,
                topics: vec![],
            }));
        }
        
        // Validate generation ID (must be >= -1)
        if request.generation_id < -1 {
            let error_response = request.topics.iter().map(|topic| {
                OffsetCommitResponseTopic {
                    name: topic.name.clone(),
                    partitions: topic.partitions.iter().map(|p| {
                        OffsetCommitResponsePartition {
                            partition_index: p.partition_index,
                            error_code: 42, // INVALID_GROUP_ID
                        }
                    }).collect(),
                }
            }).collect();
            
            return Ok(Response::OffsetCommit(OffsetCommitResponse {
                header: ResponseHeader { correlation_id },
                throttle_time_ms: 0,
                topics: error_response,
            }));
        }
        
        // Prepare offsets for storage with validation
        let mut storage_offsets = Vec::new();
        let mut validation_errors = Vec::new();
        
        for topic in &request.topics {
            // Validate topic name
            if topic.name.is_empty() {
                for partition in &topic.partitions {
                    validation_errors.push((topic.name.clone(), partition.partition_index, 17)); // INVALID_TOPIC
                }
                continue;
            }
            
            for partition in &topic.partitions {
                // Validate partition index
                if partition.partition_index < 0 {
                    validation_errors.push((topic.name.clone(), partition.partition_index, 37)); // INVALID_PARTITION_EXCEPTION
                    continue;
                }
                
                // Validate offset
                if partition.committed_offset < -1 {
                    validation_errors.push((topic.name.clone(), partition.partition_index, 1)); // OFFSET_OUT_OF_RANGE
                    continue;
                }
                
                // Validate metadata size (Kafka limit is typically 4096 bytes)
                if let Some(ref metadata) = partition.committed_metadata {
                    if metadata.len() > 4096 {
                        validation_errors.push((topic.name.clone(), partition.partition_index, 12)); // OFFSET_METADATA_TOO_LARGE
                        continue;
                    }
                }
                
                storage_offsets.push((
                    topic.name.clone(),
                    partition.partition_index as u32,
                    partition.committed_offset,
                    partition.committed_metadata.clone(),
                ));
            }
        }
        
        // If all offsets failed validation, return error response
        if !validation_errors.is_empty() && storage_offsets.is_empty() {
            let mut response_topics_map: HashMap<String, Vec<OffsetCommitResponsePartition>> = HashMap::new();
            
            for (topic, partition, error_code) in validation_errors {
                response_topics_map.entry(topic)
                    .or_insert_with(Vec::new)
                    .push(OffsetCommitResponsePartition {
                        partition_index: partition,
                        error_code,
                    });
            }
            
            let response_topics: Vec<_> = response_topics_map.into_iter()
                .map(|(topic, partitions)| OffsetCommitResponseTopic {
                    name: topic,
                    partitions,
                })
                .collect();
            
            return Ok(Response::OffsetCommit(OffsetCommitResponse {
                header: ResponseHeader { correlation_id },
                throttle_time_ms: 0,
                topics: response_topics,
            }));
        }
        
        // Store valid offsets
        let commit_results = if !storage_offsets.is_empty() {
            self.offset_storage.commit_offsets(
                &request.group_id,
                request.generation_id,
                storage_offsets,
            ).await?
        } else {
            vec![]
        };
        
        // Build response from storage results and validation errors
        let mut response_topics_map: HashMap<String, Vec<OffsetCommitResponsePartition>> = HashMap::new();
        
        // Add successful commits
        for result in commit_results {
            response_topics_map.entry(result.topic)
                .or_insert_with(Vec::new)
                .push(OffsetCommitResponsePartition {
                    partition_index: result.partition as i32,
                    error_code: result.error_code,
                });
        }
        
        // Add validation errors
        for (topic, partition, error_code) in validation_errors {
            response_topics_map.entry(topic)
                .or_insert_with(Vec::new)
                .push(OffsetCommitResponsePartition {
                    partition_index: partition,
                    error_code,
                });
        }
        
        let response_topics: Vec<_> = response_topics_map.into_iter()
            .map(|(topic, partitions)| OffsetCommitResponseTopic {
                name: topic,
                partitions,
            })
            .collect();
        
        Ok(Response::OffsetCommit(OffsetCommitResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
        }))
    }
    
    /// Handle offset fetch request
    async fn handle_offset_fetch(&self, request: OffsetFetchRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Check consumer group permission
        self.auth_middleware.check_consumer_group(session_id, &request.group_id).await
            .map_err(|e| chronik_common::Error::Unauthorized(format!("Permission denied for group {}: {:?}", request.group_id, e)))?;
        
        // Validate group ID
        if request.group_id.is_empty() {
            return Ok(Response::OffsetFetch(OffsetFetchResponse {
                header: ResponseHeader { correlation_id },
                throttle_time_ms: 0,
                topics: vec![],
                group_id: Some(request.group_id.clone()),  // v8+ requires group_id
            }));
        }
        
        // Determine which topic-partitions to fetch
        let topic_partitions = if let Some(topics) = &request.topics {
            // Validate topic names
            let mut tp = Vec::new();
            for topic_request in topics {
                if topic_request.name.is_empty() {
                    continue; // Skip invalid topic names
                }

                // If specific partitions requested, use those
                if !topic_request.partitions.is_empty() {
                    for partition_id in &topic_request.partitions {
                        tp.push((topic_request.name.clone(), *partition_id as u32));
                    }
                } else {
                    // No specific partitions - query all partitions from metadata
                    match self.produce_handler.get_metadata_store().get_topic(&topic_request.name).await? {
                        Some(topic_meta) => {
                            for partition in 0..topic_meta.config.partition_count {
                                tp.push((topic_request.name.clone(), partition));
                            }
                        }
                        None => {
                            // Topic doesn't exist - add default partitions for error response
                            for partition in 0..1 {
                                tp.push((topic_request.name.clone(), partition));
                            }
                        }
                    }
                }
            }
            tp
        } else {
            // Fetch all offsets for the group - would need to scan all topics
            // For now, return empty as fetching all is complex
            vec![]
        };
        
        // Fetch offsets from storage
        let fetched_offsets = self.offset_storage.fetch_offsets(
            &request.group_id,
            topic_partitions,
        ).await?;
        
        // Group offsets by topic
        let mut topics_map: HashMap<String, Vec<(i32, i64, Option<String>)>> = HashMap::new();
        for offset in fetched_offsets {
            topics_map.entry(offset.topic)
                .or_insert_with(Vec::new)
                .push((offset.partition as i32, offset.offset, offset.metadata));
        }
        
        // Build response
        let response_topics = if let Some(requested_topics) = request.topics {
            // Return offsets for requested topics
            requested_topics.into_iter().map(|topic_request| {
                let partitions = if let Some(topic_offsets) = topics_map.get(&topic_request.name) {
                    topic_offsets.iter().map(|(partition, offset, metadata)| {
                        OffsetFetchResponsePartition {
                            partition_index: *partition,
                            committed_offset: *offset,
                            metadata: metadata.clone(),
                            error_code: 0,
                        }
                    }).collect()
                } else {
                    // No offsets for this topic - return default
                    vec![OffsetFetchResponsePartition {
                        partition_index: 0,
                        committed_offset: -1, // No offset committed
                        metadata: None,
                        error_code: 0,
                    }]
                };
                
                OffsetFetchResponseTopic {
                    name: topic_request.name,
                    partitions,
                }
            }).collect()
        } else {
            // Return all offsets for the group
            topics_map.into_iter().map(|(topic, topic_offsets)| {
                OffsetFetchResponseTopic {
                    name: topic,
                    partitions: topic_offsets.into_iter().map(|(partition, offset, metadata)| {
                        OffsetFetchResponsePartition {
                            partition_index: partition,
                            committed_offset: offset,
                            metadata,
                            error_code: 0,
                        }
                    }).collect(),
                }
            }).collect()
        };
        
        Ok(Response::OffsetFetch(OffsetFetchResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
            group_id: Some(request.group_id.clone()),  // v8+ requires group_id
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
    
    /// Get produce handler metrics
    pub fn metrics(&self) -> &crate::produce_handler::ProduceMetrics {
        self.produce_handler.metrics()
    }
    
    /// Handle SASL handshake request
    async fn handle_sasl_handshake(&self, request: SaslHandshakeRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Get supported mechanisms
        let mechanisms = self.auth_middleware.get_sasl_mechanisms(&[request.mechanism.clone()]).await;
        
        let error_code = if mechanisms.contains(&request.mechanism) {
            // Store the mechanism for this session
            if let Some(mechanism) = SaslMechanism::from_str(&request.mechanism) {
                self.auth_middleware.set_session_mechanism(session_id, mechanism).await;
            }
            0 // No error
        } else {
            33 // Unsupported SASL mechanism
        };
        
        Ok(Response::SaslHandshake(SaslHandshakeResponse {
            error_code,
            mechanisms,
        }))
    }
    
    /// Handle SASL authenticate request
    async fn handle_sasl_authenticate(&self, request: SaslAuthenticateRequest, correlation_id: i32, session_id: &str) -> Result<Response> {
        // Get the active mechanism for this session
        let mechanism = match self.auth_middleware.get_session_mechanism(session_id).await {
            Some(mech) => mech,
            None => {
                // Default to PLAIN if no mechanism set (backward compatibility)
                SaslMechanism::Plain
            }
        };
        
        match self.auth_middleware.authenticate_sasl(session_id, &mechanism, &request.auth_bytes).await {
            Ok(SaslAuthResult::Success(principal)) => {
                Ok(Response::SaslAuthenticate(SaslAuthenticateResponse {
                    error_code: 0,
                    error_message: None,
                    auth_bytes: vec![],
                    session_lifetime_ms: 3600000, // 1 hour
                }))
            }
            Ok(SaslAuthResult::Continue(challenge_bytes)) => {
                // SCRAM requires multiple round trips
                Ok(Response::SaslAuthenticate(SaslAuthenticateResponse {
                    error_code: 0,
                    error_message: None,
                    auth_bytes: challenge_bytes,
                    session_lifetime_ms: 0, // Not authenticated yet
                }))
            }
            Err(e) => {
                Ok(Response::SaslAuthenticate(SaslAuthenticateResponse {
                    error_code: 58, // SASL authentication failed
                    error_message: Some(format!("Authentication failed: {:?}", e)),
                    auth_bytes: vec![],
                    session_lifetime_ms: 0,
                }))
            }
        }
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