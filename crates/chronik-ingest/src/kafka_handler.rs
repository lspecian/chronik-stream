//! Kafka protocol handler for the ingest server.
//! 
//! This module provides a wrapper around the chronik-protocol ProtocolHandler
//! with additional ingest-specific functionality.

use std::sync::Arc;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{ProtocolHandler, handler::Response, parser::{parse_request_header, parse_request_header_with_correlation, ApiKey, write_response_header, ResponseHeader}, error_codes};
use chronik_storage::SegmentReader;
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
        node_id: i32,
        host: String,
        port: i32,
    ) -> Result<Self> {
        let group_manager = Arc::new(GroupManager::new(metadata_store.clone()));
        group_manager.clone().start_expiration_checker();
        
        let fetch_handler = Arc::new(FetchHandler::new(
            segment_reader.clone(),
            metadata_store.clone(),
        ));
        
        // Note: The produce handler and fetch handler are connected via shared state.
        // The produce handler will update the fetch handler's buffer after writing records.
        
        Ok(Self {
            protocol_handler: ProtocolHandler::with_metadata_store(metadata_store.clone()),
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
        let (header, correlation_id) = match parse_request_header_with_correlation(&mut buf) {
            Ok((h, _)) => {
                let correlation_id = h.correlation_id;
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
                    }
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
                })
            }
            ApiKey::Fetch => {
                // Parse the fetch request
                let request = self.protocol_handler.parse_fetch_request(&header, &mut buf)?;
                
                // Call the actual fetch handler
                let response = self.fetch_handler.handle_fetch(request, header.correlation_id).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_fetch_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader { correlation_id: header.correlation_id },
                    body: body_buf.freeze(),
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
                })
            }
            ApiKey::ListGroups => {
                // We just implemented this, so it should work
                self.protocol_handler.handle_request(request_bytes).await
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
}