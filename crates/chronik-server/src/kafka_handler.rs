//! Kafka protocol handler for the ingest server.
//! 
//! This module provides a wrapper around the chronik-protocol ProtocolHandler
//! with additional ingest-specific functionality.

use std::sync::Arc;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{ProtocolHandler, handler::Response, parser::{parse_request_header, ApiKey, write_response_header, ResponseHeader}, error_codes};
use chronik_storage::{SegmentReader, ObjectStoreTrait};
use crate::produce_handler::ProduceHandler;
use crate::consumer_group::GroupManager;
use crate::fetch_handler::FetchHandler;
use crate::wal_integration::WalProduceHandler;
use bytes::{Bytes, BytesMut, BufMut, Buf};
use tracing::{debug, instrument};

/// Kafka protocol handler with ingest-specific extensions
pub struct KafkaProtocolHandler {
    /// Core protocol handler from chronik-protocol
    protocol_handler: ProtocolHandler,
    /// Produce handler for writing data
    produce_handler: Arc<ProduceHandler>,
    /// WAL handler for durability (MANDATORY - no longer optional)
    wal_handler: Arc<WalProduceHandler>,
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
    /// Create a new Kafka protocol handler with WAL (MANDATORY)
    /// WAL is now required for all Chronik Stream instances - no optional paths
    pub async fn new(
        produce_handler: Arc<ProduceHandler>,
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        fetch_handler: Arc<FetchHandler>,
        wal_handler: Arc<WalProduceHandler>,  // WAL is MANDATORY, not optional
        node_id: i32,
        host: String,
        port: i32,
    ) -> Result<Self> {
        let group_manager = Arc::new(GroupManager::new(metadata_store.clone()));
        group_manager.clone().start_expiration_checker();
        
        Ok(Self {
            protocol_handler: ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), node_id),
            produce_handler,
            wal_handler,
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
        
        
        // Parse the request header to determine which API is being called
        let mut buf = bytes::Bytes::from(request_bytes.to_vec());
        let header = parse_request_header(&mut buf)?;
        
        // Route to specific handlers for certain API keys
        // produce and fetch are special-cased because they need access to our storage layer
        // we should intercept and handle them directly
        match header.api_key {
            ApiKey::Produce => {
                // Parse the produce request
                let request = self.protocol_handler.parse_produce_request(&header, &mut buf)?;
                
                // WAL is mandatory - all produce requests MUST go through WAL
                // There is no fallback path - WAL is the only durability mechanism
                let response = self.wal_handler.handle_produce(request, header.correlation_id).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                eprintln!("CRITICAL: About to call encode_produce_response with version {}", header.api_version);
                self.protocol_handler.encode_produce_response(&mut body_buf, &response, header.api_version)?;
                
                // Create response header
                let response_header = ResponseHeader {
                    correlation_id: header.correlation_id,
                };
                
                // Write response header
                let mut header_buf = BytesMut::new();
                write_response_header(&mut header_buf, &response_header);
                
                // Combine header and body
                let mut final_buf = BytesMut::new();
                final_buf.extend_from_slice(&header_buf);
                final_buf.extend_from_slice(&body_buf);
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 9,  // v9+ uses flexible/compact encoding
                    api_key: ApiKey::Produce,
                })
            }
            ApiKey::Fetch => {
                // Parse the fetch request
                let request = self.protocol_handler.parse_fetch_request(&header, &mut buf)?;
                
                // Use our fetch handler to process the request
                let response = self.fetch_handler.handle_fetch(request, header.correlation_id).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_fetch_response(&mut body_buf, &response, header.api_version)?;
                
                // Create response header
                let response_header = ResponseHeader {
                    correlation_id: header.correlation_id,
                };
                
                // Write response header
                let mut header_buf = BytesMut::new();
                write_response_header(&mut header_buf, &response_header);
                
                // Combine header and body
                let mut final_buf = BytesMut::new();
                final_buf.extend_from_slice(&header_buf);
                final_buf.extend_from_slice(&body_buf);
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 11, // Fetch uses flexible from v11+
                    api_key: ApiKey::Fetch,
                })
            }
            ApiKey::JoinGroup => {
                debug!("Processing JoinGroup request");
                // Parse the join group request
                let request = self.protocol_handler.parse_join_group_request(&header, &mut buf)?;
                
                // Handle the join group request
                let response = self.group_manager.handle_join_group(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_join_group_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 6, // JoinGroup uses flexible from v6+
                    api_key: ApiKey::JoinGroup,
                })
            }
            ApiKey::SyncGroup => {
                debug!("Processing SyncGroup request");
                // Parse the sync group request
                let request = self.protocol_handler.parse_sync_group_request(&header, &mut buf)?;
                
                // Handle the sync group request
                let response = self.group_manager.handle_sync_group(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_sync_group_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 4, // SyncGroup uses flexible from v4+
                    api_key: ApiKey::SyncGroup,
                })
            }
            ApiKey::Heartbeat => {
                debug!("Processing Heartbeat request");
                // Parse the heartbeat request
                let request = self.protocol_handler.parse_heartbeat_request(&header, &mut buf)?;
                
                // Handle the heartbeat request
                let response = self.group_manager.handle_heartbeat(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_heartbeat_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 4, // Heartbeat uses flexible from v4+
                    api_key: ApiKey::Heartbeat,
                })
            }
            ApiKey::LeaveGroup => {
                debug!("Processing LeaveGroup request");
                // Parse the leave group request
                let request = self.protocol_handler.parse_leave_group_request(&header, &mut buf)?;
                
                // Handle the leave group request
                let response = self.group_manager.handle_leave_group(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_leave_group_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 4, // LeaveGroup uses flexible from v4+
                    api_key: ApiKey::LeaveGroup,
                })
            }
            ApiKey::OffsetCommit => {
                debug!("Processing OffsetCommit request");
                // Parse the offset commit request
                let request = self.protocol_handler.parse_offset_commit_request(&header, &mut buf)?;
                
                // Handle the offset commit request
                let response = self.group_manager.handle_offset_commit(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_offset_commit_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 8, // OffsetCommit uses flexible from v8+
                    api_key: ApiKey::OffsetCommit,
                })
            }
            ApiKey::OffsetFetch => {
                debug!("Processing OffsetFetch request");
                // Parse the offset fetch request
                let request = self.protocol_handler.parse_offset_fetch_request(&header, &mut buf)?;
                
                // Handle the offset fetch request
                let response = self.group_manager.handle_offset_fetch(request).await?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_offset_fetch_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 6, // OffsetFetch uses flexible from v6+
                    api_key: ApiKey::OffsetFetch,
                })
            }
            ApiKey::FindCoordinator => {
                debug!("Processing FindCoordinator request");
                // Parse the find coordinator request
                let request = self.protocol_handler.parse_find_coordinator_request(&header, &mut buf)?;
                
                // Handle the find coordinator request
                let response = self.group_manager.handle_find_coordinator(request, &self.host, self.port)?;
                
                // Encode the response
                let mut body_buf = BytesMut::new();
                self.protocol_handler.encode_find_coordinator_response(&mut body_buf, &response, header.api_version)?;
                
                Ok(Response {
                    header: ResponseHeader {
                        correlation_id: header.correlation_id,
                    },
                    body: body_buf.freeze(),
                    is_flexible: header.api_version >= 3, // FindCoordinator uses flexible from v3+
                    api_key: ApiKey::FindCoordinator,
                })
            }
            _ => {
                // For all other API calls, delegate to the protocol handler
                self.protocol_handler.handle_request(request_bytes).await
            }
        }
    }
}

