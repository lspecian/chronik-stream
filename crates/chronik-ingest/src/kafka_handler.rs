//! Kafka protocol handler for the ingest server.
//! 
//! This module provides a wrapper around the chronik-protocol ProtocolHandler
//! with additional ingest-specific functionality.

use std::sync::Arc;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{ProtocolHandler, handler::Response, parser::{parse_request_header, parse_request_header_with_correlation, ApiKey, write_response_header, ResponseHeader}, error_codes};
use chronik_storage::{SegmentReader, ObjectStoreTrait};
use crate::produce_handler::ProduceHandler;
use crate::consumer_group::GroupManager;
use crate::fetch_handler::FetchHandler;
use crate::handler::{RequestHandler, HandlerConfig};
use bytes::{Bytes, BytesMut, BufMut};
use tracing::{debug, instrument};

/// Kafka protocol handler with ingest-specific extensions
pub struct KafkaProtocolHandler {
    /// Core protocol handler from chronik-protocol
    protocol_handler: ProtocolHandler,
    /// Produce handler for writing data
    produce_handler: Arc<ProduceHandler>,
    /// Segment reader for fetch operations
    segment_reader: Arc<SegmentReader>,
    /// Metadata store
    metadata_store: Arc<dyn MetadataStore>,
    /// Consumer group manager
    group_manager: Arc<GroupManager>,
    /// Fetch handler for reading data
    fetch_handler: Arc<FetchHandler>,
    /// Node ID
    node_id: i32,
    /// Host address
    host: String,
    /// Port number
    port: i32,
}

impl KafkaProtocolHandler {
    /// Create a new Kafka protocol handler
    pub async fn new(
        produce_handler: Arc<ProduceHandler>,
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        node_id: i32,
        host: String,
        port: i32,
    ) -> Result<Self> {
        let group_manager = Arc::new(GroupManager::new(metadata_store.clone()));
        group_manager.clone().start_expiration_checker();
        
        let fetch_handler = Arc::new(FetchHandler::new(
            segment_reader.clone(),
            metadata_store.clone(),
            object_store,
        ));
        
        // Note: The produce handler and fetch handler are connected via shared state.
        // The produce handler will update the fetch handler's buffer after writing records.
        
        Ok(Self {
            protocol_handler: ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), node_id),
            produce_handler,
            segment_reader,
            metadata_store,
            group_manager,
            fetch_handler,
            node_id,
            host: host.clone(),
            port,
        })
    }
    
    /// Handle a Kafka protocol request
    #[instrument(skip(self, request_bytes))]
    pub async fn handle_request(&self, request_bytes: &[u8]) -> Result<Response> {
        debug!("Handling request of {} bytes", request_bytes.len());
        
        // Log first few bytes to identify request type
        if request_bytes.len() >= 4 {
            let api_key = i16::from_be_bytes([request_bytes[0], request_bytes[1]]);
            let api_version = i16::from_be_bytes([request_bytes[2], request_bytes[3]]);
            tracing::info!("Request: API key={}, version={}", api_key, api_version);
        }
        
        tracing::info!("KafkaProtocolHandler::handle_request called");
        
        // Parse the request header to determine which API is being called
        let mut buf = Bytes::copy_from_slice(request_bytes);
        tracing::debug!("Request bytes (first 20): {:?}", &request_bytes[..request_bytes.len().min(20)]);
        let (header, correlation_id) = match parse_request_header_with_correlation(&mut buf) {
            Ok((h, _)) => {
                let correlation_id = h.correlation_id;
                tracing::debug!("Parsed header: api_key={:?}, api_version={}, correlation_id={}", h.api_key, h.api_version, correlation_id);
                (Some(h), correlation_id)
            }
            Err(Error::ProtocolWithCorrelation { correlation_id, message }) => {
                // Unknown API key but we have correlation ID
                tracing::info!("Unknown API key detected: {}, correlation_id: {}", message, correlation_id);
                (None, correlation_id)
            }
            Err(e) => {
                // Other parsing error, delegate to protocol handler
                tracing::warn!("Other parsing error: {:?}", e);
                return self.protocol_handler.handle_request(request_bytes).await;
            }
        };
        
        // If we couldn't parse the header due to unknown API key, return proper error
        let header = match header {
            Some(h) => h,
            None => {
                // Return error response with preserved correlation ID
                tracing::info!("Returning UNSUPPORTED_VERSION error with correlation_id: {}", correlation_id);
                return Ok(Response {
                    header: ResponseHeader { correlation_id },
                    body: {
                        let mut buf = BytesMut::new();
                        buf.put_i16(error_codes::UNSUPPORTED_VERSION);
                        buf.freeze()
                    },
                    is_flexible: false,  // Error response - conservative default
                });
            }
        };
        
        // For certain APIs that have full implementations in the ingest layer,
        // we should intercept and handle them directly
        match header.api_key {
            ApiKey::Produce => {
                // Parse the produce request
                let request = self.protocol_handler.parse_produce_request(&header, &mut buf)?;
                
                // Call the actual produce handler
                let response = self.produce_handler.handle_produce(request, header.correlation_id).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_produce_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::Fetch => {
                // Parse the fetch request
                let request = self.protocol_handler.parse_fetch_request(&header, &mut buf)?;
                
                // Call the actual fetch handler
                let response = self.fetch_handler.handle_fetch(request, header.correlation_id).await?;
                
                tracing::info!("Fetch handler returned response with {} topics", response.topics.len());
                for topic in &response.topics {
                    for partition in &topic.partitions {
                        tracing::info!("  Topic {} partition {}: {} bytes of records", 
                            topic.name, partition.partition, partition.records.len());
                    }
                }
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                tracing::info!("About to encode Fetch response");
                self.protocol_handler.encode_fetch_response(&mut body_buf, &response, header.api_version)?;
                tracing::info!("Encoded Fetch response: {} bytes", body_buf.len());
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::FindCoordinator => {
                // Parse the FindCoordinator request
                let request = self.protocol_handler.parse_find_coordinator_request(&header, &mut buf)?;
                
                // Always return self as coordinator for now
                let response = self.handle_find_coordinator(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_find_coordinator_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::JoinGroup => {
                // Parse the JoinGroup request
                let request = self.protocol_handler.parse_join_group_request(&header, &mut buf)?;
                
                // Use GroupManager to handle join
                let response = self.handle_join_group(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_join_group_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::SyncGroup => {
                // Parse the SyncGroup request
                let request = self.protocol_handler.parse_sync_group_request(&header, &mut buf)?;
                
                // Use GroupManager to handle sync
                let response = self.handle_sync_group(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_sync_group_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::Heartbeat => {
                // Parse the Heartbeat request
                let request = self.protocol_handler.parse_heartbeat_request(&header, &mut buf)?;
                
                // Use GroupManager to handle heartbeat
                let response = self.handle_heartbeat(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_heartbeat_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::LeaveGroup => {
                // Parse the LeaveGroup request
                let request = self.protocol_handler.parse_leave_group_request(&header, &mut buf)?;
                
                // Use GroupManager to handle leave
                let response = self.handle_leave_group(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_leave_group_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::ListGroups => {
                // We just implemented this, so it should work
                self.protocol_handler.handle_request(request_bytes).await
            }
            ApiKey::OffsetCommit => {
                // Parse the OffsetCommit request
                let request = self.protocol_handler.parse_offset_commit_request(&header, &mut buf)?;
                
                // Handle offset commit
                let response = self.handle_offset_commit(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_offset_commit_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::OffsetFetch => {
                // Parse the OffsetFetch request
                let request = self.protocol_handler.parse_offset_fetch_request(&header, &mut buf)?;
                
                // Handle offset fetch
                let response = self.handle_offset_fetch(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_offset_fetch_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
                    is_flexible: false,  // TODO: Check API version for flexible encoding
                })
            }
            ApiKey::ApiVersions => {
                // ApiVersions must use the protocol handler for proper encoding
                let response = self.protocol_handler.handle_request(request_bytes).await?;
                tracing::info!("Response body size: {}", response.body.len());
                Ok(response)
            }
            _ => {
                // For all other APIs, use the protocol handler
                let response = self.protocol_handler.handle_request(request_bytes).await?;
                tracing::info!("Response body size: {}", response.body.len());
                Ok(response)
            }
        }
    }
    
    /// Handle FindCoordinator request
    async fn handle_find_coordinator(&self, request: chronik_protocol::find_coordinator_types::FindCoordinatorRequest) -> Result<chronik_protocol::find_coordinator_types::FindCoordinatorResponse> {
        use chronik_protocol::find_coordinator_types::{FindCoordinatorResponse, error_codes};
        
        // For now, always return self as coordinator
        Ok(FindCoordinatorResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
            error_message: None,
            node_id: self.node_id,
            host: self.host.clone(),
            port: self.port,
        })
    }
    
    /// Handle JoinGroup request
    async fn handle_join_group(&self, request: chronik_protocol::join_group_types::JoinGroupRequest) -> Result<chronik_protocol::join_group_types::JoinGroupResponse> {
        use chronik_protocol::join_group_types::{JoinGroupResponse, JoinGroupResponseMember};
        use std::time::Duration;
        
        // Convert protocols to the format expected by GroupManager
        let protocols: Vec<(String, Vec<u8>)> = request.protocols.into_iter()
            .map(|p| (p.name, p.metadata.to_vec()))
            .collect();
        
        // Call GroupManager
        let response = self.group_manager.join_group(
            request.group_id,
            if request.member_id.is_empty() { None } else { Some(request.member_id) },
            "client".to_string(), // TODO: Extract from request
            self.host.clone(),
            Duration::from_millis(request.session_timeout_ms as u64),
            Duration::from_millis(request.rebalance_timeout_ms as u64),
            request.protocol_type,
            protocols,
            request.group_instance_id,
        ).await?;
        
        // Convert response
        Ok(JoinGroupResponse {
            throttle_time_ms: 0,
            error_code: response.error_code,
            generation_id: response.generation_id,
            protocol_type: Some(response.protocol.clone()),
            protocol_name: Some(response.protocol),
            leader: response.leader_id,
            member_id: response.member_id,
            members: response.members.into_iter().map(|m| JoinGroupResponseMember {
                member_id: m.member_id,
                group_instance_id: None,
                metadata: m.metadata.into(),
            }).collect(),
        })
    }
    
    /// Handle SyncGroup request
    async fn handle_sync_group(&self, request: chronik_protocol::sync_group_types::SyncGroupRequest) -> Result<chronik_protocol::sync_group_types::SyncGroupResponse> {
        use chronik_protocol::sync_group_types::SyncGroupResponse;
        
        // Convert assignments if this is the leader
        let assignments = if !request.assignments.is_empty() {
            Some(request.assignments.into_iter()
                .map(|a| (a.member_id, a.assignment.to_vec()))
                .collect())
        } else {
            None
        };
        
        // Call GroupManager
        let response = self.group_manager.sync_group(
            request.group_id,
            request.generation_id,
            request.member_id,
            0, // member_epoch - TODO: Extract from request
            assignments,
        ).await?;
        
        // Convert response
        Ok(SyncGroupResponse {
            throttle_time_ms: 0,
            error_code: response.error_code,
            protocol_type: None, // TODO: Get from group state
            protocol_name: None, // TODO: Get from group state
            assignment: response.assignment.into(),
        })
    }
    
    /// Handle Heartbeat request
    async fn handle_heartbeat(&self, request: chronik_protocol::heartbeat_types::HeartbeatRequest) -> Result<chronik_protocol::heartbeat_types::HeartbeatResponse> {
        use chronik_protocol::heartbeat_types::HeartbeatResponse;
        
        // Call GroupManager
        let response = self.group_manager.heartbeat(
            request.group_id,
            request.member_id,
            request.generation_id,
            None, // member_epoch - TODO: Extract from request
        ).await?;
        
        // Convert response
        Ok(HeartbeatResponse {
            throttle_time_ms: 0,
            error_code: response.error_code,
        })
    }
    
    /// Handle LeaveGroup request
    async fn handle_leave_group(&self, request: chronik_protocol::leave_group_types::LeaveGroupRequest) -> Result<chronik_protocol::leave_group_types::LeaveGroupResponse> {
        use chronik_protocol::leave_group_types::{LeaveGroupResponse, MemberResponse};
        
        // Handle each member leaving
        let mut member_responses = Vec::new();
        for member in request.members {
            match self.group_manager.leave_group(
                request.group_id.clone(),
                member.member_id.clone(),
            ).await {
                Ok(_) => {
                    member_responses.push(MemberResponse {
                        member_id: member.member_id,
                        group_instance_id: member.group_instance_id,
                        error_code: 0,
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to leave group: {:?}", e);
                    member_responses.push(MemberResponse {
                        member_id: member.member_id,
                        group_instance_id: member.group_instance_id,
                        error_code: 25, // UNKNOWN_MEMBER_ID
                    });
                }
            }
        }
        
        Ok(LeaveGroupResponse {
            throttle_time_ms: 0,
            error_code: 0,
            members: member_responses,
        })
    }
    
    /// Handle OffsetCommit request
    async fn handle_offset_commit(&self, request: chronik_protocol::types::OffsetCommitRequest) -> Result<chronik_protocol::types::OffsetCommitResponse> {
        use chronik_protocol::types::{OffsetCommitResponse, OffsetCommitResponseTopic, OffsetCommitResponsePartition};
        use chronik_protocol::ResponseHeader;
        use chronik_common::metadata::ConsumerOffset;
        
        let mut response_topics = Vec::new();
        
        for topic in request.topics {
            let mut response_partitions = Vec::new();
            
            for partition in topic.partitions {
                // Store the offset in metadata
                let offset = ConsumerOffset {
                    group_id: request.group_id.clone(),
                    topic: topic.name.clone(),
                    partition: partition.partition_index as u32,
                    offset: partition.committed_offset,
                    metadata: partition.committed_metadata,
                    commit_timestamp: chrono::Utc::now(),
                };
                
                match self.metadata_store.commit_offset(offset).await {
                    Ok(_) => {
                        response_partitions.push(OffsetCommitResponsePartition {
                            partition_index: partition.partition_index,
                            error_code: 0,
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to commit offset: {:?}", e);
                        response_partitions.push(OffsetCommitResponsePartition {
                            partition_index: partition.partition_index,
                            error_code: 5, // UNKNOWN_TOPIC_OR_PARTITION
                        });
                    }
                }
            }
            
            response_topics.push(OffsetCommitResponseTopic {
                name: topic.name,
                partitions: response_partitions,
            });
        }
        
        Ok(OffsetCommitResponse {
            header: ResponseHeader {
                correlation_id: 0, // Will be set by the caller
            },
            throttle_time_ms: 0,
            topics: response_topics,
        })
    }
    
    /// Handle OffsetFetch request
    async fn handle_offset_fetch(&self, request: chronik_protocol::types::OffsetFetchRequest) -> Result<chronik_protocol::types::OffsetFetchResponse> {
        use chronik_protocol::types::{OffsetFetchResponse, OffsetFetchResponseTopic, OffsetFetchResponsePartition};
        use chronik_protocol::ResponseHeader;
        
        let mut response_topics = Vec::new();
        
        // If topics is None or empty, fetch all topics for this group
        let topics_to_fetch = if let Some(topics) = request.topics {
            topics
        } else {
            // For now, return empty if no specific topics requested
            // In a full implementation, we'd query all topics with offsets for this group
            vec![]
        };
        
        for topic_name in topics_to_fetch {
            let mut response_partitions = Vec::new();
            
            // For now, check partitions 0-9 (in production, we'd know the actual partition count)
            for partition_index in 0..10 {
                match self.metadata_store.get_consumer_offset(&request.group_id, &topic_name, partition_index).await {
                    Ok(Some(offset)) => {
                        response_partitions.push(OffsetFetchResponsePartition {
                            partition_index: partition_index as i32,
                            committed_offset: offset.offset,
                            metadata: offset.metadata,
                            error_code: 0,
                        });
                    }
                    Ok(None) => {
                        // No offset found for this partition - skip it
                        continue;
                    }
                    Err(_) => {
                        // Error fetching - skip this partition
                        continue;
                    }
                }
            }
            
            // Only add topic if we found at least one partition with an offset
            if !response_partitions.is_empty() {
                response_topics.push(OffsetFetchResponseTopic {
                    name: topic_name,
                    partitions: response_partitions,
                });
            }
        }
        
        Ok(OffsetFetchResponse {
            header: ResponseHeader {
                correlation_id: 0, // Will be set by the caller
            },
            throttle_time_ms: 0,
            topics: response_topics,
        })
    }
}