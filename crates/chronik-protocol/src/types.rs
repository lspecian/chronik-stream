//! Kafka protocol types.

use crate::parser::{RequestHeader, ResponseHeader};
use crate::sasl_types::{SaslHandshakeRequest, SaslHandshakeResponse, SaslAuthenticateRequest, SaslAuthenticateResponse};

/// API version type
pub type ApiVersion = i16;

/// Request wrapper
#[derive(Debug, Clone)]
pub struct Request {
    pub header: RequestHeader,
    pub body: RequestBody,
}

/// Request body variants
#[derive(Debug, Clone)]
pub enum RequestBody {
    Produce(ProduceRequest),
    Fetch(FetchRequest),
    Metadata(MetadataRequest),
    JoinGroup(JoinGroupRequest),
    SyncGroup(SyncGroupRequest),
    Heartbeat(HeartbeatRequest),
    LeaveGroup(LeaveGroupRequest),
    OffsetCommit(OffsetCommitRequest),
    OffsetFetch(OffsetFetchRequest),
    ApiVersions(ApiVersionsRequest),
    SaslHandshake(SaslHandshakeRequest),
    SaslAuthenticate(SaslAuthenticateRequest),
    DescribeConfigs(DescribeConfigsRequest),
}

/// Response variants
#[derive(Debug, Clone)]
pub enum Response {
    Produce(ProduceResponse),
    Fetch(FetchResponse),
    Metadata(MetadataResponse),
    JoinGroup(JoinGroupResponse),
    SyncGroup(SyncGroupResponse),
    Heartbeat(HeartbeatResponse),
    LeaveGroup(LeaveGroupResponse),
    OffsetCommit(OffsetCommitResponse),
    OffsetFetch(OffsetFetchResponse),
    ApiVersions(ApiVersionsResponse),
    SaslHandshake(SaslHandshakeResponse),
    SaslAuthenticate(SaslAuthenticateResponse),
    DescribeConfigs(DescribeConfigsResponse),
}

/// Produce request
#[derive(Debug, Clone)]
pub struct ProduceRequest {
    pub transactional_id: Option<String>,
    pub acks: i16,
    pub timeout_ms: i32,
    pub topics: Vec<ProduceRequestTopic>,
}

/// Produce request topic
#[derive(Debug, Clone)]
pub struct ProduceRequestTopic {
    pub name: String,
    pub partitions: Vec<ProduceRequestPartition>,
}

/// Produce request partition
#[derive(Debug, Clone)]
pub struct ProduceRequestPartition {
    pub index: i32,
    pub records: Vec<u8>, // Encoded records
}

/// Produce response
#[derive(Debug, Clone)]
pub struct ProduceResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub topics: Vec<ProduceResponseTopic>,
}

/// Produce response topic
#[derive(Debug, Clone)]
pub struct ProduceResponseTopic {
    pub name: String,
    pub partitions: Vec<ProduceResponsePartition>,
}

/// Produce response partition
#[derive(Debug, Clone)]
pub struct ProduceResponsePartition {
    pub index: i32,
    pub error_code: i16,
    pub base_offset: i64,
    pub log_append_time: i64,
    pub log_start_offset: i64,
}

/// Fetch request
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub replica_id: i32,
    pub max_wait_ms: i32,
    pub min_bytes: i32,
    pub max_bytes: i32,
    pub isolation_level: i8,
    pub session_id: i32,
    pub session_epoch: i32,
    pub topics: Vec<FetchRequestTopic>,
}

/// Fetch request topic
#[derive(Debug, Clone)]
pub struct FetchRequestTopic {
    pub name: String,
    pub partitions: Vec<FetchRequestPartition>,
}

/// Fetch request partition
#[derive(Debug, Clone)]
pub struct FetchRequestPartition {
    pub partition: i32,
    pub current_leader_epoch: i32,
    pub fetch_offset: i64,
    pub log_start_offset: i64,
    pub partition_max_bytes: i32,
}

/// Fetch response
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub error_code: i16,      // v7+
    pub session_id: i32,       // v7+
    pub topics: Vec<FetchResponseTopic>,
}

/// Fetch response topic
#[derive(Debug, Clone)]
pub struct FetchResponseTopic {
    pub name: String,
    pub partitions: Vec<FetchResponsePartition>,
}

/// Fetch response partition
#[derive(Debug, Clone)]
pub struct FetchResponsePartition {
    pub partition: i32,
    pub error_code: i16,
    pub high_watermark: i64,
    pub last_stable_offset: i64,
    pub log_start_offset: i64,
    pub aborted: Option<Vec<AbortedTransaction>>,
    pub preferred_read_replica: i32,
    pub records: Vec<u8>, // Encoded records
}

/// Aborted transaction
#[derive(Debug, Clone)]
pub struct AbortedTransaction {
    pub producer_id: i64,
    pub first_offset: i64,
}

/// Metadata request
#[derive(Debug, Clone)]
pub struct MetadataRequest {
    pub topics: Option<Vec<String>>,
    pub allow_auto_topic_creation: bool,
    pub include_cluster_authorized_operations: bool,
    pub include_topic_authorized_operations: bool,
}

/// Metadata response
#[derive(Debug, Clone)]
pub struct MetadataResponse {
    pub throttle_time_ms: i32,
    pub brokers: Vec<MetadataBroker>,
    pub cluster_id: Option<String>,
    pub controller_id: i32,
    pub topics: Vec<MetadataTopic>,
    pub cluster_authorized_operations: Option<i32>,
}

/// Metadata broker
#[derive(Debug, Clone)]
pub struct MetadataBroker {
    pub node_id: i32,
    pub host: String,
    pub port: i32,
    pub rack: Option<String>,
}

/// Metadata topic
#[derive(Debug, Clone)]
pub struct MetadataTopic {
    pub error_code: i16,
    pub name: String,
    pub is_internal: bool,
    pub partitions: Vec<MetadataPartition>,
}

/// Metadata partition
#[derive(Debug, Clone)]
pub struct MetadataPartition {
    pub error_code: i16,
    pub partition_index: i32,
    pub leader_id: i32,
    pub leader_epoch: i32,
    pub replica_nodes: Vec<i32>,
    pub isr_nodes: Vec<i32>,
    pub offline_replicas: Vec<i32>,
}

/// Join group request
#[derive(Debug, Clone)]
pub struct JoinGroupRequest {
    pub group_id: String,
    pub session_timeout: i32,
    pub rebalance_timeout: i32,
    pub member_id: String,
    pub protocol_type: String,
    pub protocols: Vec<JoinGroupProtocol>,
}

/// Join group protocol
#[derive(Debug, Clone)]
pub struct JoinGroupProtocol {
    pub name: String,
    pub metadata: Vec<u8>,
}

/// Join group response
#[derive(Debug, Clone)]
pub struct JoinGroupResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub error_code: i16,
    pub generation_id: i32,
    pub protocol: String,
    pub leader_id: String,
    pub member_id: String,
    pub members: Vec<JoinGroupMember>,
}

/// Join group member
#[derive(Debug, Clone)]
pub struct JoinGroupMember {
    pub member_id: String,
    pub metadata: Vec<u8>,
}

/// Sync group request
#[derive(Debug, Clone)]
pub struct SyncGroupRequest {
    pub group_id: String,
    pub generation_id: i32,
    pub member_id: String,
    pub assignments: Vec<SyncGroupAssignment>,
}

/// Sync group assignment
#[derive(Debug, Clone)]
pub struct SyncGroupAssignment {
    pub member_id: String,
    pub assignment: Vec<u8>,
}

/// Sync group response
#[derive(Debug, Clone)]
pub struct SyncGroupResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub error_code: i16,
    pub assignment: Vec<u8>,
}

/// Heartbeat request
#[derive(Debug, Clone)]
pub struct HeartbeatRequest {
    pub group_id: String,
    pub generation_id: i32,
    pub member_id: String,
}

/// Heartbeat response
#[derive(Debug, Clone)]
pub struct HeartbeatResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub error_code: i16,
}

/// Leave group request
#[derive(Debug, Clone)]
pub struct LeaveGroupRequest {
    pub group_id: String,
    pub member_id: String,
}

/// Leave group response
#[derive(Debug, Clone)]
pub struct LeaveGroupResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub error_code: i16,
}

/// Offset commit request
#[derive(Debug, Clone)]
pub struct OffsetCommitRequest {
    pub group_id: String,
    pub generation_id: i32,
    pub member_id: String,
    pub topics: Vec<OffsetCommitTopic>,
}

/// Offset commit topic
#[derive(Debug, Clone)]
pub struct OffsetCommitTopic {
    pub name: String,
    pub partitions: Vec<OffsetCommitPartition>,
}

/// Offset commit partition
#[derive(Debug, Clone)]
pub struct OffsetCommitPartition {
    pub partition_index: i32,
    pub committed_offset: i64,
    pub committed_metadata: Option<String>,
}

/// Offset commit response
#[derive(Debug, Clone)]
pub struct OffsetCommitResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub topics: Vec<OffsetCommitResponseTopic>,
}

/// Offset commit response topic
#[derive(Debug, Clone)]
pub struct OffsetCommitResponseTopic {
    pub name: String,
    pub partitions: Vec<OffsetCommitResponsePartition>,
}

/// Offset commit response partition
#[derive(Debug, Clone)]
pub struct OffsetCommitResponsePartition {
    pub partition_index: i32,
    pub error_code: i16,
}

/// Offset fetch request
#[derive(Debug, Clone)]
pub struct OffsetFetchRequest {
    pub group_id: String,
    pub topics: Option<Vec<String>>,
}

/// Offset fetch response
#[derive(Debug, Clone)]
pub struct OffsetFetchResponse {
    pub header: ResponseHeader,
    pub throttle_time_ms: i32,
    pub topics: Vec<OffsetFetchResponseTopic>,
}

/// Offset fetch response topic
#[derive(Debug, Clone)]
pub struct OffsetFetchResponseTopic {
    pub name: String,
    pub partitions: Vec<OffsetFetchResponsePartition>,
}

/// Offset fetch response partition
#[derive(Debug, Clone)]
pub struct OffsetFetchResponsePartition {
    pub partition_index: i32,
    pub committed_offset: i64,
    pub metadata: Option<String>,
    pub error_code: i16,
}

/// API versions request
#[derive(Debug, Clone)]
pub struct ApiVersionsRequest {
    pub client_software_name: String,
    pub client_software_version: String,
}

/// API versions response
#[derive(Debug, Clone)]
pub struct ApiVersionsResponse {
    pub header: ResponseHeader,
    pub error_code: i16,
    pub api_keys: Vec<ApiVersionsResponseKey>,
    pub throttle_time_ms: i32,
}

/// API version key info
#[derive(Debug, Clone)]
pub struct ApiVersionsResponseKey {
    pub api_key: i16,
    pub min_version: i16,
    pub max_version: i16,
}

impl Request {
    /// Decode a request from bytes
    pub fn decode(data: &[u8]) -> Result<Self, String> {
        use crate::parser::parse_request_header_with_correlation;
        use bytes::Bytes;
        use chronik_common::Error;
        
        let mut buf = Bytes::copy_from_slice(data);
        let header = match parse_request_header_with_correlation(&mut buf) {
            Ok((h, _)) => h,
            Err(Error::ProtocolWithCorrelation { message, .. }) => {
                return Err(format!("Failed to parse header: {}", message));
            }
            Err(e) => {
                return Err(format!("Failed to parse header: {:?}", e));
            }
        };
        
        // For now, just parse metadata requests
        let body = match header.api_key {
            crate::parser::ApiKey::Metadata => {
                RequestBody::Metadata(MetadataRequest {
                    topics: None,
                    allow_auto_topic_creation: false,
                    include_cluster_authorized_operations: false,
                    include_topic_authorized_operations: false,
                })
            }
            crate::parser::ApiKey::Produce => {
                RequestBody::Produce(ProduceRequest {
                    transactional_id: None,
                    acks: -1,
                    timeout_ms: 30000,
                    topics: vec![],
                })
            }
            crate::parser::ApiKey::Fetch => {
                RequestBody::Fetch(FetchRequest {
                    replica_id: -1,
                    max_wait_ms: 500,
                    min_bytes: 1,
                    max_bytes: 1048576,
                    isolation_level: 0,
                    session_id: 0,
                    session_epoch: -1,
                    topics: vec![],
                })
            }
            crate::parser::ApiKey::ApiVersions => {
                RequestBody::ApiVersions(ApiVersionsRequest {
                    client_software_name: String::new(),
                    client_software_version: String::new(),
                })
            }
            _ => return Err(format!("Unsupported API key: {:?}", header.api_key)),
        };
        
        Ok(Request { header, body })
    }
}

impl Response {
    /// Encode a response to bytes (deprecated - use encode_versioned)
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        // Default to highest version for backward compatibility
        // This is incorrect but maintains existing behavior
        self.encode_versioned(9)
    }
    
    /// Encode a response to bytes with version awareness
    pub fn encode_versioned(&self, api_version: i16) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        
        match self {
            Response::Metadata(resp) => {
                // Note: correlation_id is handled in the response header, not here
                
                // Throttle time only for v3+
                if api_version >= 3 {
                    bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                }
                
                // Brokers array
                bytes.extend_from_slice(&(resp.brokers.len() as i32).to_be_bytes());
                for broker in &resp.brokers {
                    bytes.extend_from_slice(&broker.node_id.to_be_bytes());
                    bytes.extend_from_slice(&(broker.host.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(broker.host.as_bytes());
                    bytes.extend_from_slice(&broker.port.to_be_bytes());
                    // Rack (null)
                    bytes.extend_from_slice(&(-1i16).to_be_bytes());
                }
                
                // Cluster ID
                if let Some(cluster_id) = &resp.cluster_id {
                    bytes.extend_from_slice(&(cluster_id.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(cluster_id.as_bytes());
                } else {
                    bytes.extend_from_slice(&(-1i16).to_be_bytes());
                }
                
                // Controller ID
                bytes.extend_from_slice(&resp.controller_id.to_be_bytes());
                
                // Topics array
                bytes.extend_from_slice(&(resp.topics.len() as i32).to_be_bytes());
                for topic in &resp.topics {
                    bytes.extend_from_slice(&topic.error_code.to_be_bytes());
                    bytes.extend_from_slice(&(topic.name.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(topic.name.as_bytes());
                    bytes.push(if topic.is_internal { 1 } else { 0 });
                    
                    // Partitions
                    bytes.extend_from_slice(&(topic.partitions.len() as i32).to_be_bytes());
                    for partition in &topic.partitions {
                        bytes.extend_from_slice(&partition.error_code.to_be_bytes());
                        bytes.extend_from_slice(&partition.partition_index.to_be_bytes());
                        bytes.extend_from_slice(&partition.leader_id.to_be_bytes());
                        bytes.extend_from_slice(&partition.leader_epoch.to_be_bytes());
                        
                        // Replica nodes
                        bytes.extend_from_slice(&(partition.replica_nodes.len() as i32).to_be_bytes());
                        for replica in &partition.replica_nodes {
                            bytes.extend_from_slice(&replica.to_be_bytes());
                        }
                        
                        // ISR nodes
                        bytes.extend_from_slice(&(partition.isr_nodes.len() as i32).to_be_bytes());
                        for isr in &partition.isr_nodes {
                            bytes.extend_from_slice(&isr.to_be_bytes());
                        }
                        
                        // Offline replicas
                        bytes.extend_from_slice(&(partition.offline_replicas.len() as i32).to_be_bytes());
                        for offline in &partition.offline_replicas {
                            bytes.extend_from_slice(&offline.to_be_bytes());
                        }
                    }
                }
            }
            Response::Produce(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());

                // Use comprehensive encoding with flexible protocol support
                use crate::parser::Encoder;
                use bytes::BytesMut;

                let mut body_buf = BytesMut::new();
                let mut encoder = Encoder::new(&mut body_buf);

                // Check if this is a flexible/compact version (v9+)
                let flexible = api_version >= 9;

                // NOTE: throttle_time_ms position differs between versions
                // For v1-v8: throttle_time_ms goes at the END of the response
                // For v9+: throttle_time_ms goes at the BEGINNING of the response

                // Write throttle_time_ms at the BEGINNING for v9+
                if api_version >= 9 {
                    encoder.write_i32(resp.throttle_time_ms);
                }

                // Topics array (called "responses" in the protocol spec)
                if flexible {
                    // v9+ uses compact array
                    encoder.write_unsigned_varint((resp.topics.len() + 1) as u32);
                } else {
                    encoder.write_i32(resp.topics.len() as i32);
                }

                for topic in &resp.topics {
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

                        if api_version >= 2 {
                            encoder.write_i64(partition.log_append_time);
                        }

                        if api_version >= 5 {
                            encoder.write_i64(partition.log_start_offset);
                        }

                        // Record errors (COMPACT_ARRAY of RecordError) - v8+
                        if api_version >= 8 {
                            if flexible {
                                encoder.write_unsigned_varint(1); // Empty array (0 + 1 for compact encoding)
                            } else {
                                encoder.write_i32(0); // Empty array
                            }
                        }

                        // Error message (NULLABLE_COMPACT_STRING) - v8+
                        if api_version >= 8 {
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
                if api_version >= 1 && api_version < 9 {
                    encoder.write_i32(resp.throttle_time_ms);
                }

                // Append the encoded body to bytes
                bytes.extend_from_slice(&body_buf);
            }
            Response::Fetch(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());

                // Use comprehensive encoding with flexible protocol support
                use crate::parser::Encoder;
                use bytes::BytesMut;

                let mut body_buf = BytesMut::new();
                let mut encoder = Encoder::new(&mut body_buf);

                // Check if this is a flexible/compact version (v12+)
                let flexible = api_version >= 12;

                // Throttle time (v1+)
                if api_version >= 1 {
                    encoder.write_i32(resp.throttle_time_ms);
                }

                // Error code (v7+, for session-level errors)
                if api_version >= 7 {
                    encoder.write_i16(resp.error_code);
                }

                // Session ID (v7+, for incremental fetch sessions)
                if api_version >= 7 {
                    encoder.write_i32(resp.session_id);
                }

                // Topics array (called "responses" in the protocol spec)
                if flexible {
                    // v12+ uses compact array
                    encoder.write_unsigned_varint((resp.topics.len() + 1) as u32);
                } else {
                    encoder.write_i32(resp.topics.len() as i32);
                }

                for topic in &resp.topics {
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
                        // Partition index
                        encoder.write_i32(partition.partition);

                        // Error code
                        encoder.write_i16(partition.error_code);

                        // High watermark
                        encoder.write_i64(partition.high_watermark);

                        // Last stable offset (v4+)
                        if api_version >= 4 {
                            encoder.write_i64(partition.last_stable_offset);
                        }

                        // Log start offset (v5+)
                        if api_version >= 5 {
                            encoder.write_i64(partition.log_start_offset);
                        }

                        // Aborted transactions (v4+)
                        if api_version >= 4 {
                            if let Some(ref aborted_txns) = partition.aborted {
                                if flexible {
                                    encoder.write_unsigned_varint((aborted_txns.len() + 1) as u32);
                                } else {
                                    encoder.write_i32(aborted_txns.len() as i32);
                                }
                                for txn in aborted_txns {
                                    encoder.write_i64(txn.producer_id);
                                    encoder.write_i64(txn.first_offset);
                                    if flexible {
                                        encoder.write_unsigned_varint(0); // Tagged fields for aborted txn
                                    }
                                }
                            } else {
                                // Empty array
                                if flexible {
                                    encoder.write_unsigned_varint(1); // 0 + 1 for compact encoding
                                } else {
                                    encoder.write_i32(0);
                                }
                            }
                        }

                        // Preferred read replica (v11+)
                        if api_version >= 11 {
                            encoder.write_i32(partition.preferred_read_replica);
                        }

                        // Records (NULLABLE_BYTES for v0-v11, COMPACT_RECORDS for v12+)
                        if partition.records.is_empty() {
                            // Empty records
                            if flexible {
                                encoder.write_unsigned_varint(0); // Null in compact encoding
                            } else {
                                encoder.write_i32(0); // Empty byte array
                            }
                        } else {
                            // Has records
                            if flexible {
                                encoder.write_unsigned_varint((partition.records.len() + 1) as u32);
                            } else {
                                encoder.write_i32(partition.records.len() as i32);
                            }
                            encoder.write_raw_bytes(&partition.records);
                        }

                        // Tagged fields for partition (v12+)
                        if flexible {
                            encoder.write_unsigned_varint(0); // No tagged fields
                        }
                    }

                    // Tagged fields for topic (v12+)
                    if flexible {
                        encoder.write_unsigned_varint(0); // No tagged fields
                    }
                }

                // Tagged fields at response level (v12+)
                if flexible {
                    encoder.write_unsigned_varint(0); // No tagged fields
                }

                // Append the encoded body to bytes
                bytes.extend_from_slice(&body_buf);
            }
            Response::JoinGroup(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
                
                // Throttle time
                bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                
                // Error code
                bytes.extend_from_slice(&resp.error_code.to_be_bytes());
                
                // Generation ID
                bytes.extend_from_slice(&resp.generation_id.to_be_bytes());
                
                // Protocol
                bytes.extend_from_slice(&(resp.protocol.len() as i16).to_be_bytes());
                bytes.extend_from_slice(resp.protocol.as_bytes());
                
                // Leader ID
                bytes.extend_from_slice(&(resp.leader_id.len() as i16).to_be_bytes());
                bytes.extend_from_slice(resp.leader_id.as_bytes());
                
                // Member ID
                bytes.extend_from_slice(&(resp.member_id.len() as i16).to_be_bytes());
                bytes.extend_from_slice(resp.member_id.as_bytes());
                
                // Members
                bytes.extend_from_slice(&(resp.members.len() as i32).to_be_bytes());
                for member in &resp.members {
                    bytes.extend_from_slice(&(member.member_id.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(member.member_id.as_bytes());
                    bytes.extend_from_slice(&(member.metadata.len() as i32).to_be_bytes());
                    bytes.extend_from_slice(&member.metadata);
                }
            }
            Response::SyncGroup(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
                
                // Throttle time
                bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                
                // Error code
                bytes.extend_from_slice(&resp.error_code.to_be_bytes());
                
                // Assignment
                bytes.extend_from_slice(&(resp.assignment.len() as i32).to_be_bytes());
                bytes.extend_from_slice(&resp.assignment);
            }
            Response::Heartbeat(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
                
                // Throttle time
                bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                
                // Error code
                bytes.extend_from_slice(&resp.error_code.to_be_bytes());
            }
            Response::LeaveGroup(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
                
                // Throttle time
                bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                
                // Error code
                bytes.extend_from_slice(&resp.error_code.to_be_bytes());
            }
            Response::OffsetCommit(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
                
                // Throttle time
                bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                
                // Topics
                bytes.extend_from_slice(&(resp.topics.len() as i32).to_be_bytes());
                for topic in &resp.topics {
                    bytes.extend_from_slice(&(topic.name.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(topic.name.as_bytes());
                    
                    // Partitions
                    bytes.extend_from_slice(&(topic.partitions.len() as i32).to_be_bytes());
                    for partition in &topic.partitions {
                        bytes.extend_from_slice(&partition.partition_index.to_be_bytes());
                        bytes.extend_from_slice(&partition.error_code.to_be_bytes());
                    }
                }
            }
            Response::OffsetFetch(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
                
                // Throttle time
                bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                
                // Topics
                bytes.extend_from_slice(&(resp.topics.len() as i32).to_be_bytes());
                for topic in &resp.topics {
                    bytes.extend_from_slice(&(topic.name.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(topic.name.as_bytes());
                    
                    // Partitions
                    bytes.extend_from_slice(&(topic.partitions.len() as i32).to_be_bytes());
                    for partition in &topic.partitions {
                        bytes.extend_from_slice(&partition.partition_index.to_be_bytes());
                        bytes.extend_from_slice(&partition.committed_offset.to_be_bytes());
                        
                        // Metadata (nullable string)
                        if let Some(metadata) = &partition.metadata {
                            bytes.extend_from_slice(&(metadata.len() as i16).to_be_bytes());
                            bytes.extend_from_slice(metadata.as_bytes());
                        } else {
                            bytes.extend_from_slice(&(-1i16).to_be_bytes());
                        }
                        
                        bytes.extend_from_slice(&partition.error_code.to_be_bytes());
                    }
                }
            }
            Response::ApiVersions(resp) => {
                // Correlation ID
                bytes.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
                
                // Error code
                bytes.extend_from_slice(&resp.error_code.to_be_bytes());
                
                // API keys array
                bytes.extend_from_slice(&(resp.api_keys.len() as i32).to_be_bytes());
                for api_key in &resp.api_keys {
                    bytes.extend_from_slice(&api_key.api_key.to_be_bytes());
                    bytes.extend_from_slice(&api_key.min_version.to_be_bytes());
                    bytes.extend_from_slice(&api_key.max_version.to_be_bytes());
                    // Tagged fields (empty)
                    bytes.push(0);
                }
                
                // Throttle time
                bytes.extend_from_slice(&resp.throttle_time_ms.to_be_bytes());
                
                // Tagged fields (empty)
                bytes.push(0);
            }
            Response::SaslHandshake(resp) => {
                // Error code
                bytes.extend_from_slice(&resp.error_code.to_be_bytes());
                
                // Mechanisms array
                bytes.extend_from_slice(&(resp.mechanisms.len() as i32).to_be_bytes());
                for mechanism in &resp.mechanisms {
                    bytes.extend_from_slice(&(mechanism.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(mechanism.as_bytes());
                }
            }
            Response::SaslAuthenticate(resp) => {
                // Error code
                bytes.extend_from_slice(&resp.error_code.to_be_bytes());
                
                // Error message
                if let Some(error_msg) = &resp.error_message {
                    bytes.extend_from_slice(&(error_msg.len() as i16).to_be_bytes());
                    bytes.extend_from_slice(error_msg.as_bytes());
                } else {
                    bytes.extend_from_slice(&(-1i16).to_be_bytes());
                }
                
                // Auth bytes
                bytes.extend_from_slice(&(resp.auth_bytes.len() as i32).to_be_bytes());
                bytes.extend_from_slice(&resp.auth_bytes);
                
                // Session lifetime
                bytes.extend_from_slice(&resp.session_lifetime_ms.to_be_bytes());
            }
            Response::DescribeConfigs(_resp) => {
                // DescribeConfigs encoding is handled in the handler
                // This shouldn't be called directly
                return Err("DescribeConfigs response should be encoded in handler".into());
            }
        }
        
        Ok(bytes)
    }
}

/// DescribeConfigs request
#[derive(Debug, Clone)]
pub struct DescribeConfigsRequest {
    pub resources: Vec<ConfigResource>,
    pub include_synonyms: bool,
    pub include_documentation: bool,
}

/// Config resource to describe
#[derive(Debug, Clone)]
pub struct ConfigResource {
    pub resource_type: i8,  // 2 = topic, 4 = broker
    pub resource_name: String,
    pub configuration_keys: Option<Vec<String>>,  // None = all configs
}

/// DescribeConfigs response
#[derive(Debug, Clone)]
pub struct DescribeConfigsResponse {
    pub throttle_time_ms: i32,
    pub results: Vec<DescribeConfigsResult>,
}

/// Result for a single resource
#[derive(Debug, Clone)]
pub struct DescribeConfigsResult {
    pub error_code: i16,
    pub error_message: Option<String>,
    pub resource_type: i8,
    pub resource_name: String,
    pub configs: Vec<ConfigEntry>,
}

/// Configuration entry
#[derive(Debug, Clone)]
pub struct ConfigEntry {
    pub name: String,
    pub value: Option<String>,
    pub read_only: bool,
    pub is_default: bool,
    pub config_source: i8,  // 0 = UNKNOWN, 1 = TOPIC_CONFIG, 2 = DYNAMIC_BROKER_CONFIG, etc.
    pub is_sensitive: bool,
    pub synonyms: Vec<ConfigSynonym>,
    pub config_type: Option<i8>,  // v3+: 1 = BOOLEAN, 2 = STRING, 3 = INT, etc.
    pub documentation: Option<String>,  // v3+
}

/// Config synonym
#[derive(Debug, Clone)]
pub struct ConfigSynonym {
    pub name: String,
    pub value: Option<String>,
    pub source: i8,
}

/// Config source constants
pub mod config_source {
    pub const UNKNOWN_CONFIG: i8 = 0;
    pub const TOPIC_CONFIG: i8 = 1;
    pub const DYNAMIC_BROKER_CONFIG: i8 = 2;
    pub const DYNAMIC_DEFAULT_BROKER_CONFIG: i8 = 3;
    pub const STATIC_BROKER_CONFIG: i8 = 4;
    pub const DEFAULT_CONFIG: i8 = 5;
    pub const DYNAMIC_BROKER_LOGGER_CONFIG: i8 = 6;
}

/// Config type constants (v3+)
pub mod config_type {
    pub const UNKNOWN: i8 = 0;
    pub const BOOLEAN: i8 = 1;
    pub const STRING: i8 = 2;
    pub const INT: i8 = 3;
    pub const SHORT: i8 = 4;
    pub const LONG: i8 = 5;
    pub const DOUBLE: i8 = 6;
    pub const LIST: i8 = 7;
    pub const CLASS: i8 = 8;
    pub const PASSWORD: i8 = 9;
}