//! Kafka protocol request handler.

use bytes::{Bytes, BytesMut};
use chronik_common::{Result, Error};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::error_codes;
use crate::parser::{
    ApiKey, RequestHeader, ResponseHeader, VersionRange, 
    parse_request_header_with_correlation,
    supported_api_versions,
    Encoder,
    is_flexible_version
};

// Consumer group state management
#[derive(Debug, Clone)]
pub struct ConsumerGroup {
    pub group_id: String,
    pub leader_id: Option<String>,
    pub generation_id: i32,
    pub protocol_type: Option<String>,
    pub protocol: Option<String>,
    pub members: Vec<GroupMember>,
    pub state: String,
    pub offsets: HashMap<String, HashMap<i32, OffsetInfo>>, // topic -> partition -> offset
}

#[derive(Debug, Clone)]
pub struct GroupMember {
    pub member_id: String,
    pub group_instance_id: Option<String>,
    pub client_id: String,
    pub client_host: String,
    pub metadata: Option<Vec<u8>>,
    pub assignment: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct OffsetInfo {
    pub offset: i64,
    pub metadata: Option<String>,
    pub timestamp: i64,
}

#[derive(Debug, Default)]
pub struct ConsumerGroupState {
    pub groups: HashMap<String, ConsumerGroup>,
}

use crate::types::{
    ConfigEntry, ConfigSynonym, config_source, config_type,
    DescribeConfigsResponse
};

/// Response for an API request
pub struct Response {
    pub header: ResponseHeader,
    pub body: Bytes,
    pub is_flexible: bool,  // Whether response HEADER should have tagged fields (not about body encoding)
    pub api_key: ApiKey,    // Track which API this response is for
}

/// Handler for specific API versions request
pub struct ApiVersionsRequest {
    // Empty for v0-3
}

/// Response for API versions
pub struct ApiVersionsResponse {
    pub error_code: i16,
    pub api_versions: Vec<ApiVersionInfo>,
    pub throttle_time_ms: i32,
}

/// Information about a supported API
pub struct ApiVersionInfo {
    pub api_key: i16,
    pub min_version: i16,
    pub max_version: i16,
}

/// Handles Kafka protocol requests.
pub struct ProtocolHandler {
    supported_versions: HashMap<ApiKey, VersionRange>,
    /// Optional metadata store for topic management
    metadata_store: Option<Arc<dyn chronik_common::metadata::traits::MetadataStore>>,
    /// Broker ID for this instance
    broker_id: i32,
    /// Consumer group state
    consumer_groups: Arc<Mutex<ConsumerGroupState>>,
}

impl ProtocolHandler {
    /// Helper to create a Response with proper flexible tracking
    fn make_response(header: &RequestHeader, api_key: ApiKey, body: Bytes) -> Response {
        // CRITICAL: ApiVersions is special - the response BODY uses flexible encoding
        // but the response HEADER does not! This is unique to ApiVersions.
        // 
        // For clarity: is_flexible here means "should the header have tagged fields"
        // The body encoding (flexible vs non-flexible) is already handled when creating the body
        let header_has_tagged_fields = if api_key == ApiKey::ApiVersions {
            false  // ApiVersions response header NEVER has tagged fields, even for v3+
        } else {
            is_flexible_version(api_key, header.api_version)
        };
        
        eprintln!("DEBUG: make_response - api_key={:?}, body.len()={}, header_has_tagged_fields={}", 
                 api_key, body.len(), header_has_tagged_fields);
        
        Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body,
            is_flexible: header_has_tagged_fields,  // This specifically means header flexibility
            api_key,
        }
    }
    
    /// Create a new protocol handler.
    pub fn new() -> Self {
        Self {
            supported_versions: supported_api_versions(),
            metadata_store: None,
            broker_id: 1, // Default broker ID
            consumer_groups: Arc::new(Mutex::new(ConsumerGroupState::default())),
        }
    }
    
    /// Create a new protocol handler with metadata store
    pub fn with_metadata_store(metadata_store: Arc<dyn chronik_common::metadata::traits::MetadataStore>) -> Self {
        let versions = supported_api_versions();
        Self {
            supported_versions: versions,
            metadata_store: Some(metadata_store),
            broker_id: 1, // Default broker ID
            consumer_groups: Arc::new(Mutex::new(ConsumerGroupState::default())),
        }
    }
    
    /// Create a new protocol handler with metadata store and broker ID
    pub fn with_metadata_and_broker(
        metadata_store: Arc<dyn chronik_common::metadata::traits::MetadataStore>,
        broker_id: i32,
    ) -> Self {
        let versions = supported_api_versions();
        Self {
            supported_versions: versions,
            metadata_store: Some(metadata_store),
            broker_id,
            consumer_groups: Arc::new(Mutex::new(ConsumerGroupState::default())),
        }
    }
    
    // Public parse methods for use by KafkaProtocolHandler
    
    /// Parse FindCoordinator request
    pub fn parse_find_coordinator_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::find_coordinator_types::FindCoordinatorRequest> {
        use crate::parser::Decoder;
        use crate::find_coordinator_types::FindCoordinatorRequest;
        
        let mut decoder = Decoder::new(body);
        
        let key = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Coordinator key cannot be null".into()))?;
        
        let key_type = if header.api_version >= 1 {
            decoder.read_i8()?
        } else {
            0 // GROUP type
        };
        
        Ok(FindCoordinatorRequest { key, key_type })
    }
    
    /// Parse JoinGroup request
    pub fn parse_join_group_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::join_group_types::JoinGroupRequest> {
        use crate::parser::Decoder;
        use crate::join_group_types::{JoinGroupRequest, JoinGroupRequestProtocol};
        
        let mut decoder = Decoder::new(body);
        
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        let session_timeout_ms = decoder.read_i32()?;
        let rebalance_timeout_ms = if header.api_version >= 1 {
            decoder.read_i32()?
        } else {
            session_timeout_ms
        };
        let member_id = decoder.read_string()?.unwrap_or_default();
        let group_instance_id = if header.api_version >= 5 {
            decoder.read_string()?
        } else {
            None
        };
        let protocol_type = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Protocol type cannot be null".into()))?;
        
        let protocol_count = decoder.read_i32()? as usize;
        let mut protocols = Vec::with_capacity(protocol_count);
        for _ in 0..protocol_count {
            let name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Protocol name cannot be null".into()))?;
            let metadata = decoder.read_bytes()?
                .ok_or_else(|| Error::Protocol("Protocol metadata cannot be null".into()))?;
            protocols.push(JoinGroupRequestProtocol { name, metadata });
        }
        
        Ok(JoinGroupRequest {
            group_id,
            session_timeout_ms,
            rebalance_timeout_ms,
            member_id,
            group_instance_id,
            protocol_type,
            protocols,
        })
    }
    
    /// Parse SyncGroup request
    pub fn parse_sync_group_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::sync_group_types::SyncGroupRequest> {
        use crate::parser::Decoder;
        use crate::sync_group_types::{SyncGroupRequest, SyncGroupRequestAssignment};
        
        let mut decoder = Decoder::new(body);
        
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        let generation_id = decoder.read_i32()?;
        let member_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;
        let group_instance_id = if header.api_version >= 3 {
            decoder.read_string()?
        } else {
            None
        };
        
        let protocol_type = if header.api_version >= 5 {
            decoder.read_string()?
        } else {
            None
        };
        
        let protocol_name = if header.api_version >= 5 {
            decoder.read_string()?
        } else {
            None
        };
        
        let assignment_count = decoder.read_i32()? as usize;
        let mut assignments = Vec::with_capacity(assignment_count);
        for _ in 0..assignment_count {
            let member_id = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Assignment member ID cannot be null".into()))?;
            let assignment = decoder.read_bytes()?
                .ok_or_else(|| Error::Protocol("Assignment cannot be null".into()))?;
            assignments.push(SyncGroupRequestAssignment { member_id, assignment });
        }
        
        Ok(SyncGroupRequest {
            group_id,
            generation_id,
            member_id,
            group_instance_id,
            protocol_type,
            protocol_name,
            assignments,
        })
    }
    
    /// Parse Heartbeat request
    pub fn parse_heartbeat_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::heartbeat_types::HeartbeatRequest> {
        use crate::parser::Decoder;
        use crate::heartbeat_types::HeartbeatRequest;
        
        let mut decoder = Decoder::new(body);
        
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        let generation_id = decoder.read_i32()?;
        let member_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;
        let group_instance_id = if header.api_version >= 3 {
            decoder.read_string()?
        } else {
            None
        };
        
        Ok(HeartbeatRequest {
            group_id,
            generation_id,
            member_id,
            group_instance_id,
        })
    }
    
    /// Parse LeaveGroup request
    pub fn parse_leave_group_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::leave_group_types::LeaveGroupRequest> {
        use crate::parser::Decoder;
        use crate::leave_group_types::{LeaveGroupRequest, MemberIdentity};
        
        let mut decoder = Decoder::new(body);
        
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        
        let members = if header.api_version >= 3 {
            // V3+ has array of members
            let member_count = decoder.read_i32()? as usize;
            let mut members = Vec::with_capacity(member_count);
            for _ in 0..member_count {
                let member_id = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;
                let group_instance_id = decoder.read_string()?;
                members.push(MemberIdentity { member_id, group_instance_id });
            }
            members
        } else {
            // V0-2 has single member_id
            let member_id = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;
            vec![MemberIdentity { member_id, group_instance_id: None }]
        };
        
        Ok(LeaveGroupRequest { group_id, members })
    }
    
    /// Parse OffsetCommit request
    pub fn parse_offset_commit_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::types::OffsetCommitRequest> {
        use crate::parser::Decoder;
        use crate::types::{OffsetCommitRequest, OffsetCommitTopic, OffsetCommitPartition};
        
        let mut decoder = Decoder::new(body);
        
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        
        let generation_id = decoder.read_i32()?;
        let member_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;
        
        // V2+ has retention_time field
        if header.api_version >= 2 && header.api_version <= 4 {
            let _retention_time = decoder.read_i64()?; // We ignore this for now
        }
        
        // Read topics array
        let topic_count = decoder.read_i32()?;
        if topic_count < 0 || topic_count > 10000 {
            return Err(Error::Protocol(format!("Invalid topic count: {}", topic_count)));
        }
        let mut topics = Vec::with_capacity(topic_count as usize);
        
        for _ in 0..topic_count {
            let topic_name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
            
            let partition_count = decoder.read_i32()?;
            if partition_count < 0 || partition_count > 10000 {
                return Err(Error::Protocol(format!("Invalid partition count: {}", partition_count)));
            }
            let mut partitions = Vec::with_capacity(partition_count as usize);
            
            for _ in 0..partition_count {
                let partition_index = decoder.read_i32()?;
                let committed_offset = decoder.read_i64()?;
                let committed_metadata = decoder.read_string()?;
                
                partitions.push(OffsetCommitPartition {
                    partition_index,
                    committed_offset,
                    committed_metadata,
                });
            }
            
            topics.push(OffsetCommitTopic {
                name: topic_name,
                partitions,
            });
        }
        
        Ok(OffsetCommitRequest {
            group_id,
            generation_id,
            member_id,
            topics,
        })
    }
    
    /// Parse OffsetFetch request
    pub fn parse_offset_fetch_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::types::OffsetFetchRequest> {
        use crate::parser::Decoder;
        use crate::types::OffsetFetchRequest;
        
        let mut decoder = Decoder::new(body);
        
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        
        // Read topics array (null means all topics)
        let topics = if header.api_version >= 1 {
            let topic_count = decoder.read_i32()?;
            if topic_count < 0 {
                // Null array means fetch all topics
                None
            } else {
                let mut topics = Vec::with_capacity(topic_count as usize);
                for _ in 0..topic_count {
                    let topic_name = decoder.read_string()?
                        .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
                    topics.push(topic_name);
                }
                Some(topics)
            }
        } else {
            // V0 doesn't have topics array, fetches all
            None
        };
        
        Ok(OffsetFetchRequest {
            group_id,
            topics,
        })
    }
    
    // Public encode methods for use by KafkaProtocolHandler
    
    /// Encode FindCoordinator response
    pub fn encode_find_coordinator_response(&self, buf: &mut BytesMut, response: &crate::find_coordinator_types::FindCoordinatorResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        encoder.write_i16(response.error_code);
        
        if version >= 1 {
            encoder.write_string(response.error_message.as_deref());
        }
        
        encoder.write_i32(response.node_id);
        encoder.write_string(Some(&response.host));
        encoder.write_i32(response.port);
        
        Ok(())
    }
    
    /// Encode JoinGroup response
    pub fn encode_join_group_response(&self, buf: &mut BytesMut, response: &crate::join_group_types::JoinGroupResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 2 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        encoder.write_i16(response.error_code);
        encoder.write_i32(response.generation_id);
        
        if version >= 7 {
            encoder.write_string(response.protocol_type.as_deref());
        }
        
        encoder.write_string(response.protocol_name.as_deref());
        encoder.write_string(Some(&response.leader));
        encoder.write_string(Some(&response.member_id));
        
        encoder.write_i32(response.members.len() as i32);
        for member in &response.members {
            encoder.write_string(Some(&member.member_id));
            
            if version >= 5 {
                encoder.write_string(member.group_instance_id.as_deref());
            }
            
            encoder.write_bytes(Some(&member.metadata));
        }
        
        Ok(())
    }
    
    /// Encode SyncGroup response
    pub fn encode_sync_group_response(&self, buf: &mut BytesMut, response: &crate::sync_group_types::SyncGroupResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        encoder.write_i16(response.error_code);
        
        if version >= 5 {
            encoder.write_string(response.protocol_type.as_deref());
            encoder.write_string(response.protocol_name.as_deref());
        }
        
        encoder.write_bytes(Some(&response.assignment));
        
        Ok(())
    }
    
    /// Encode Heartbeat response
    pub fn encode_heartbeat_response(&self, buf: &mut BytesMut, response: &crate::heartbeat_types::HeartbeatResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        encoder.write_i16(response.error_code);
        
        Ok(())
    }
    
    /// Encode LeaveGroup response
    pub fn encode_leave_group_response(&self, buf: &mut BytesMut, response: &crate::leave_group_types::LeaveGroupResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        encoder.write_i16(response.error_code);
        
        if version >= 3 {
            encoder.write_i32(response.members.len() as i32);
            for member in &response.members {
                encoder.write_string(Some(&member.member_id));
                encoder.write_string(member.group_instance_id.as_deref());
                encoder.write_i16(member.error_code);
            }
        }
        
        Ok(())
    }
    
    /// Encode OffsetCommit response
    pub fn encode_offset_commit_response(&self, buf: &mut BytesMut, response: &crate::types::OffsetCommitResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 3 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        encoder.write_i32(response.topics.len() as i32);
        for topic in &response.topics {
            encoder.write_string(Some(&topic.name));
            
            encoder.write_i32(topic.partitions.len() as i32);
            for partition in &topic.partitions {
                encoder.write_i32(partition.partition_index);
                encoder.write_i16(partition.error_code);
            }
        }
        
        Ok(())
    }
    
    /// Encode OffsetFetch response
    pub fn encode_offset_fetch_response(&self, buf: &mut BytesMut, response: &crate::types::OffsetFetchResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 3 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        encoder.write_i32(response.topics.len() as i32);
        for topic in &response.topics {
            encoder.write_string(Some(&topic.name));
            
            encoder.write_i32(topic.partitions.len() as i32);
            for partition in &topic.partitions {
                encoder.write_i32(partition.partition_index);
                encoder.write_i64(partition.committed_offset);
                encoder.write_string(partition.metadata.as_deref());
                encoder.write_i16(partition.error_code);
            }
        }
        
        if version >= 2 {
            encoder.write_i16(0); // error_code at response level
        }
        
        Ok(())
    }
    
    /// Parse Fetch request
    pub fn parse_fetch_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::types::FetchRequest> {
        use crate::parser::Decoder;
        use crate::types::{FetchRequest, FetchRequestTopic, FetchRequestPartition};
        
        let mut decoder = Decoder::new(body);
        
        let replica_id = decoder.read_i32()?;
        let max_wait_ms = decoder.read_i32()?;
        let min_bytes = decoder.read_i32()?;
        
        let max_bytes = if header.api_version >= 3 {
            decoder.read_i32()?
        } else {
            i32::MAX
        };
        
        let isolation_level = if header.api_version >= 4 {
            decoder.read_i8()?
        } else {
            0
        };
        
        let session_id = if header.api_version >= 7 {
            decoder.read_i32()?
        } else {
            0
        };
        
        let session_epoch = if header.api_version >= 7 {
            decoder.read_i32()?
        } else {
            -1
        };
        
        // Topics array
        let topic_count = decoder.read_i32()? as usize;
        let mut topics = Vec::with_capacity(topic_count);
        
        for _ in 0..topic_count {
            let name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
            
            // Partitions array
            let partition_count = decoder.read_i32()? as usize;
            let mut partitions = Vec::with_capacity(partition_count);
            
            for _ in 0..partition_count {
                let partition = decoder.read_i32()?;
                let current_leader_epoch = if header.api_version >= 9 {
                    decoder.read_i32()?
                } else {
                    -1
                };
                let fetch_offset = decoder.read_i64()?;
                let log_start_offset = if header.api_version >= 5 {
                    decoder.read_i64()?
                } else {
                    -1
                };
                let partition_max_bytes = decoder.read_i32()?;
                
                partitions.push(FetchRequestPartition {
                    partition,
                    current_leader_epoch,
                    fetch_offset,
                    log_start_offset,
                    partition_max_bytes,
                });
            }
            
            topics.push(FetchRequestTopic { name, partitions });
        }
        
        // Read and ignore forgotten topics data for v7+
        if header.api_version >= 7 {
            // Read forgotten topics array
            let count = decoder.read_i32()? as usize;
            for _ in 0..count {
                let _topic = decoder.read_string()?;
                let partition_count = decoder.read_i32()? as usize;
                for _ in 0..partition_count {
                    let _partition = decoder.read_i32()?;
                }
            }
        }
        
        // Read and ignore rack_id for v11+
        if header.api_version >= 11 {
            let _rack_id = decoder.read_string()?;
        }
        
        Ok(FetchRequest {
            replica_id,
            max_wait_ms,
            min_bytes,
            max_bytes,
            isolation_level,
            session_id,
            session_epoch,
            topics,
        })
    }
    
    /// Encode Fetch response
    pub fn encode_fetch_response(&self, buf: &mut BytesMut, response: &crate::types::FetchResponse, version: i16) -> Result<()> {
        let initial_len = buf.len();
        let mut encoder = Encoder::new(buf);
        
        tracing::debug!("Encoding Fetch response v{}", version);
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
            tracing::trace!("  throttle_time_ms: {}", response.throttle_time_ms);
        }
        
        if version >= 7 {
            encoder.write_i16(0); // error_code
            encoder.write_i32(0); // session_id
            tracing::trace!("  error_code: 0, session_id: 0");
        }
        
        // Topics array
        encoder.write_i32(response.topics.len() as i32);
        tracing::debug!("  Topics count: {}", response.topics.len());
        
        for topic in &response.topics {
            encoder.write_string(Some(&topic.name));
            tracing::trace!("    Topic: {}", topic.name);
            
            // Partitions array
            encoder.write_i32(topic.partitions.len() as i32);
            tracing::trace!("    Partitions count: {}", topic.partitions.len());
            
            for partition in &topic.partitions {
                encoder.write_i32(partition.partition);
                encoder.write_i16(partition.error_code);
                encoder.write_i64(partition.high_watermark);
                
                tracing::trace!("      Partition {}: error={}, hw={}", 
                    partition.partition, partition.error_code, partition.high_watermark);
                
                if version >= 4 {
                    encoder.write_i64(partition.last_stable_offset);
                    
                    if version >= 5 {
                        encoder.write_i64(partition.log_start_offset);
                        tracing::trace!("        lso={}, log_start={}", 
                            partition.last_stable_offset, partition.log_start_offset);
                    }
                    
                    // Aborted transactions (empty for now)
                    encoder.write_i32(0);
                }
                
                if version >= 11 {
                    encoder.write_i32(partition.preferred_read_replica);
                    tracing::trace!("        preferred_read_replica={}", partition.preferred_read_replica);
                }
                
                // Records
                // For the records field, write the actual bytes if we have them
                // or write an empty byte array (length 0) for no records
                let records_len = partition.records.len();
                if records_len == 0 {
                    // Write empty bytes (length 0) - not NULL
                    // This tells the client there are no records without causing parsing errors
                    tracing::trace!("        Records: empty (0 bytes)");
                    encoder.write_bytes(Some(&[]));
                } else {
                    tracing::debug!("        Records: {} bytes", records_len);
                    tracing::trace!("        Records data (first 32 bytes): {:?}", 
                        &partition.records[..std::cmp::min(32, records_len)]);
                    encoder.write_bytes(Some(&partition.records));
                }
            }
        }
        
        let total_encoded = buf.len() - initial_len;
        tracing::info!("Encoded Fetch response: {} bytes total", total_encoded);
        
        Ok(())
    }
    
    /// Handle a raw request and return a response
    pub async fn handle_request(&self, request_bytes: &[u8]) -> Result<Response> {
        let mut buf = Bytes::copy_from_slice(request_bytes);
        
        // Use the new parsing function that preserves correlation ID
        let header = match parse_request_header_with_correlation(&mut buf) {
            Ok((h, _)) => h,
            Err(Error::ProtocolWithCorrelation { correlation_id, message }) => {
                // Unknown API key but we have correlation ID
                tracing::warn!("Unknown API key error: {}, correlation_id: {}", message, correlation_id);
                return self.error_response(correlation_id, error_codes::UNSUPPORTED_VERSION);
            }
            Err(e) => {
                // Other parsing error, no correlation ID available
                tracing::error!("Failed to parse request header: {:?}", e);
                return Err(e);
            }
        };
        
        tracing::info!(
            "Parsed request header - API: {:?} ({}), Version: {}, Correlation ID: {}",
            header.api_key,
            header.api_key as i16,
            header.api_version,
            header.correlation_id
        );
        
        // Check if we support this API and version
        if let Some(version_range) = self.supported_versions.get(&header.api_key) {
            if header.api_version < version_range.min || header.api_version > version_range.max {
                return self.error_response(
                    header.correlation_id,
                    error_codes::UNSUPPORTED_VERSION,
                );
            }
        } else {
            return self.error_response(
                header.correlation_id,
                35, // UNSUPPORTED_VERSION
            );
        }
        
        // Route to appropriate handler
        match header.api_key {
            // Implemented APIs
            ApiKey::Produce => self.handle_produce(header, &mut buf).await,
            ApiKey::Fetch => self.handle_fetch(header, &mut buf).await,
            ApiKey::Metadata => self.handle_metadata(header, &mut buf).await,
            ApiKey::ApiVersions => self.handle_api_versions(header, &mut buf).await,
            ApiKey::DescribeConfigs => self.handle_describe_configs(header, &mut buf).await,
            
            // Consumer group APIs
            ApiKey::OffsetCommit => self.handle_offset_commit(header, &mut buf).await,
            ApiKey::OffsetFetch => self.handle_offset_fetch(header, &mut buf).await,
            ApiKey::FindCoordinator => self.handle_find_coordinator(header, &mut buf).await,
            ApiKey::JoinGroup => self.handle_join_group(header, &mut buf).await,
            ApiKey::Heartbeat => self.handle_heartbeat(header, &mut buf).await,
            ApiKey::LeaveGroup => self.unimplemented_api_response(header.correlation_id, "LeaveGroup"),
            ApiKey::SyncGroup => self.handle_sync_group(header, &mut buf).await,
            ApiKey::DescribeGroups => self.handle_describe_groups(header, &mut buf).await,
            ApiKey::ListGroups => self.handle_list_groups(header, &mut buf).await,
            
            // Administrative APIs - TODO: implement these
            ApiKey::CreateTopics => self.handle_create_topics(header, &mut buf).await,
            ApiKey::DeleteTopics => self.unimplemented_api_response(header.correlation_id, "DeleteTopics"),
            ApiKey::AlterConfigs => self.unimplemented_api_response(header.correlation_id, "AlterConfigs"),
            
            // Other APIs
            ApiKey::ListOffsets => self.handle_list_offsets(header, &mut buf).await,
            
            // SASL authentication (partial support for compatibility)
            ApiKey::SaslHandshake => self.handle_sasl_handshake(header, &mut buf).await,
            
            // Broker-to-broker APIs (not client-facing)
            ApiKey::LeaderAndIsr |
            ApiKey::StopReplica |
            ApiKey::UpdateMetadata |
            ApiKey::ControlledShutdown => {
                tracing::warn!("Received broker-to-broker API request: {:?}", header.api_key);
                self.error_response(header.correlation_id, error_codes::UNSUPPORTED_VERSION)
            }
            
            // Transaction APIs
            ApiKey::InitProducerId |
            ApiKey::AddPartitionsToTxn |
            ApiKey::AddOffsetsToTxn |
            ApiKey::EndTxn |
            ApiKey::WriteTxnMarkers |
            ApiKey::TxnOffsetCommit => {
                tracing::warn!("Received transaction API request: {:?}", header.api_key);
                self.error_response(header.correlation_id, error_codes::UNSUPPORTED_VERSION)  
            }
            
            // ACL APIs
            ApiKey::DescribeAcls |
            ApiKey::CreateAcls |
            ApiKey::DeleteAcls => {
                tracing::warn!("Received ACL API request: {:?}", header.api_key);
                self.error_response(header.correlation_id, error_codes::UNSUPPORTED_VERSION)
            }
            
            // Other APIs - catch all unimplemented APIs
            ApiKey::DeleteRecords |
            ApiKey::OffsetForLeaderEpoch |
            ApiKey::AlterReplicaLogDirs |
            ApiKey::DescribeLogDirs |
            ApiKey::SaslAuthenticate |
            ApiKey::CreatePartitions |
            ApiKey::CreateDelegationToken |
            ApiKey::RenewDelegationToken |
            ApiKey::ExpireDelegationToken |
            ApiKey::DescribeDelegationToken |
            ApiKey::DeleteGroups |
            ApiKey::ElectLeaders |
            ApiKey::IncrementalAlterConfigs |
            ApiKey::AlterPartitionReassignments |
            ApiKey::ListPartitionReassignments |
            ApiKey::OffsetDelete |
            ApiKey::DescribeClientQuotas |
            ApiKey::AlterClientQuotas |
            ApiKey::DescribeUserScramCredentials |
            ApiKey::AlterUserScramCredentials |
            // ApiKey::Vote |  // API 52 - not in CP Kafka
            // ApiKey::BeginQuorumEpoch |  // API 53 - not in CP Kafka
            // ApiKey::EndQuorumEpoch |  // API 54 - not in CP Kafka
            // ApiKey::DescribeQuorum |  // API 55 - not in CP Kafka
            ApiKey::AlterPartition |
            ApiKey::UpdateFeatures |
            ApiKey::Envelope |
            // ApiKey::FetchSnapshot |  // API 59 - not in CP Kafka
            ApiKey::DescribeCluster |
            ApiKey::DescribeProducers |
            // ApiKey::BrokerRegistration |  // API 62 - not in CP Kafka
            // ApiKey::BrokerHeartbeat |  // API 63 - not in CP Kafka
            // ApiKey::UnregisterBroker |  // API 64 - not in CP Kafka
            ApiKey::DescribeTransactions |
            ApiKey::ListTransactions |
            ApiKey::AllocateProducerIds => {
            // ApiKey::ConsumerGroupHeartbeat => {  // API 68 - not in CP Kafka
                tracing::warn!("Received unsupported API request: {:?}", header.api_key);
                self.error_response(header.correlation_id, error_codes::UNSUPPORTED_VERSION)
            }
        }
    }
    
    /// Handle ApiVersions request
    async fn handle_api_versions(
        &self,
        header: RequestHeader,
        _body: &mut Bytes,
    ) -> Result<Response> {
        if self.supported_versions.is_empty() {
            // Use the directly fetched versions instead of minimal list
            let test_versions = supported_api_versions();
            let mut api_versions = Vec::new();
            for (api_key, version_range) in &test_versions {
                api_versions.push(ApiVersionInfo {
                    api_key: *api_key as i16,
                    min_version: version_range.min,
                    max_version: version_range.max,
                });
            }
            // CRITICAL: Sort by API key for consistent ordering
            api_versions.sort_by_key(|a| a.api_key);
            
            let response = ApiVersionsResponse {
                error_code: 0,
                api_versions,
                throttle_time_ms: 0,
            };
            
            let mut body_buf = BytesMut::new();
            self.encode_api_versions_response(&mut body_buf, &response, header.api_version)?;
            
            eprintln!("DEBUG: After encode, body_buf first 32 bytes: {:?}", &body_buf[..body_buf.len().min(32)]);
            let encoded_bytes = body_buf.freeze();
            eprintln!("DEBUG: After freeze, encoded_bytes first 32 bytes: {:?}", &encoded_bytes[..encoded_bytes.len().min(32)]);
            let response = Self::make_response(&header, ApiKey::ApiVersions, encoded_bytes);
            eprintln!("DEBUG: Response body first 32 bytes: {:?}", &response.body[..response.body.len().min(32)]);
            return Ok(response);
        }
        
        
        let mut api_versions = Vec::new();
        
        for (api_key, version_range) in &self.supported_versions {
            api_versions.push(ApiVersionInfo {
                api_key: *api_key as i16,
                min_version: version_range.min,
                max_version: version_range.max,
            });
        }
        // CRITICAL: Sort by API key for consistent ordering
        api_versions.sort_by_key(|a| a.api_key);
        
        let response = ApiVersionsResponse {
            error_code: 0,
            api_versions,
            throttle_time_ms: 0,
        };
        
        let mut body_buf = BytesMut::new();
        
        // Encode the response body (without correlation ID)
        self.encode_api_versions_response(&mut body_buf, &response, header.api_version)?;
        
        let encoded_bytes = body_buf.freeze();
        
        Ok(Self::make_response(&header, ApiKey::ApiVersions, encoded_bytes))
    }
    
    /// Encode ApiVersions response
    fn encode_api_versions_response(
        &self,
        buf: &mut BytesMut,
        response: &ApiVersionsResponse,
        version: i16,
    ) -> Result<()> {
        eprintln!("DEBUG encode_api_versions_response: version={}, api_count={}", version, response.api_versions.len());
        let mut encoder = Encoder::new(buf);
        
        // CRITICAL: For v0 protocol, field order matters!
        // v0: api_versions array comes BEFORE error_code
        // v1+: error_code comes first (standard field ordering)
        
        if version == 0 {
            // v0 field order: api_versions array, then error_code
            
            // Write array of API versions first
            encoder.write_i32(response.api_versions.len() as i32);
            for api in &response.api_versions {
                encoder.write_i16(api.api_key);
                encoder.write_i16(api.min_version);
                encoder.write_i16(api.max_version);
            }
            
            // Then write error_code
            encoder.write_i16(response.error_code);
        } else {
            // v1+ field order depends on version:
            // v1-2: throttle_time_ms, error_code, api_versions
            // v3: error_code, api_versions (NO throttle_time_ms according to spec)
            // v4+: throttle_time_ms, error_code, api_versions
            
            // Write throttle_time_ms for v1-2 and v4+, but NOT v3
            if version >= 1 && version <= 2 {
                encoder.write_i32(response.throttle_time_ms);
            } else if version >= 4 {
                encoder.write_i32(response.throttle_time_ms);
            }
            
            // Then error_code
            eprintln!("DEBUG: About to write error_code = {}", response.error_code);
            eprintln!("DEBUG: Buffer before error_code: {:?}", encoder.debug_buffer());
            encoder.write_i16(response.error_code);
            eprintln!("DEBUG: Buffer after error_code: {:?}", encoder.debug_buffer());
            
            // Write array of API versions
            eprintln!("DEBUG: version = {}, will use compact = {}", version, version >= 3);
            if version >= 3 {
                // Use compact array encoding
                // Compact arrays use length+1 encoding
                let array_len = (response.api_versions.len() + 1) as u32;
                eprintln!("DEBUG: IN COMPACT PATH - Writing compact array for {} APIs, varint value = {}", response.api_versions.len(), array_len);
                eprintln!("DEBUG: Buffer before varint: {:?}", encoder.debug_buffer());
                encoder.write_unsigned_varint(array_len);
                eprintln!("DEBUG: Buffer after varint: {:?}", encoder.debug_buffer());
                
                for (i, api) in response.api_versions.iter().enumerate() {
                    // CRITICAL: librdkafka compatibility issue
                    // Kafka spec says v3+ uses INT8 for min/max, but librdkafka v2.11.1
                    // seems to expect INT16. Let's try INT16 for compatibility.
                    if i == 0 {
                        eprintln!("DEBUG: First API - key={}, min={}, max={}", api.api_key, api.min_version, api.max_version);
                    }
                    encoder.write_i16(api.api_key);
                    
                    // Try INT16 for librdkafka compatibility
                    encoder.write_i16(api.min_version);
                    encoder.write_i16(api.max_version);
                    
                    // Write tagged fields (empty for now)
                    encoder.write_unsigned_varint(0);
                }
            } else {
                encoder.write_i32(response.api_versions.len() as i32);
                
                for api in &response.api_versions {
                    encoder.write_i16(api.api_key);
                    encoder.write_i16(api.min_version);
                    encoder.write_i16(api.max_version);
                }
            }
            
            // CRITICAL FIX: librdkafka expects throttle_time_ms at the end for v3
            // The error shows it expects 4 bytes at position 423/424
            if version == 3 {
                eprintln!("DEBUG: v3 - Writing throttle_time THEN tagged fields");
                encoder.write_i32(response.throttle_time_ms);
                encoder.write_unsigned_varint(0);
            }
            
            // v4+ has tagged fields at the end (and throttle_time is earlier)
            if version >= 4 {
                eprintln!("DEBUG: Writing end tagged field for version {}", version);
                // Write tagged fields at the end (empty for now)
                encoder.write_unsigned_varint(0);
            }
        }
        
        eprintln!("DEBUG: Final buffer in encode_api_versions_response: len={}, first 32 bytes: {:?}", 
            encoder.debug_buffer().len(),
            &encoder.debug_buffer()[..encoder.debug_buffer().len().min(32)]);
        Ok(())
    }
    
    /// Handle Metadata request
    async fn handle_metadata(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::types::{MetadataRequest, MetadataResponse, MetadataBroker};
        use crate::parser::Decoder;
        
        eprintln!("DEBUG: handle_metadata called with v{}", header.api_version);
        tracing::debug!("Handling metadata request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        let flexible = header.api_version >= 9;
        
        // Parse metadata request
        eprintln!("DEBUG: Parsing metadata request, flexible={}", flexible);
        let topics = if header.api_version >= 1 {
            let topic_count = if flexible {
                // Compact array
                let count = match decoder.read_unsigned_varint() {
                    Ok(c) => c as i32,
                    Err(e) => {
                        eprintln!("DEBUG: Failed to read topic count varint: {:?}", e);
                        return Err(e);
                    }
                };
                eprintln!("DEBUG: Topic count varint read: {}", count);
                if count == 0 {
                    -1  // Null array
                } else {
                    count - 1  // Compact arrays use +1 encoding
                }
            } else {
                decoder.read_i32()?
            };
            
            eprintln!("DEBUG: Topic count decoded: {}", topic_count);
            if topic_count < 0 {
                None // All topics
            } else {
                let mut topic_names = Vec::with_capacity(topic_count as usize);
                for i in 0..topic_count {
                    eprintln!("DEBUG: Reading topic {}/{}", i+1, topic_count);
                    
                    // v10+ includes topic_id (UUID) before name
                    if header.api_version >= 10 {
                        // Skip the 16-byte UUID
                        let mut topic_id = [0u8; 16];
                        for j in 0..16 {
                            topic_id[j] = decoder.read_i8()? as u8;
                        }
                        eprintln!("DEBUG: Topic {} ID: {:02x?}", i, &topic_id);
                    }
                    
                    let name = if flexible {
                        eprintln!("DEBUG: Reading compact string for topic name");
                        decoder.read_compact_string()?
                    } else {
                        decoder.read_string()?
                    };
                    
                    eprintln!("DEBUG: Topic {} name: {:?}", i, name);
                    
                    if let Some(name) = name {
                        topic_names.push(name);
                    }
                    
                    if flexible {
                        // Skip tagged fields for each topic
                        let tagged_count = decoder.read_unsigned_varint()?;
                        eprintln!("DEBUG: Topic {} has {} tagged fields", i, tagged_count);
                    }
                }
                Some(topic_names)
            }
        } else {
            // v0 gets all topics
            None
        };
        
        let allow_auto_topic_creation = if header.api_version >= 4 {
            let val = decoder.read_bool()?;
            eprintln!("DEBUG: allow_auto_topic_creation read: {}", val);
            val
        } else {
            true
        };
        
        // include_cluster_authorized_operations was removed in v11
        let include_cluster_authorized_operations = if header.api_version >= 8 && header.api_version <= 10 {
            let val = decoder.read_bool()?;
            eprintln!("DEBUG: include_cluster_authorized_operations read: {}", val);
            val
        } else {
            false
        };
        
        let include_topic_authorized_operations = if header.api_version >= 8 {
            let val = decoder.read_bool()?;
            eprintln!("DEBUG: include_topic_authorized_operations read: {}", val);
            val
        } else {
            false
        };
        
        if flexible {
            // Skip tagged fields at the end
            // librdkafka v2.11.1 quirk: First Metadata request may be missing tagged fields
            if decoder.has_remaining() {
                match decoder.read_unsigned_varint() {
                    Ok(count) => eprintln!("DEBUG: Body tagged field count: {}", count),
                    Err(e) => {
                        eprintln!("DEBUG: Failed to read body tagged fields: {:?}", e);
                        return Err(e);
                    }
                }
            } else {
                eprintln!("DEBUG: No body tagged fields present (librdkafka compatibility)");
            }
        }
        
        let _request = MetadataRequest {
            topics,
            allow_auto_topic_creation,
            include_cluster_authorized_operations,
            include_topic_authorized_operations,
        };
        
        eprintln!("DEBUG: Parsed metadata request - topics: {:?}, auto_create: {}", 
                  _request.topics, _request.allow_auto_topic_creation);
        
        // Create response with topics from metadata store
        tracing::info!("Creating metadata response - topics requested: {:?}, auto_create: {}", 
            _request.topics, _request.allow_auto_topic_creation);
        
        // Handle auto-topic creation if enabled
        if _request.allow_auto_topic_creation {
            if let Some(requested_topics) = &_request.topics {
                // Check which topics don't exist and create them
                self.auto_create_topics(requested_topics).await?;
            }
        }
        
        // Get topics from metadata store (including newly created ones)
        let mut topics = match self.get_topics_from_metadata(&_request.topics).await {
            Ok(topics) => topics,
            Err(e) => {
                tracing::error!("Failed to get topics from metadata: {:?}", e);
                vec![]
            }
        };
        
        // CRITICAL FIX: Ensure at least one topic exists for clients to connect
        // Without this, Kafka clients cannot establish connections as they require
        // at least one topic in metadata responses
        if topics.is_empty() && _request.allow_auto_topic_creation {
            tracing::info!("No topics exist, creating default topic 'chronik-default' for client compatibility");
            
            // Create a default topic
            if let Err(e) = self.auto_create_topics(&["chronik-default".to_string()]).await {
                tracing::error!("Failed to auto-create default topic: {:?}", e);
            }
            
            // Try to get topics again after creating default
            topics = match self.get_topics_from_metadata(&_request.topics).await {
                Ok(topics) => topics,
                Err(e) => {
                    tracing::error!("Failed to get topics after creating default: {:?}", e);
                    
                    // As a last resort, return a fake topic so clients can at least connect
                    vec![crate::types::MetadataTopic {
                        error_code: 0,
                        name: "chronik-default".to_string(),
                        is_internal: false,
                        partitions: vec![crate::types::MetadataPartition {
                            error_code: 0,
                            partition_index: 0,
                            leader_id: self.broker_id,
                            leader_epoch: 0,
                            replica_nodes: vec![self.broker_id],
                            isr_nodes: vec![self.broker_id],
                            offline_replicas: vec![],
                        }],
                    }]
                }
            };
        }
        
        // Get brokers from metadata store
        let brokers = if let Some(metadata_store) = &self.metadata_store {
            match metadata_store.list_brokers().await {
                Ok(broker_metas) => {
                    // Filter out broker 0 with 0.0.0.0 - this is a phantom broker
                    let brokers: Vec<MetadataBroker> = broker_metas.into_iter()
                        .filter(|b| !(b.broker_id == 0 && b.host == "0.0.0.0"))
                        .map(|b| MetadataBroker {
                            node_id: b.broker_id,
                            host: b.host,
                            port: b.port,
                            rack: b.rack,
                        }).collect();
                    tracing::info!("Got {} brokers from metadata store (filtered)", brokers.len());
                    for b in &brokers {
                        tracing::info!("  Broker {}: {}:{}", b.node_id, b.host, b.port);
                    }
                    brokers
                }
                Err(e) => {
                    tracing::error!("Failed to get brokers from metadata: {:?}", e);
                    // Fallback to current broker if we can't get from metadata store
                    vec![MetadataBroker {
                        node_id: self.broker_id,
                        host: "localhost".to_string(),
                        port: 9092,
                        rack: None,
                    }]
                }
            }
        } else {
            tracing::warn!("No metadata store available, using default broker");
            // No metadata store, use current broker
            vec![MetadataBroker {
                node_id: self.broker_id,
                host: "localhost".to_string(),
                port: 9092,
                rack: None,
            }]
        };
        
        let response = MetadataResponse {
            correlation_id: header.correlation_id,
            throttle_time_ms: 0,
            brokers,
            cluster_id: Some("chronik-stream".to_string()),
            controller_id: self.broker_id, // Use actual broker ID
            topics,
        };
        tracing::info!("Metadata response has {} topics and {} brokers", 
                      response.topics.len(), response.brokers.len());
        
        // Debug: Log broker details
        for (i, broker) in response.brokers.iter().enumerate() {
            tracing::debug!("  Broker {}: id={}, host={}, port={}", 
                           i, broker.node_id, broker.host, broker.port);
        }
        
        let mut body_buf = BytesMut::new();
        
        eprintln!("DEBUG: About to encode metadata response - brokers={}, topics={}", 
                  response.brokers.len(), response.topics.len());
        tracing::info!("About to encode metadata response with {} brokers", response.brokers.len());
        // Encode the response body (without correlation ID)
        match self.encode_metadata_response(&mut body_buf, &response, header.api_version) {
            Ok(_) => eprintln!("DEBUG: encode_metadata_response succeeded, body_buf.len()={}", body_buf.len()),
            Err(e) => {
                eprintln!("DEBUG: encode_metadata_response failed: {:?}", e);
                return Err(e);
            }
        }
        
        eprintln!("DEBUG: Before make_response, body_buf.len()={}", body_buf.len());
        Ok(Self::make_response(&header, ApiKey::Metadata, body_buf.freeze()))
    }
    
    /// Parse a produce request from bytes
    pub fn parse_produce_request(
        &self,
        header: &RequestHeader,
        body: &mut Bytes,
    ) -> Result<crate::types::ProduceRequest> {
        use crate::types::ProduceRequest;
        use crate::parser::Decoder;
        
        // Debug log the raw bytes for diagnostics
        if body.len() < 100 {
            tracing::debug!("Produce request v{} body ({} bytes): {:02x?}", 
                header.api_version, body.len(), body.as_ref());
        } else {
            tracing::debug!("Produce request v{} body (first 100 of {} bytes): {:02x?}", 
                header.api_version, body.len(), &body.as_ref()[..100]);
        }
        
        let mut decoder = Decoder::new(body);
        
        // Check if this is a flexible/compact version (v9+)
        let flexible = header.api_version >= 9;
        tracing::debug!("Produce v{}: flexible={}", header.api_version, flexible);
        
        // Parse produce request based on version
        let transactional_id = if header.api_version >= 3 {
            tracing::debug!("Reading transactional_id, remaining bytes: {}", decoder.remaining());
            // v9+ uses compact string for transactional_id
            let id = if flexible {
                tracing::debug!("DEBUG: Using compact string for transactional_id (flexible=true)");
                decoder.read_compact_string()?
            } else {
                tracing::debug!("DEBUG: Using normal string for transactional_id (flexible=false)");
                decoder.read_string()?
            };
            tracing::debug!("transactional_id: {:?}", id);
            id
        } else {
            None
        };
        
        tracing::debug!("Reading acks, remaining bytes: {}", decoder.remaining());
        let acks = decoder.read_i16()?;
        tracing::debug!("acks: {}", acks);
        
        tracing::debug!("Reading timeout_ms, remaining bytes: {}", decoder.remaining());
        let timeout_ms = decoder.read_i32()?;
        tracing::debug!("timeout_ms: {}", timeout_ms);
        
        // Read topics array
        tracing::debug!("Reading topic count, remaining bytes: {}", decoder.remaining());
        // v9+ uses compact array
        let topic_count = if flexible {
            let count = decoder.read_unsigned_varint()? as i32 - 1;
            if count < 0 {
                0
            } else {
                count as usize
            }
        } else {
            decoder.read_i32()? as usize
        };
        tracing::debug!("Topic count: {}", topic_count);
        let mut topics = Vec::with_capacity(topic_count);
        
        for topic_idx in 0..topic_count {
            tracing::debug!("Reading topic {}/{}, remaining bytes: {}", topic_idx + 1, topic_count, decoder.remaining());
            // v9+ uses compact string for topic name
            let topic_name = if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }.ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
            tracing::debug!("Topic name: {}", topic_name);
            
            // Read partitions array
            tracing::debug!("Reading partition count for topic '{}', remaining bytes: {}", topic_name, decoder.remaining());
            // v9+ uses compact array
            let partition_count = if flexible {
                let count = decoder.read_unsigned_varint()? as i32 - 1;
                if count < 0 {
                    0
                } else {
                    count as usize
                }
            } else {
                decoder.read_i32()? as usize
            };
            tracing::debug!("Partition count: {}", partition_count);
            let mut partitions = Vec::with_capacity(partition_count);
            
            for part_idx in 0..partition_count {
                tracing::debug!("Reading partition {}/{}, remaining bytes: {}", part_idx + 1, partition_count, decoder.remaining());
                let partition_index = decoder.read_i32()?;
                tracing::debug!("Partition index: {}", partition_index);
                
                tracing::debug!("Reading records, remaining bytes: {}", decoder.remaining());
                let records_opt = if flexible {
                    decoder.read_compact_bytes()?
                } else {
                    decoder.read_bytes()?
                };
                
                // Allow null records (common for connectivity checks or flush operations)
                let records = records_opt.map(|r| r.to_vec()).unwrap_or_else(Vec::new);
                tracing::debug!("Records size: {} bytes", records.len());
                
                partitions.push(crate::types::ProduceRequestPartition {
                    index: partition_index,
                    records,
                });
                
                // Skip tagged fields in partitions for v9+
                if flexible {
                    let tag_count = decoder.read_unsigned_varint()?;
                    tracing::debug!("Partition tagged fields: {}", tag_count);
                    // Skip any tagged fields  
                    for _ in 0..tag_count {
                        let tag_id = decoder.read_unsigned_varint()?;
                        let tag_size = decoder.read_unsigned_varint()? as usize;
                        decoder.advance(tag_size);
                    }
                }
            }
            
            // Skip tagged fields in topics for v9+
            if flexible {
                let tag_count = decoder.read_unsigned_varint()?;
                tracing::debug!("Topic tagged fields: {}", tag_count);
                // Skip any tagged fields
                for _ in 0..tag_count {
                    let tag_id = decoder.read_unsigned_varint()?;
                    let tag_size = decoder.read_unsigned_varint()? as usize;
                    decoder.advance(tag_size);
                }
            }
            
            topics.push(crate::types::ProduceRequestTopic {
                name: topic_name,
                partitions,
            });
        }
        
        // Skip tagged fields at request level for v9+
        if flexible {
            let tag_count = decoder.read_unsigned_varint()?;
            tracing::debug!("Request tagged fields: {}", tag_count);
            // Skip any tagged fields
            for _ in 0..tag_count {
                let tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size);
            }
        }
        
        Ok(ProduceRequest {
            transactional_id,
            acks,
            timeout_ms,
            topics,
        })
    }
    
    /// Handle Produce request
    async fn handle_produce(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::types::{ProduceResponse, ProduceResponseTopic, ProduceResponsePartition};
        
        let request = self.parse_produce_request(&header, body)?;
        
        // For now, return a simple success response
        let response_topics = request.topics.into_iter().map(|topic| {
            ProduceResponseTopic {
                name: topic.name,
                partitions: topic.partitions.into_iter().map(|p| {
                    ProduceResponsePartition {
                        index: p.index,
                        error_code: 0,
                        base_offset: 0,
                        log_append_time: -1,
                        log_start_offset: 0,
                    }
                }).collect(),
            }
        }).collect();
        
        let response = ProduceResponse {
            header: ResponseHeader { correlation_id: header.correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_produce_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::Produce, body_buf.freeze()))
    }
    
    /// Handle Fetch request
    async fn handle_fetch(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::types::{FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition};
        use crate::parser::Decoder;
        
        let mut decoder = Decoder::new(body);
        
        // Parse fetch request
        let replica_id = decoder.read_i32()?;
        let max_wait_ms = decoder.read_i32()?;
        let min_bytes = decoder.read_i32()?;
        
        let max_bytes = if header.api_version >= 3 {
            decoder.read_i32()?
        } else {
            i32::MAX
        };
        
        let isolation_level = if header.api_version >= 4 {
            decoder.read_i8()?
        } else {
            0
        };
        
        let session_id = if header.api_version >= 7 {
            decoder.read_i32()?
        } else {
            0
        };
        
        let session_epoch = if header.api_version >= 7 {
            decoder.read_i32()?
        } else {
            -1
        };
        
        // Read topics array
        let topic_count = decoder.read_i32()? as usize;
        let mut topics = Vec::with_capacity(topic_count);
        
        for _ in 0..topic_count {
            let topic_name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
            
            // Read partitions array
            let partition_count = decoder.read_i32()? as usize;
            let mut partitions = Vec::with_capacity(partition_count);
            
            for _ in 0..partition_count {
                let partition = decoder.read_i32()?;
                let current_leader_epoch = if header.api_version >= 9 {
                    decoder.read_i32()?
                } else {
                    -1
                };
                let fetch_offset = decoder.read_i64()?;
                let log_start_offset = if header.api_version >= 5 {
                    decoder.read_i64()?
                } else {
                    -1
                };
                let partition_max_bytes = decoder.read_i32()?;
                
                partitions.push(crate::types::FetchRequestPartition {
                    partition,
                    current_leader_epoch,
                    fetch_offset,
                    log_start_offset,
                    partition_max_bytes,
                });
            }
            
            topics.push(crate::types::FetchRequestTopic {
                name: topic_name,
                partitions,
            });
        }
        
        let request = FetchRequest {
            replica_id,
            max_wait_ms,
            min_bytes,
            max_bytes,
            isolation_level,
            session_id,
            session_epoch,
            topics,
        };
        
        // For now, return empty response
        let response_topics = request.topics.into_iter().map(|topic| {
            FetchResponseTopic {
                name: topic.name,
                partitions: topic.partitions.into_iter().map(|p| {
                    FetchResponsePartition {
                        partition: p.partition,
                        error_code: 0,
                        high_watermark: 0,
                        last_stable_offset: 0,
                        log_start_offset: 0,
                        aborted: None,
                        preferred_read_replica: -1,
                        records: vec![],
                    }
                }).collect(),
            }
        }).collect();
        
        let response = FetchResponse {
            header: ResponseHeader { correlation_id: header.correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
        };
        
        tracing::info!("Building Fetch response with correlation_id={}, {} topics", 
            header.correlation_id, response.topics.len());
        
        let mut body_buf = BytesMut::new();
        self.encode_fetch_response(&mut body_buf, &response, header.api_version)?;
        
        let body_bytes = body_buf.freeze();
        tracing::info!("Fetch response body encoded: {} bytes", body_bytes.len());
        tracing::trace!("Response body (first 64 bytes): {:?}", 
            &body_bytes[..std::cmp::min(64, body_bytes.len())]);
        
        Ok(Self::make_response(&header, ApiKey::Fetch, body_bytes))
    }
    
    /// Handle SASL handshake request
    async fn handle_sasl_handshake(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::sasl_types::SaslHandshakeResponse;
        use crate::parser::Decoder;
        
        tracing::debug!("Handling SASL handshake request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        
        // Parse mechanism
        let mechanism = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("SASL mechanism cannot be null".into()))?;
        
        tracing::info!("SASL handshake request for mechanism: {}", mechanism);
        
        // For now, we don't actually support SASL, but we can return a proper response
        let response = SaslHandshakeResponse {
            error_code: 33, // SASL_AUTHENTICATION_FAILED
            mechanisms: vec![], // No supported mechanisms
        };
        
        let mut body_buf = BytesMut::new();
        
        let mut encoder = Encoder::new(&mut body_buf);
        // Error code
        encoder.write_i16(response.error_code);
        
        // Mechanisms array
        encoder.write_i32(response.mechanisms.len() as i32);
        for mechanism in &response.mechanisms {
            encoder.write_string(Some(mechanism));
        }
        
        Ok(Self::make_response(&header, ApiKey::SaslHandshake, body_buf.freeze()))
    }
    
    /// Handle DescribeConfigs request
    async fn handle_describe_configs(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::types::{
            DescribeConfigsResult,
            ConfigResource
        };
        use crate::parser::Decoder;
        
        tracing::debug!("Handling DescribeConfigs request v{}", header.api_version);
        tracing::debug!("Request body has {} bytes", body.len());
        
        let mut decoder = Decoder::new(body);
        
        // Parse resources array
        let resource_count = decoder.read_i32()? as usize;
        tracing::debug!("Resource count: {}", resource_count);
        let mut resources = Vec::with_capacity(resource_count);
        
        for i in 0..resource_count {
            let resource_type = decoder.read_i8()?;
            tracing::debug!("Resource {}: type = {}", i, resource_type);
            let resource_name = decoder.read_string()?
                .unwrap_or_else(|| String::new());
            tracing::debug!("Resource {}: name = '{}'", i, resource_name);
            
            // Configuration keys (v1+)
            let configuration_keys = if header.api_version >= 1 {
                let key_count = decoder.read_i32()?;
                if key_count < 0 {
                    None
                } else {
                    let mut keys = Vec::with_capacity(key_count as usize);
                    for _ in 0..key_count {
                        if let Some(key) = decoder.read_string()? {
                            keys.push(key);
                        }
                    }
                    Some(keys)
                }
            } else {
                None
            };
            
            resources.push(ConfigResource {
                resource_type,
                resource_name,
                configuration_keys,
            });
        }
        
        // Include synonyms (v1+)
        let include_synonyms = if header.api_version >= 1 {
            decoder.read_bool()?
        } else {
            false
        };
        
        // Include documentation (v3+)
        let include_documentation = if header.api_version >= 3 {
            decoder.read_bool()?
        } else {
            false
        };
        
        // Process each resource
        let mut results = Vec::new();
        
        for resource in resources {
            let configs = match resource.resource_type {
                2 => {
                    // Topic configs
                    self.get_topic_configs(
                        &resource.resource_name,
                        &resource.configuration_keys,
                        include_synonyms,
                        include_documentation,
                        header.api_version,
                    ).await?
                },
                4 => {
                    // Broker configs
                    self.get_broker_configs(
                        &resource.resource_name,
                        &resource.configuration_keys,
                        include_synonyms,
                        include_documentation,
                        header.api_version,
                    ).await?
                },
                _ => {
                    // Unsupported resource type
                    results.push(DescribeConfigsResult {
                        error_code: 87, // INVALID_CONFIG
                        error_message: Some(format!("Unsupported resource type: {}", resource.resource_type)),
                        resource_type: resource.resource_type,
                        resource_name: resource.resource_name,
                        configs: vec![],
                    });
                    continue;
                }
            };
            
            results.push(DescribeConfigsResult {
                error_code: 0,
                error_message: None,
                resource_type: resource.resource_type,
                resource_name: resource.resource_name,
                configs,
            });
        }
        
        let response = DescribeConfigsResponse {
            throttle_time_ms: 0,
            results,
        };
        
        let mut body_buf = BytesMut::new();
        
        // Encode the response body (without correlation ID)
        self.encode_describe_configs_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::DescribeConfigs, body_buf.freeze()))
    }
    
    /// Get topic configurations
    async fn get_topic_configs(
        &self,
        _topic_name: &str,
        configuration_keys: &Option<Vec<String>>,
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> Result<Vec<ConfigEntry>> {
        let mut configs = Vec::new();
        
        // Default topic configurations
        let all_configs = vec![
            ("retention.ms", "604800000", "The minimum age of a log file to be eligible for deletion", config_type::LONG),
            ("segment.ms", "604800000", "The time after which Kafka will force the log to roll", config_type::LONG),
            ("segment.bytes", "1073741824", "The segment file size for the log", config_type::LONG),
            ("min.insync.replicas", "1", "Minimum number of replicas that must acknowledge a write", config_type::INT),
            ("compression.type", "producer", "The compression type for a topic", config_type::STRING),
            ("cleanup.policy", "delete", "The retention policy to use on log segments", config_type::STRING),
            ("max.message.bytes", "1048588", "The maximum size of a message", config_type::INT),
        ];
        
        for (name, default_value, doc, config_type_val) in all_configs {
            // Check if we should include this config
            if let Some(keys) = configuration_keys {
                if !keys.contains(&name.to_string()) {
                    continue;
                }
            }
            
            let mut synonyms = Vec::new();
            if include_synonyms {
                synonyms.push(ConfigSynonym {
                    name: name.to_string(),
                    value: Some(default_value.to_string()),
                    source: config_source::DEFAULT_CONFIG,
                });
            }
            
            configs.push(ConfigEntry {
                name: name.to_string(),
                value: Some(default_value.to_string()),
                read_only: false,
                is_default: true,
                config_source: config_source::DEFAULT_CONFIG,
                is_sensitive: false,
                synonyms,
                config_type: if api_version >= 3 { Some(config_type_val) } else { None },
                documentation: if include_documentation && api_version >= 3 {
                    Some(doc.to_string())
                } else {
                    None
                },
            });
        }
        
        Ok(configs)
    }
    
    /// Get broker configurations
    async fn get_broker_configs(
        &self,
        _broker_id: &str,
        configuration_keys: &Option<Vec<String>>,
        _include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> Result<Vec<ConfigEntry>> {
        let mut configs = Vec::new();
        
        // Default broker configurations
        let all_configs = vec![
            ("log.retention.hours", "168", "The number of hours to keep a log file", config_type::INT),
            ("log.segment.bytes", "1073741824", "The maximum size of a single log file", config_type::LONG),
            ("num.network.threads", "8", "The number of threads for network requests", config_type::INT),
            ("num.io.threads", "8", "The number of threads for I/O", config_type::INT),
            ("socket.send.buffer.bytes", "102400", "The SO_SNDBUF buffer size", config_type::INT),
            ("socket.receive.buffer.bytes", "102400", "The SO_RCVBUF buffer size", config_type::INT),
        ];
        
        for (name, default_value, doc, config_type_val) in all_configs {
            // Check if we should include this config
            if let Some(keys) = configuration_keys {
                if !keys.contains(&name.to_string()) {
                    continue;
                }
            }
            
            configs.push(ConfigEntry {
                name: name.to_string(),
                value: Some(default_value.to_string()),
                read_only: true,
                is_default: true,
                config_source: config_source::STATIC_BROKER_CONFIG,
                is_sensitive: false,
                synonyms: vec![],
                config_type: if api_version >= 3 { Some(config_type_val) } else { None },
                documentation: if include_documentation && api_version >= 3 {
                    Some(doc.to_string())
                } else {
                    None
                },
            });
        }
        
        Ok(configs)
    }
    
    /// Encode DescribeConfigs response
    fn encode_describe_configs_response(
        &self,
        buf: &mut BytesMut,
        response: &DescribeConfigsResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        // Throttle time ms (v0+)
        encoder.write_i32(response.throttle_time_ms);
        
        // Results array
        encoder.write_i32(response.results.len() as i32);
        
        for result in &response.results {
            // Error code
            encoder.write_i16(result.error_code);
            
            // Error message
            encoder.write_string(result.error_message.as_deref());
            
            // Resource type
            encoder.write_i8(result.resource_type);
            
            // Resource name
            encoder.write_string(Some(&result.resource_name));
            
            // Configs array
            encoder.write_i32(result.configs.len() as i32);
            
            for config in &result.configs {
                // Config name
                encoder.write_string(Some(&config.name));
                
                // Config value
                encoder.write_string(config.value.as_deref());
                
                // Read only
                encoder.write_bool(config.read_only);
                
                // Config source (v1+)
                if version >= 1 {
                    encoder.write_i8(config.config_source);
                }
                
                // Is sensitive
                encoder.write_bool(config.is_sensitive);
                
                // Synonyms (v1+)
                if version >= 1 {
                    encoder.write_i32(config.synonyms.len() as i32);
                    
                    for synonym in &config.synonyms {
                        // Synonym name
                        encoder.write_string(Some(&synonym.name));
                        
                        // Synonym value
                        encoder.write_string(synonym.value.as_deref());
                        
                        // Synonym source
                        encoder.write_i8(synonym.source);
                    }
                } else {
                    // Default (v0)
                    encoder.write_bool(config.is_default);
                }
                
                // Config type (v3+)
                if version >= 3 {
                    if let Some(config_type) = config.config_type {
                        encoder.write_i8(config_type);
                    } else {
                        encoder.write_i8(config_type::UNKNOWN);
                    }
                }
                
                // Documentation (v3+)
                if version >= 3 {
                    encoder.write_string(config.documentation.as_deref());
                }
            }
        }
        
        Ok(())
    }
    
    /// Create an error response
    fn error_response(&self, correlation_id: i32, error_code: i16) -> Result<Response> {
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        
        // Just write error code
        encoder.write_i16(error_code);
        
        Ok(Response {
            header: ResponseHeader { correlation_id },
            body: body_buf.freeze(),
            is_flexible: false,  // Conservative default for error responses
            api_key: ApiKey::ApiVersions, // Default to ApiVersions for error responses
        })
    }
    
    /// Create a response for unimplemented APIs
    fn unimplemented_api_response(&self, correlation_id: i32, api_name: &str) -> Result<Response> {
        tracing::info!("Received request for unimplemented API: {}", api_name);
        // Return UNSUPPORTED_VERSION error
        self.error_response(correlation_id, 35)
    }
    
    /// Handle CreateTopics request
    async fn handle_create_topics(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::create_topics_types::{
            CreateTopicsRequest, CreateTopicsResponse, CreateTopicResponse,
            error_codes
        };
        use crate::parser::Decoder;
        
        tracing::debug!("Handling CreateTopics request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        
        // Parse topic count
        let topic_count = decoder.read_i32()? as usize;
        let mut topics = Vec::with_capacity(topic_count);
        
        for _ in 0..topic_count {
            // Topic name
            let name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
            
            // Number of partitions
            let num_partitions = decoder.read_i32()?;
            
            // Replication factor
            let replication_factor = decoder.read_i16()?;
            
            // Replica assignments
            let assignment_count = decoder.read_i32()?;
            let mut replica_assignments = Vec::new();
            
            if assignment_count >= 0 {
                for _ in 0..assignment_count {
                    let partition_index = decoder.read_i32()?;
                    let broker_count = decoder.read_i32()? as usize;
                    let mut broker_ids = Vec::with_capacity(broker_count);
                    
                    for _ in 0..broker_count {
                        broker_ids.push(decoder.read_i32()?);
                    }
                    
                    replica_assignments.push(crate::create_topics_types::ReplicaAssignment {
                        partition_index,
                        broker_ids,
                    });
                }
            }
            
            // Configs
            let config_count = decoder.read_i32()? as usize;
            let mut configs = std::collections::HashMap::new();
            
            for _ in 0..config_count {
                let key = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Config key cannot be null".into()))?;
                let value = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Config value cannot be null".into()))?;
                configs.insert(key, value);
            }
            
            topics.push(crate::create_topics_types::CreateTopicRequest {
                name,
                num_partitions,
                replication_factor,
                replica_assignments,
                configs,
            });
        }
        
        // Timeout
        let timeout_ms = decoder.read_i32()?;
        
        // Validate only (v1+)
        let validate_only = if header.api_version >= 1 {
            decoder.read_bool()?
        } else {
            false
        };
        
        let request = CreateTopicsRequest {
            topics,
            timeout_ms,
            validate_only,
        };
        
        // Create response
        let mut response_topics = Vec::new();
        
        for topic in request.topics {
            // Validate topic name
            let error_code = if !Self::is_valid_topic_name(&topic.name) {
                error_codes::INVALID_TOPIC_EXCEPTION
            } else if topic.num_partitions <= 0 {
                error_codes::INVALID_PARTITIONS
            } else if topic.num_partitions > 10000 {
                error_codes::INVALID_PARTITIONS
            } else if topic.replication_factor <= 0 {
                error_codes::INVALID_REPLICATION_FACTOR
            } else if let Err(e) = self.validate_topic_configs(&topic.configs, topic.replication_factor) {
                tracing::error!("Invalid topic configuration: {}", e);
                error_codes::INVALID_CONFIG
            } else {
                // Check replication factor against available brokers
                match self.validate_replication_factor(&topic).await {
                    Ok(_) => {
                        if validate_only {
                            // Just validating, don't create
                            error_codes::NONE
                        } else {
                            // Actually create the topic in metadata store
                            match self.create_topic_in_metadata(&topic).await {
                                Ok(_) => {
                                    tracing::info!("CreateTopics: Created topic '{}' with {} partitions, replication factor {}",
                                        topic.name, topic.num_partitions, topic.replication_factor);
                                    error_codes::NONE
                                }
                                Err(e) => {
                                    tracing::error!("Failed to create topic '{}': {:?}", topic.name, e);
                                    match e {
                                        Error::Internal(msg) if msg.contains("already exists") => error_codes::TOPIC_ALREADY_EXISTS,
                                        _ => error_codes::INVALID_REQUEST,
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Replication factor validation failed: {}", e);
                        error_codes::INVALID_REPLICATION_FACTOR
                    }
                }
            };
            
            let error_message = match error_code {
                error_codes::INVALID_TOPIC_EXCEPTION => Some("Invalid topic name".to_string()),
                error_codes::INVALID_PARTITIONS => Some("Invalid number of partitions".to_string()),
                error_codes::INVALID_REPLICATION_FACTOR => Some("Invalid replication factor".to_string()),
                error_codes::INVALID_CONFIG => Some("Invalid topic configuration".to_string()),
                _ => None,
            };
            
            response_topics.push(CreateTopicResponse {
                name: topic.name,
                error_code,
                error_message,
                num_partitions: if header.api_version >= 5 { topic.num_partitions } else { -1 },
                replication_factor: if header.api_version >= 5 { topic.replication_factor } else { -1 },
                configs: Vec::new(), // TODO: Return actual configs
            });
        }
        
        let response = CreateTopicsResponse {
            throttle_time_ms: 0,
            topics: response_topics,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_create_topics_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::CreateTopics, body_buf.freeze()))
    }
    
    /// Encode CreateTopics response
    fn encode_create_topics_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::create_topics_types::CreateTopicsResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        // Throttle time (v2+)
        if version >= 2 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        // Topics array
        encoder.write_i32(response.topics.len() as i32);
        
        for topic in &response.topics {
            // Topic name
            encoder.write_string(Some(&topic.name));
            
            // Error code
            encoder.write_i16(topic.error_code);
            
            // Error message (v1+)
            if version >= 1 {
                encoder.write_string(topic.error_message.as_deref());
            }
            
            // Detailed topic info (v5+)
            if version >= 5 {
                encoder.write_i32(topic.num_partitions);
                encoder.write_i16(topic.replication_factor);
                
                // Configs array
                encoder.write_i32(topic.configs.len() as i32);
                for config in &topic.configs {
                    encoder.write_string(Some(&config.name));
                    encoder.write_string(config.value.as_deref());
                    encoder.write_bool(config.read_only);
                    encoder.write_i8(config.config_source);
                    encoder.write_bool(config.is_sensitive);
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle ListOffsets request
    async fn handle_list_offsets(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::list_offsets_types::{
            ListOffsetsRequest, ListOffsetsResponse, ListOffsetsResponseTopic,
            ListOffsetsResponsePartition, LATEST_TIMESTAMP, EARLIEST_TIMESTAMP,
            error_codes
        };
        use crate::parser::Decoder;
        
        tracing::debug!("Handling ListOffsets request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        
        // Replica ID
        let replica_id = decoder.read_i32()?;
        
        // Isolation level (v2+)
        let isolation_level = if header.api_version >= 2 {
            decoder.read_i8()?
        } else {
            0 // READ_UNCOMMITTED
        };
        
        // Topics array
        let topic_count = decoder.read_i32()? as usize;
        let mut topics = Vec::with_capacity(topic_count);
        
        for _ in 0..topic_count {
            let name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
            
            // Partitions array
            let partition_count = decoder.read_i32()? as usize;
            let mut partitions = Vec::with_capacity(partition_count);
            
            for _ in 0..partition_count {
                let partition_index = decoder.read_i32()?;
                
                // Current leader epoch (v4+)
                let current_leader_epoch = if header.api_version >= 4 {
                    decoder.read_i32()?
                } else {
                    -1
                };
                
                let timestamp = decoder.read_i64()?;
                
                partitions.push(crate::list_offsets_types::ListOffsetsRequestPartition {
                    partition_index,
                    current_leader_epoch,
                    timestamp,
                });
            }
            
            topics.push(crate::list_offsets_types::ListOffsetsRequestTopic {
                name,
                partitions,
            });
        }
        
        let request = ListOffsetsRequest {
            replica_id,
            isolation_level,
            topics,
        };
        
        // Build response
        let mut response_topics = Vec::new();
        
        for topic in request.topics {
            let mut response_partitions = Vec::new();
            
            for partition in topic.partitions {
                // Get actual offsets from metadata store if available
                let (offset, timestamp_found) = match partition.timestamp {
                    LATEST_TIMESTAMP => {
                        // Get actual high watermark from metadata store
                        let high_watermark = if let Some(ref metadata_store) = self.metadata_store {
                            // Get segments for this topic-partition
                            match metadata_store.list_segments(&topic.name, Some(partition.partition_index as u32)).await {
                                Ok(segments) => {
                                    // Calculate high watermark from segments
                                    segments.iter()
                                        .map(|s| s.end_offset + 1)
                                        .max()
                                        .unwrap_or(0)
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to get segments for {}-{}: {}", 
                                        topic.name, partition.partition_index, e);
                                    0
                                }
                            }
                        } else {
                            // No metadata store, return 0
                            0
                        };
                        
                        tracing::info!("ListOffsets: Returning high watermark {} for {}-{}", 
                            high_watermark, topic.name, partition.partition_index);
                        
                        (high_watermark, std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as i64)
                    }
                    EARLIEST_TIMESTAMP => {
                        // Return the earliest offset (always 0 for now)
                        (0, std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as i64 - 86400000) // 1 day ago
                    }
                    ts if ts >= 0 => {
                        // Return offset for specific timestamp (dummy implementation)
                        (500, ts)
                    }
                    _ => {
                        // Invalid timestamp
                        (0, 0)
                    }
                };
                
                response_partitions.push(ListOffsetsResponsePartition {
                    partition_index: partition.partition_index,
                    error_code: error_codes::NONE,
                    timestamp: if header.api_version >= 1 { timestamp_found } else { -1 },
                    offset,
                    leader_epoch: if header.api_version >= 4 { 0 } else { -1 },
                });
            }
            
            response_topics.push(ListOffsetsResponseTopic {
                name: topic.name,
                partitions: response_partitions,
            });
        }
        
        let response = ListOffsetsResponse {
            throttle_time_ms: 0,
            topics: response_topics,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_list_offsets_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::ListOffsets, body_buf.freeze()))
    }
    
    /// Encode ListOffsets response
    pub fn encode_list_offsets_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::list_offsets_types::ListOffsetsResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        // Throttle time (v2+)
        if version >= 2 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        // Topics array
        encoder.write_i32(response.topics.len() as i32);
        
        for topic in &response.topics {
            // Topic name
            encoder.write_string(Some(&topic.name));
            
            // Partitions array
            encoder.write_i32(topic.partitions.len() as i32);
            
            for partition in &topic.partitions {
                // Partition index
                encoder.write_i32(partition.partition_index);

                // Error code
                encoder.write_i16(partition.error_code);

                // For v0, we need to write an array of offsets
                if version == 0 {
                    // Write offset array (for v0, we return a single offset in an array)
                    encoder.write_i32(1); // Offset count = 1
                    encoder.write_i64(partition.offset); // The single offset
                } else {
                    // Timestamp (v1+)
                    if version >= 1 {
                        encoder.write_i64(partition.timestamp);
                    }

                    // Offset (v1+)
                    encoder.write_i64(partition.offset);

                    // Leader epoch (v4+)
                    if version >= 4 {
                        encoder.write_i32(partition.leader_epoch);
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle FindCoordinator request
    async fn handle_find_coordinator(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::find_coordinator_types::{
            FindCoordinatorRequest, FindCoordinatorResponse,
            coordinator_type, error_codes
        };
        use crate::parser::Decoder;
        
        tracing::debug!("Handling FindCoordinator request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        
        // Coordinator key (group ID)
        let key = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Coordinator key cannot be null".into()))?;
        
        // Coordinator type (v1+)
        let key_type = if header.api_version >= 1 {
            decoder.read_i8()?
        } else {
            coordinator_type::GROUP // Default to group coordinator
        };
        
        let request = FindCoordinatorRequest {
            key,
            key_type,
        };
        
        tracing::info!("FindCoordinator request for key '{}', type {}", request.key, request.key_type);
        
        // For now, always return this node as the coordinator
        let response = FindCoordinatorResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
            error_message: None,
            node_id: 1, // This node
            host: "localhost".to_string(),
            port: 9092,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_find_coordinator_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::FindCoordinator, body_buf.freeze()))
    }
    
    /// Encode Produce response
    pub fn encode_produce_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::types::ProduceResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        // Check if this is a flexible/compact version (v9+)
        let flexible = version >= 9;
        
        // NOTE: throttle_time_ms position differs between versions
        // For v1-v8: throttle_time_ms goes at the END of the response
        // For v9+: handled differently with flexible versions
        // This was discovered through comparison with real Kafka - Python clients
        // expect throttle_time_ms at the end for v2 responses
        
        // Topics array
        if flexible {
            // v9+ uses compact array
            encoder.write_unsigned_varint((response.topics.len() + 1) as u32);
        } else {
            encoder.write_i32(response.topics.len() as i32);
        }
        
        for topic in &response.topics {
            // Topic name
            if flexible {
                encoder.write_compact_string(Some(&topic.name));
            } else {
                encoder.write_string(Some(&topic.name));
            }
            
            // Partitions array
            if flexible {
                encoder.write_unsigned_varint((topic.partitions.len() + 1) as u32);
            } else {
                encoder.write_i32(topic.partitions.len() as i32);
            }
            
            for partition in &topic.partitions {
                encoder.write_i32(partition.index);
                encoder.write_i16(partition.error_code);
                encoder.write_i64(partition.base_offset);
                
                if version >= 2 {
                    encoder.write_i64(partition.log_append_time);
                }
                
                if version >= 5 {
                    encoder.write_i64(partition.log_start_offset);
                }
                
                // Record errors (COMPACT_ARRAY of RecordError) - v8+
                if version >= 8 {
                    if flexible {
                        encoder.write_unsigned_varint(1); // Empty array (0 + 1 for compact encoding)
                    } else {
                        encoder.write_i32(0); // Empty array
                    }
                }
                
                // Error message (NULLABLE_COMPACT_STRING) - v8+
                if version >= 8 {
                    if flexible {
                        encoder.write_unsigned_varint(0); // Null string (compact encoding)
                    } else {
                        encoder.write_i16(-1); // Null string
                    }
                }
                
                // Write tagged fields for partition in v9+
                if flexible {
                    encoder.write_unsigned_varint(0); // No tagged fields
                }
            }
            
            // Write tagged fields for topic in v9+
            if flexible {
                encoder.write_unsigned_varint(0); // No tagged fields
            }
        }
        
        // Write tagged fields at response level in v9+
        if flexible {
            encoder.write_unsigned_varint(0); // No tagged fields
        }
        
        // Write throttle_time_ms at the END for v1-v8
        // This is critical for client compatibility (especially Python kafka-python)
        if version >= 1 && version < 9 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        // Log the encoded response for debugging
        tracing::debug!("Encoded produce response v{} total size: {} bytes", version, buf.len());
        if buf.len() < 100 {
            tracing::debug!("Encoded produce response ({} bytes): {:02x?}", buf.len(), buf.as_ref());
        } else {
            tracing::debug!("Encoded produce response (first 100 of {} bytes): {:02x?}", 
                buf.len(), &buf.as_ref()[..100]);
        }
        
        Ok(())
    }
    
    
    /// Encode Metadata response
    fn encode_metadata_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::types::MetadataResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        eprintln!("DEBUG: encode_metadata_response v{} - brokers={}, topics={}", 
                  version, response.brokers.len(), response.topics.len());
        tracing::info!("Encoding metadata response v{} with {} topics, {} brokers, throttle_time: {}", 
                      version, response.topics.len(), response.brokers.len(), response.throttle_time_ms);
        
        // Check if this is a flexible/compact version (v9+)
        let flexible = version >= 9;
        
        // Throttle time (v3+ always, regardless of flexible)
        if version >= 3 {
            eprintln!("DEBUG: Writing throttle_time_ms = {}", response.throttle_time_ms);
            encoder.write_i32(response.throttle_time_ms);
        }
        
        // Brokers array
        eprintln!("DEBUG: About to encode {} brokers, flexible={}", response.brokers.len(), flexible);
        eprintln!("DEBUG: Broker list contents:");
        for (i, broker) in response.brokers.iter().enumerate() {
            eprintln!("  Broker[{}]: id={}, host={}, port={}", i, broker.node_id, broker.host, broker.port);
        }
        
        tracing::debug!("About to encode {} brokers", response.brokers.len());
        
        // Get buffer position before writing
        let buffer_before = encoder.debug_buffer().len();
        eprintln!("DEBUG: Buffer position before broker array encoding: {}", buffer_before);
        
        if flexible {
            eprintln!("DEBUG: Using compact array encoding for {} brokers", response.brokers.len());
            encoder.write_compact_array_len(response.brokers.len());
            eprintln!("DEBUG: Expected to write compact varint for length {}", response.brokers.len() + 1);
        } else {
            eprintln!("DEBUG: Using INT32 encoding for {} brokers", response.brokers.len());
            encoder.write_i32(response.brokers.len() as i32);
        }
        
        let buffer_after = encoder.debug_buffer().len();
        eprintln!("DEBUG: Buffer position after broker array encoding: {}, wrote {} bytes", 
                 buffer_after, buffer_after - buffer_before);
        let debug_buf = encoder.debug_buffer();
        eprintln!("DEBUG: Actual bytes written for broker array length: {:02x?}", 
                 &debug_buf[buffer_before..buffer_after]);
        
        for broker in &response.brokers {
            encoder.write_i32(broker.node_id);
            
            if flexible {
                encoder.write_compact_string(Some(&broker.host));
            } else {
                encoder.write_string(Some(&broker.host));
            }
            
            encoder.write_i32(broker.port);
            
            if version >= 1 {
                if flexible {
                    encoder.write_compact_string(broker.rack.as_deref());
                } else {
                    encoder.write_string(broker.rack.as_deref());
                }
            }
            
            if flexible {
                encoder.write_tagged_fields();
            }
        }
        
        // Cluster ID comes BEFORE controller ID (librdkafka expects this order)
        if version >= 2 {
            if flexible {
                encoder.write_compact_string(response.cluster_id.as_deref());
            } else {
                encoder.write_string(response.cluster_id.as_deref());
            }
        }
        
        if version >= 1 {
            encoder.write_i32(response.controller_id);
        }
        
        // Cluster authorized operations (v8-v10 only, librdkafka doesn't read for v11+)
        if version >= 8 && version <= 10 {
            encoder.write_i32(-2147483648); // INT32_MIN indicates this field is null
        }
        
        // Topics array
        if flexible {
            encoder.write_compact_array_len(response.topics.len());
        } else {
            encoder.write_i32(response.topics.len() as i32);
        }
        
        for topic in &response.topics {
            encoder.write_i16(topic.error_code);
            
            if flexible {
                encoder.write_compact_string(Some(&topic.name));
            } else {
                encoder.write_string(Some(&topic.name));
            }
            
            // Topic ID (v10+) - UUID (16 bytes)
            if version >= 10 {
                // For now, use a null UUID (all zeros)
                encoder.write_raw_bytes(&[0u8; 16]);
            }
            
            if version >= 1 {
                encoder.write_bool(topic.is_internal);
            }
            
            // Partitions array
            if flexible {
                encoder.write_compact_array_len(topic.partitions.len());
            } else {
                encoder.write_i32(topic.partitions.len() as i32);
            }
            
            for partition in &topic.partitions {
                encoder.write_i16(partition.error_code);
                encoder.write_i32(partition.partition_index);
                encoder.write_i32(partition.leader_id);
                
                if version >= 7 {
                    encoder.write_i32(partition.leader_epoch);
                }
                
                // Replica nodes
                if flexible {
                    encoder.write_compact_array_len(partition.replica_nodes.len());
                } else {
                    encoder.write_i32(partition.replica_nodes.len() as i32);
                }
                for replica in &partition.replica_nodes {
                    encoder.write_i32(*replica);
                }
                
                // ISR nodes
                if flexible {
                    encoder.write_compact_array_len(partition.isr_nodes.len());
                } else {
                    encoder.write_i32(partition.isr_nodes.len() as i32);
                }
                for isr in &partition.isr_nodes {
                    encoder.write_i32(*isr);
                }
                
                if version >= 5 {
                    // Offline replicas
                    if flexible {
                        encoder.write_compact_array_len(partition.offline_replicas.len());
                    } else {
                        encoder.write_i32(partition.offline_replicas.len() as i32);
                    }
                    for offline in &partition.offline_replicas {
                        encoder.write_i32(*offline);
                    }
                }
                
                if flexible {
                    encoder.write_tagged_fields();
                }
            }
            
            // Topic authorized operations (v8+)
            if version >= 8 {
                encoder.write_i32(-2147483648); // INT32_MIN means "null"
            }
            
            if flexible {
                encoder.write_tagged_fields();
            }
        }
        
        // Remove duplicate cluster_authorized_operations - already written above
        
        if flexible {
            eprintln!("DEBUG: Writing final tagged fields for Metadata v{}", version);
            encoder.write_tagged_fields();
        }
        
        eprintln!("DEBUG: Metadata response encoded, buffer size = {} bytes", encoder.debug_buffer().len());
        Ok(())
    }
    
    /// Handle JoinGroup request
    async fn handle_join_group(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::join_group_types::{
            JoinGroupRequest, JoinGroupResponse, JoinGroupRequestProtocol,
            JoinGroupResponseMember, error_codes
        };
        use crate::parser::Decoder;
        
        tracing::debug!("Handling JoinGroup request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        
        // Parse request
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        let session_timeout_ms = decoder.read_i32()?;
        
        let rebalance_timeout_ms = if header.api_version >= 1 {
            decoder.read_i32()?
        } else {
            session_timeout_ms // Default to session timeout
        };
        
        let member_id = decoder.read_string()?.unwrap_or_default();
        
        let group_instance_id = if header.api_version >= 5 {
            decoder.read_string()?
        } else {
            None
        };
        
        let protocol_type = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Protocol type cannot be null".into()))?;
        
        // Read protocols array
        let protocol_count = decoder.read_i32()? as usize;
        let mut protocols = Vec::with_capacity(protocol_count);
        
        for _ in 0..protocol_count {
            let name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Protocol name cannot be null".into()))?;
            let metadata = decoder.read_bytes()?
                .ok_or_else(|| Error::Protocol("Protocol metadata cannot be null".into()))?;
            
            protocols.push(JoinGroupRequestProtocol { name, metadata });
        }
        
        let request = JoinGroupRequest {
            group_id: group_id.clone(),
            session_timeout_ms,
            rebalance_timeout_ms,
            member_id: member_id.clone(),
            group_instance_id,
            protocol_type,
            protocols,
        };
        
        tracing::info!(
            "JoinGroup request for group '{}', member '{}', protocol '{}'",
            request.group_id, request.member_id, request.protocol_type
        );
        
        // Update consumer group state
        let mut group_state = self.consumer_groups.lock().await;
        
        let is_new_member = request.member_id.is_empty();
        let assigned_member_id = if is_new_member {
            format!("{}-{}", request.group_id, uuid::Uuid::new_v4())
        } else {
            request.member_id.clone()
        };
        
        // Get or create the consumer group
        let group = group_state.groups.entry(request.group_id.clone())
            .or_insert_with(|| ConsumerGroup {
                group_id: request.group_id.clone(),
                leader_id: None,
                generation_id: 0,
                protocol_type: Some(request.protocol_type.clone()),
                protocol: None,
                members: Vec::new(),
                state: "Empty".to_string(),
                offsets: HashMap::new(),
            });
        
        // Update group state
        group.state = "PreparingRebalance".to_string();
        group.generation_id += 1;
        group.protocol_type = Some(request.protocol_type.clone());
        
        // Add/update member
        let member_exists = group.members.iter_mut()
            .find(|m| m.member_id == assigned_member_id)
            .map(|m| {
                m.client_id = "kafka-python".to_string(); // TODO: Parse from metadata
                m.client_host = "/127.0.0.1".to_string();
                m.group_instance_id = request.group_instance_id.clone();
                m.metadata = request.protocols.first().map(|p| p.metadata.to_vec());
                true
            })
            .unwrap_or(false);
        
        if !member_exists {
            group.members.push(GroupMember {
                member_id: assigned_member_id.clone(),
                group_instance_id: request.group_instance_id.clone(),
                client_id: "kafka-python".to_string(),
                client_host: "/127.0.0.1".to_string(),
                metadata: request.protocols.first().map(|p| p.metadata.to_vec()),
                assignment: None,
            });
        }
        
        // Make first member the leader
        if group.leader_id.is_none() {
            group.leader_id = Some(assigned_member_id.clone());
        }
        
        // Select first protocol if available
        let selected_protocol = request.protocols.first().map(|p| p.name.clone());
        group.protocol = selected_protocol.clone();
        
        // Update state to Stable once we have a leader
        group.state = "Stable".to_string();
        
        let is_leader = group.leader_id.as_ref() == Some(&assigned_member_id);
        let generation_id = group.generation_id;
        
        // Build member list for leader
        let members = if is_leader {
            group.members.iter().map(|m| {
                JoinGroupResponseMember {
                    member_id: m.member_id.clone(),
                    group_instance_id: m.group_instance_id.clone(),
                    metadata: m.metadata.clone().map(Bytes::from).unwrap_or_else(|| Bytes::new()),
                }
            }).collect()
        } else {
            vec![]
        };
        
        let response = JoinGroupResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
            generation_id,
            protocol_type: if header.api_version >= 7 {
                Some(request.protocol_type)
            } else {
                None
            },
            protocol_name: selected_protocol,
            leader: group.leader_id.clone().unwrap_or_default(),
            member_id: assigned_member_id,
            members,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_join_group_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::JoinGroup, body_buf.freeze()))
    }
    
    /// Handle SyncGroup request
    async fn handle_sync_group(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::sync_group_types::{
            SyncGroupRequest, SyncGroupResponse, SyncGroupRequestAssignment,
            error_codes
        };
        use crate::parser::Decoder;
        
        tracing::debug!("Handling SyncGroup request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        
        // Parse request
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        let generation_id = decoder.read_i32()?;
        let member_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;
        
        let group_instance_id = if header.api_version >= 3 {
            decoder.read_string()?
        } else {
            None
        };
        
        let (protocol_type, protocol_name) = if header.api_version >= 5 {
            (decoder.read_string()?, decoder.read_string()?)
        } else {
            (None, None)
        };
        
        // Read assignments array
        let assignment_count = decoder.read_i32()? as usize;
        let mut assignments = Vec::with_capacity(assignment_count);
        
        for _ in 0..assignment_count {
            let member_id = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Member ID in assignment cannot be null".into()))?;
            let assignment = decoder.read_bytes()?
                .ok_or_else(|| Error::Protocol("Assignment cannot be null".into()))?;
            
            assignments.push(SyncGroupRequestAssignment { member_id, assignment });
        }
        
        let request = SyncGroupRequest {
            group_id: group_id.clone(),
            generation_id,
            member_id: member_id.clone(),
            group_instance_id,
            protocol_type: protocol_type.clone(),
            protocol_name: protocol_name.clone(),
            assignments,
        };
        
        tracing::info!(
            "SyncGroup request for group '{}', generation {}, member '{}'",
            request.group_id, request.generation_id, request.member_id
        );
        
        // Update consumer group state with assignments
        let mut group_state = self.consumer_groups.lock().await;
        let member_assignment = if let Some(group) = group_state.groups.get_mut(&request.group_id) {
            // If leader sent assignments, use them
            if !request.assignments.is_empty() {
                for assignment in &request.assignments {
                    if let Some(member) = group.members.iter_mut()
                        .find(|m| m.member_id == assignment.member_id) {
                        member.assignment = Some(assignment.assignment.to_vec());
                    }
                }

                // Return the assignment for the requesting member
                request.assignments.iter()
                    .find(|a| a.member_id == request.member_id)
                    .map(|a| a.assignment.clone())
                    .unwrap_or_else(|| self.create_default_assignment())
            } else {
                // Create default assignment for simple round-robin to "test-topic" partition 0
                self.create_default_assignment()
            }
        } else {
            self.create_default_assignment()
        };
        
        let response = SyncGroupResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
            protocol_type: if header.api_version >= 5 { protocol_type } else { None },
            protocol_name: if header.api_version >= 5 { protocol_name } else { None },
            assignment: member_assignment,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_sync_group_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::SyncGroup, body_buf.freeze()))
    }

    /// Create a default consumer assignment for "test-topic" partition 0
    fn create_default_assignment(&self) -> Bytes {
        use bytes::BufMut;
        let mut buf = BytesMut::new();

        // Consumer Protocol Assignment format:
        // Version: i16
        // TopicPartitions: [Topic]
        //   Topic:
        //     Name: string
        //     Partitions: [i32]
        // UserData: bytes

        // Version (0)
        buf.put_i16(0);

        // Topics array length (1 topic)
        buf.put_i32(1);

        // Topic name: "test-topic"
        let topic_name = "test-topic";
        buf.put_i16(topic_name.len() as i16);
        buf.put_slice(topic_name.as_bytes());

        // Partitions array length (1 partition)
        buf.put_i32(1);

        // Partition ID (0)
        buf.put_i32(0);

        // UserData length (0)
        buf.put_i32(0);

        buf.freeze()
    }

    /// Handle DescribeGroups request
    async fn handle_describe_groups(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::consumer_group_types::{
            DescribeGroupsRequest, DescribeGroupsResponse, DescribedGroup, GroupMember,
            error_codes
        };
        
        tracing::info!("Handling DescribeGroups request - version: {}", header.api_version);
        
        // Parse request
        let request = DescribeGroupsRequest::parse(body, header.api_version)?;
        tracing::info!("DescribeGroups for groups: {:?}", request.group_ids);
        tracing::info!("Request include_authorized_operations: {}", request.include_authorized_operations);
        
        // Build response for each group
        let mut groups = Vec::new();
        
        for group_id in request.group_ids {
            // Check if this is a tracked consumer group
            let group_state = self.consumer_groups.lock().await;
            
            if let Some(group) = group_state.groups.get(&group_id) {
                // Build member list
                let mut members = Vec::new();
                for member in &group.members {
                    members.push(GroupMember {
                        member_id: member.member_id.clone(),
                        group_instance_id: member.group_instance_id.clone(),
                        client_id: member.client_id.clone(),
                        client_host: member.client_host.clone(),
                        member_metadata: member.metadata.clone().unwrap_or_default(),
                        member_assignment: member.assignment.clone().unwrap_or_default(),
                    });
                }
                
                groups.push(DescribedGroup {
                    error_code: error_codes::NONE,
                    group_id: group_id.clone(),
                    group_state: group.state.clone(),
                    protocol_type: group.protocol_type.clone().unwrap_or_else(|| "consumer".to_string()),
                    protocol_data: group.protocol.clone().unwrap_or_default(),
                    members,
                    authorized_operations: -2147483648, // No auth
                });
            } else {
                // Group not found
                groups.push(DescribedGroup {
                    error_code: error_codes::GROUP_ID_NOT_FOUND,
                    group_id: group_id.clone(),
                    group_state: String::new(),
                    protocol_type: String::new(),
                    protocol_data: String::new(),
                    members: Vec::new(),
                    authorized_operations: -2147483648,
                });
            }
        }
        
        let response = DescribeGroupsResponse {
            throttle_time_ms: 0,
            groups,
        };
        
        tracing::info!("DescribeGroups response has {} groups", response.groups.len());
        for group in &response.groups {
            tracing::info!("  Group '{}': error_code={}, state='{}', members={}", 
                group.group_id, group.error_code, group.group_state, group.members.len());
        }
        
        let body = response.encode(header.api_version);
        tracing::info!("Encoded DescribeGroups response: {} bytes", body.len());
        Ok(Self::make_response(&header, ApiKey::DescribeGroups, body))
    }
    
    /// Handle Heartbeat request
    async fn handle_heartbeat(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::heartbeat_types::{
            HeartbeatRequest, HeartbeatResponse, error_codes
        };
        use crate::parser::Decoder;
        
        tracing::debug!("Handling Heartbeat request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        
        // Parse request
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
        let generation_id = decoder.read_i32()?;
        let member_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;
        
        let group_instance_id = if header.api_version >= 3 {
            decoder.read_string()?
        } else {
            None
        };
        
        let request = HeartbeatRequest {
            group_id: group_id.clone(),
            generation_id,
            member_id: member_id.clone(),
            group_instance_id,
        };
        
        tracing::debug!(
            "Heartbeat from member '{}' in group '{}', generation {}",
            request.member_id, request.group_id, request.generation_id
        );
        
        // For now, always return success
        // In a real implementation, this would check member validity and group state
        let response = HeartbeatResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_heartbeat_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::Heartbeat, body_buf.freeze()))
    }
    
    /// Handle ListGroups request
    async fn handle_list_groups(
        &self,
        header: RequestHeader,
        _body: &mut Bytes,
    ) -> Result<Response> {
        use crate::list_groups_types::{
            ListGroupsResponse, error_codes
        };
        
        tracing::debug!("Handling ListGroups request v{}", header.api_version);
        
        // For now, return an empty list of groups
        // In a real implementation, this would list all consumer groups
        let response = ListGroupsResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
            groups: vec![],
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_list_groups_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::ListGroups, body_buf.freeze()))
    }
    
    /// Handle OffsetFetch request (ListConsumerGroupOffsets)
    async fn handle_offset_fetch(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::consumer_group_types::{
            ListConsumerGroupOffsetsRequest, ListConsumerGroupOffsetsResponse,
            OffsetFetchResponseTopic, OffsetFetchResponsePartition,
        };
        
        tracing::info!("Handling OffsetFetch request - version: {}", header.api_version);
        
        // Parse request
        let request = ListConsumerGroupOffsetsRequest::parse(body, header.api_version)?;
        tracing::info!("OffsetFetch for group: {}", request.group_id);
        
        // Get group offsets from storage
        let group_state = self.consumer_groups.lock().await;
        let mut topics = Vec::new();
        
        if let Some(group) = group_state.groups.get(&request.group_id) {
            // Return offsets for requested topics or all topics
            for (topic_name, topic_offsets) in &group.offsets {
                // Check if this topic was requested
                let include_topic = if let Some(ref requested_topics) = request.topics {
                    requested_topics.iter().any(|t| t.name == *topic_name)
                } else {
                    true // Include all topics if none specified
                };
                
                if include_topic {
                    let mut partitions = Vec::new();
                    
                    for (partition_id, offset_info) in topic_offsets {
                        partitions.push(OffsetFetchResponsePartition {
                            partition_index: *partition_id,
                            committed_offset: offset_info.offset,
                            committed_leader_epoch: -1,
                            metadata: offset_info.metadata.clone(),
                            error_code: 0,
                        });
                    }
                    
                    topics.push(OffsetFetchResponseTopic {
                        name: topic_name.clone(),
                        partitions,
                    });
                }
            }
        }
        
        let response = ListConsumerGroupOffsetsResponse {
            throttle_time_ms: 0,
            error_code: 0,
            topics,
        };
        
        let body = response.encode(header.api_version);
        Ok(Self::make_response(&header, ApiKey::OffsetFetch, body))
    }
    
    /// Handle OffsetCommit request
    async fn handle_offset_commit(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::types::{OffsetCommitResponse, OffsetCommitResponseTopic, OffsetCommitResponsePartition};
        
        tracing::info!("Handling OffsetCommit request - version: {}", header.api_version);
        
        // Parse request
        let request = self.parse_offset_commit_request(&header, body)?;
        tracing::info!("OffsetCommit for group: {}", request.group_id);
        
        // Store offsets
        let mut group_state = self.consumer_groups.lock().await;
        let group = group_state.groups.entry(request.group_id.clone())
            .or_insert_with(|| ConsumerGroup {
                group_id: request.group_id.clone(),
                leader_id: None,
                generation_id: 0,
                protocol_type: Some("consumer".to_string()),
                protocol: None,
                members: Vec::new(),
                state: "Empty".to_string(),
                offsets: std::collections::HashMap::new(),
            });
        
        // Update offsets
        let mut response_topics = Vec::new();
        
        for topic in &request.topics {
            let topic_offsets = group.offsets.entry(topic.name.clone())
                .or_insert_with(std::collections::HashMap::new);
            
            let mut response_partitions = Vec::new();
            
            for partition in &topic.partitions {
                topic_offsets.insert(partition.partition_index, OffsetInfo {
                    offset: partition.committed_offset,
                    metadata: partition.committed_metadata.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64,
                });
                
                response_partitions.push(OffsetCommitResponsePartition {
                    partition_index: partition.partition_index,
                    error_code: 0,
                });
            }
            
            response_topics.push(OffsetCommitResponseTopic {
                name: topic.name.clone(),
                partitions: response_partitions,
            });
        }
        
        let response = OffsetCommitResponse {
            header: ResponseHeader { correlation_id: header.correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_offset_commit_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Self::make_response(&header, ApiKey::OffsetCommit, body_buf.freeze()))
    }
    
    /// Encode ListGroups response
    fn encode_list_groups_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::list_groups_types::ListGroupsResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        // Throttle time (v1+)
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        // Error code
        encoder.write_i16(response.error_code);
        
        // Groups array
        encoder.write_i32(response.groups.len() as i32);
        for group in &response.groups {
            encoder.write_string(Some(&group.group_id));
            encoder.write_string(Some(&group.protocol_type));
        }
        
        Ok(())
    }
    
    /// Create a topic in the metadata store
    async fn create_topic_in_metadata(&self, topic: &crate::create_topics_types::CreateTopicRequest) -> Result<()> {
        if let Some(metadata_store) = &self.metadata_store {
            use chronik_common::metadata::traits::{TopicConfig, PartitionAssignment};
            
            // Parse configurations - validation already done in handle_create_topics
            let retention_ms = topic.configs.get("retention.ms")
                .and_then(|v| v.parse().ok());
            let segment_bytes = topic.configs.get("segment.bytes")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024 * 1024 * 1024); // 1GB default
            
            let config = TopicConfig {
                partition_count: topic.num_partitions as u32,
                replication_factor: topic.replication_factor as u32,
                retention_ms,
                segment_bytes,
                config: topic.configs.clone(),
            };
            
            // Get available brokers for partition assignment
            let brokers = metadata_store.list_brokers().await
                .map_err(|e| Error::Internal(format!("Failed to list brokers: {:?}", e)))?;
            
            let online_brokers: Vec<_> = brokers.iter()
                .filter(|b| b.status == chronik_common::metadata::BrokerStatus::Online)
                .collect();
            
            // Handle case where no brokers are registered
            if online_brokers.is_empty() {
                tracing::warn!("No online brokers found, using default broker ID {}", self.broker_id);
                // Register this broker if not already registered
                if metadata_store.get_broker(self.broker_id).await
                    .map_err(|e| Error::Internal(format!("Failed to get broker: {:?}", e)))?
                    .is_none() {
                    let broker_metadata = chronik_common::metadata::BrokerMetadata {
                        broker_id: self.broker_id,
                        host: "localhost".to_string(), // TODO: Get from config
                        port: 9092, // TODO: Get from config
                        rack: None,
                        status: chronik_common::metadata::BrokerStatus::Online,
                        created_at: chronik_common::Utc::now(),
                        updated_at: chronik_common::Utc::now(),
                    };
                    metadata_store.register_broker(broker_metadata).await
                        .map_err(|e| Error::Internal(format!("Failed to register broker: {:?}", e)))?;
                }
            }
            
            // Create partition assignments using round-robin assignment
            let mut assignments = Vec::new();
            let broker_count = if online_brokers.is_empty() { 1 } else { online_brokers.len() };
            
            for partition in 0..topic.num_partitions {
                let broker_index = (partition as usize) % broker_count;
                let broker_id = if online_brokers.is_empty() {
                    self.broker_id
                } else {
                    online_brokers[broker_index].broker_id
                };
                
                assignments.push(PartitionAssignment {
                    topic: topic.name.clone(),
                    partition: partition as u32,
                    broker_id,
                    is_leader: true, // For now, all replicas are leaders
                });
            }
            
            // Prepare offset initialization data
            let mut offsets = Vec::new();
            for partition in 0..topic.num_partitions {
                offsets.push((
                    partition as u32,
                    0i64, // high_watermark
                    0i64, // log_start_offset
                ));
            }
            
            // Create topic atomically with all assignments and offsets
            let topic_metadata = metadata_store.create_topic_with_assignments(
                &topic.name,
                config,
                assignments,
                offsets,
            ).await
                .map_err(|e| Error::Internal(format!("Failed to create topic: {:?}", e)))?;
            
            tracing::info!("Successfully created topic '{}' with ID {} and {} partitions atomically across {} brokers", 
                topic.name, topic_metadata.id, topic.num_partitions, broker_count);
            
            Ok(())
        } else {
            // No metadata store, just log
            tracing::warn!("No metadata store configured, topic creation not persisted");
            Ok(())
        }
    }
    
    /// Validate topic name according to Kafka rules
    fn is_valid_topic_name(name: &str) -> bool {
        // Topic name must:
        // - Not be empty
        // - Not be "." or ".."
        // - Not exceed 249 characters
        // - Contain only alphanumeric, '.', '_', or '-'
        if name.is_empty() || name == "." || name == ".." || name.len() > 249 {
            return false;
        }
        
        name.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '-')
    }
    
    /// Validate topic configurations
    async fn validate_replication_factor(&self, topic: &crate::create_topics_types::CreateTopicRequest) -> Result<()> {
        if let Some(metadata_store) = &self.metadata_store {
            let brokers = metadata_store.list_brokers().await
                .map_err(|e| Error::Internal(format!("Failed to list brokers: {}", e)))?;
            
            let online_brokers: Vec<_> = brokers.iter()
                .filter(|b| b.status == chronik_common::metadata::BrokerStatus::Online)
                .collect();
            
            if online_brokers.is_empty() {
                return Err(Error::Protocol("No online brokers available".into()));
            }
            
            if topic.replication_factor as usize > online_brokers.len() {
                return Err(Error::Protocol(format!(
                    "Replication factor {} exceeds available brokers {}", 
                    topic.replication_factor, 
                    online_brokers.len()
                )));
            }
        }
        Ok(())
    }

    fn validate_topic_configs(&self, configs: &HashMap<String, String>, replication_factor: i16) -> Result<()> {
        for (key, value) in configs {
            match key.as_str() {
                "retention.ms" => {
                    if let Ok(ms) = value.parse::<i64>() {
                        if ms < -1 {
                            return Err(Error::Protocol("retention.ms must be >= -1".into()));
                        }
                    } else {
                        return Err(Error::Protocol(format!("Invalid retention.ms value: {}", value)));
                    }
                }
                "segment.bytes" => {
                    if let Ok(bytes) = value.parse::<i64>() {
                        if bytes < 14 {
                            return Err(Error::Protocol("segment.bytes must be >= 14".into()));
                        }
                    } else {
                        return Err(Error::Protocol(format!("Invalid segment.bytes value: {}", value)));
                    }
                }
                "compression.type" => {
                    if !["none", "gzip", "snappy", "lz4", "zstd"].contains(&value.as_str()) {
                        return Err(Error::Protocol(format!("Invalid compression.type: {}", value)));
                    }
                }
                "cleanup.policy" => {
                    if !["delete", "compact", "delete,compact", "compact,delete"].contains(&value.as_str()) {
                        return Err(Error::Protocol(format!("Invalid cleanup.policy: {}", value)));
                    }
                }
                "min.insync.replicas" => {
                    if let Ok(replicas) = value.parse::<i32>() {
                        if replicas < 1 || replicas > replication_factor as i32 {
                            return Err(Error::Protocol(format!(
                                "min.insync.replicas must be between 1 and replication factor ({})", 
                                replication_factor
                            )));
                        }
                    } else {
                        return Err(Error::Protocol(format!("Invalid min.insync.replicas value: {}", value)));
                    }
                }
                _ => {
                    // Other configs are allowed without validation
                }
            }
        }
        Ok(())
    }
    
    /// Get topics from metadata store
    async fn get_topics_from_metadata(&self, requested_topics: &Option<Vec<String>>) -> Result<Vec<crate::types::MetadataTopic>> {
        use crate::types::{MetadataTopic, MetadataPartition};
        use crate::error_codes;
        
        if let Some(metadata_store) = &self.metadata_store {
            let all_topics = metadata_store.list_topics().await
                .map_err(|e| Error::Internal(format!("Failed to list topics: {:?}", e)))?;
            
            // Prepare the topics we'll process
            let mut topics_to_process = Vec::new();
            let mut error_topics = Vec::new();
            
            if let Some(requested) = requested_topics {
                if requested.is_empty() {
                    // Empty list means return all topics (Kafka protocol behavior)
                    tracing::debug!("Empty topics list in request - returning all topics");
                    topics_to_process = all_topics;
                } else {
                    // When specific topics are requested, include them even if they don't exist
                    for requested_topic in requested {
                        if let Some(topic_meta) = all_topics.iter().find(|t| &t.name == requested_topic) {
                            // Topic exists - add it for normal processing
                            topics_to_process.push(topic_meta.clone());
                        } else {
                            // Topic doesn't exist - add to error list
                            tracing::debug!("Topic {} not found, will return with UNKNOWN_TOPIC_OR_PARTITION", requested_topic);
                            error_topics.push(MetadataTopic {
                                error_code: error_codes::UNKNOWN_TOPIC_OR_PARTITION,
                                name: requested_topic.clone(),
                                is_internal: false,
                                partitions: vec![],
                            });
                        }
                    }
                }
            } else {
                // None means return all topics
                topics_to_process = all_topics;
            };
            
            // Convert to Kafka metadata format
            let mut result = Vec::new();
            
            // First add all existing topics with their proper metadata
            for topic_meta in topics_to_process {
                let mut partitions = Vec::new();
                
                // Get actual partition assignments from metadata store
                let assignments = match metadata_store.get_partition_assignments(&topic_meta.name).await {
                    Ok(assignments) => assignments,
                    Err(e) => {
                        tracing::error!("Failed to get partition assignments for topic {}: {:?}", topic_meta.name, e);
                        vec![]
                    }
                };
                
                // Create partition metadata from assignments
                if assignments.is_empty() {
                    // Fallback: create default partition metadata if no assignments found
                    for partition_id in 0..topic_meta.config.partition_count {
                        partitions.push(MetadataPartition {
                            error_code: 0,
                            partition_index: partition_id as i32,
                            leader_id: self.broker_id,
                            leader_epoch: 0,
                            replica_nodes: vec![self.broker_id],
                            isr_nodes: vec![self.broker_id],
                            offline_replicas: vec![],
                        });
                    }
                } else {
                    // Use actual partition assignments
                    let mut sorted_assignments = assignments;
                    sorted_assignments.sort_by_key(|a| a.partition);
                    
                    for assignment in sorted_assignments {
                        partitions.push(MetadataPartition {
                            error_code: 0,
                            partition_index: assignment.partition as i32,
                            leader_id: if assignment.is_leader { assignment.broker_id } else { self.broker_id },
                            leader_epoch: 0,
                            replica_nodes: vec![assignment.broker_id],
                            isr_nodes: vec![assignment.broker_id], // For now, all replicas are in sync
                            offline_replicas: vec![],
                        });
                    }
                }
                
                result.push(MetadataTopic {
                    error_code: 0,
                    name: topic_meta.name,
                    is_internal: false,
                    partitions,
                });
            }
            
            // Then add any error topics (topics that don't exist)
            result.extend(error_topics);
            
            Ok(result)
        } else {
            Ok(vec![])
        }
    }
    
    /// Auto-create topics that don't exist
    async fn auto_create_topics(&self, requested_topics: &[String]) -> Result<()> {
        if let Some(metadata_store) = &self.metadata_store {
            // Get list of existing topics
            let existing_topics = metadata_store.list_topics().await
                .map_err(|e| Error::Internal(format!("Failed to list topics: {:?}", e)))?;
            
            let existing_topic_names: HashSet<String> = 
                existing_topics.into_iter().map(|t| t.name).collect();
            
            // Find topics that don't exist
            let topics_to_create: Vec<&String> = requested_topics
                .iter()
                .filter(|t| !existing_topic_names.contains(*t))
                .collect();
            
            if !topics_to_create.is_empty() {
                tracing::info!("Auto-creating {} topics: {:?}", topics_to_create.len(), topics_to_create);
                
                // Get default configuration values (should be configurable)
                let default_num_partitions = 3;
                let default_replication_factor = 1;
                let default_retention_ms = 604800000; // 7 days
                let default_segment_bytes = 1073741824; // 1GB
                
                for topic_name in topics_to_create {
                    // Create topic configuration
                    let mut config_map = HashMap::new();
                    config_map.insert("compression.type".to_string(), "none".to_string());
                    config_map.insert("cleanup.policy".to_string(), "delete".to_string());
                    config_map.insert("min.insync.replicas".to_string(), "1".to_string());
                    
                    let config = chronik_common::metadata::TopicConfig {
                        partition_count: default_num_partitions,
                        replication_factor: default_replication_factor,
                        retention_ms: Some(default_retention_ms),
                        segment_bytes: default_segment_bytes,
                        config: config_map,
                    };
                    
                    // Get broker ID for assignment
                    let brokers = metadata_store.list_brokers().await
                        .map_err(|e| Error::Internal(format!("Failed to list brokers: {:?}", e)))?;
                    
                    let online_brokers: Vec<_> = brokers.iter()
                        .filter(|b| b.status == chronik_common::metadata::BrokerStatus::Online)
                        .collect();
                    
                    if online_brokers.is_empty() {
                        tracing::warn!("No online brokers available for auto-topic creation");
                        continue;
                    }
                    
                    let broker_id = online_brokers[0].broker_id;
                    
                    // Create assignments for all partitions
                    let mut assignments = Vec::new();
                    for partition in 0..default_num_partitions {
                        assignments.push(chronik_common::metadata::PartitionAssignment {
                            topic: topic_name.clone(),
                            partition,
                            broker_id,
                            is_leader: true,
                        });
                    }
                    
                    // Create initial offsets
                    let mut offsets = Vec::new();
                    for partition in 0..default_num_partitions {
                        offsets.push((partition, 0i64, 0i64)); // partition, high_watermark, log_start_offset
                    }
                    
                    // Create topic with assignments
                    match metadata_store.create_topic_with_assignments(
                        topic_name,
                        config,
                        assignments,
                        offsets,
                    ).await {
                        Ok(topic_meta) => {
                            tracing::info!("Auto-created topic '{}' with ID {} and {} partitions",
                                topic_name, topic_meta.id, default_num_partitions);
                        }
                        Err(e) => {
                            tracing::error!("Failed to auto-create topic '{}': {:?}", topic_name, e);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl Default for ProtocolHandler {
    fn default() -> Self {
        Self::new()
    }
}