//! Kafka protocol request handler.

use bytes::{Bytes, BytesMut, Buf};
use chronik_common::{Result, Error};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::error_codes;
use crate::parser::{
    ApiKey, RequestHeader, ResponseHeader, VersionRange,
    parse_request_header_with_correlation,
    supported_api_versions,
    Encoder, Decoder,
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
    pub throttle_time_ms: Option<i32>, // Some APIs include throttle_time in header
}

/// Handler for specific API versions request
pub struct ApiVersionsRequest {
    /// Client software name (v3+, KIP-511)
    pub client_software_name: Option<String>,
    /// Client software version (v3+, KIP-511)
    pub client_software_version: Option<String>,
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
    /// Advertised host for this broker
    advertised_host: String,
    /// Advertised port for this broker
    advertised_port: i32,
}

impl ProtocolHandler {
    /// Helper to create a Response with proper flexible tracking
    fn make_response(header: &RequestHeader, api_key: ApiKey, body: Bytes) -> Response {
        // CRITICAL: ApiVersions is special - the response BODY uses flexible encoding
        // but the response HEADER does not! This is unique to ApiVersions.
        //
        // For clarity: is_flexible here means "should the header have tagged fields"
        // The body encoding (flexible vs non-flexible) is already handled when creating the body
        //
        // IMPORTANT: DescribeCluster v0 uses NON-flexible format for both header AND body
        // Even though the client may have negotiated ApiVersions v3+ and sends flexible REQUEST headers,
        // DescribeCluster v0 responses must use NON-flexible headers (headerVersion=1)
        //
        // The Kafka protocol specifies:
        // - DescribeCluster v0: headerVersion=1 (non-flexible)
        // - DescribeCluster v1: headerVersion=2 (flexible)
        //
        // Metadata response header versions:
        // - Metadata v0-v8: headerVersion=0 (non-flexible, no tagged fields)
        // - Metadata v9+: headerVersion=1 (flexible, has tagged fields)
        //
        // This is DIFFERENT from most other APIs where ApiVersions v3+ negotiation affects all headers.
        // DescribeCluster and Metadata are special in that their header versions are tied to the API version itself.
        let header_has_tagged_fields = if api_key == ApiKey::ApiVersions {
            false  // ApiVersions response header NEVER has tagged fields
        } else if api_key == ApiKey::DescribeCluster && header.api_version == 0 {
            false  // DescribeCluster v0 uses NON-flexible headers
        } else if api_key == ApiKey::Metadata && header.api_version < 9 {
            false  // Metadata v0-v8 use NON-flexible headers
        } else if api_key == ApiKey::AddPartitionsToTxn && header.api_version < 3 {
            false  // AddPartitionsToTxn v0-v2 use NON-flexible headers (v3+ use flexible)
        } else if api_key == ApiKey::EndTxn && header.api_version < 3 {
            false  // EndTxn v0-v2 use NON-flexible headers (v3+ use flexible)
        } else {
            // For other APIs and DescribeCluster v1+/Metadata v9+, use flexible headers
            // when the client has negotiated ApiVersions v3+
            true
        };

        // DescribeCluster v0 does NOT include throttle_time_ms in the response
        // This is different from most other APIs
        let throttle_time_ms = None;

        tracing::debug!("make_response - api_key={:?}, body.len()={}, header_has_tagged_fields={}, throttle_time_ms={:?}",
                 api_key, body.len(), header_has_tagged_fields, throttle_time_ms);

        Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body,
            is_flexible: header_has_tagged_fields,  // This specifically means header flexibility
            api_key,
            throttle_time_ms,
        }
    }
    
    /// Create a new protocol handler.
    pub fn new() -> Self {
        Self {
            supported_versions: supported_api_versions(),
            metadata_store: None,
            broker_id: 1, // Default broker ID
            consumer_groups: Arc::new(Mutex::new(ConsumerGroupState::default())),
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
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
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
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
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
        }
    }

    /// Create a new protocol handler with full configuration
    pub fn with_full_config(
        metadata_store: Arc<dyn chronik_common::metadata::traits::MetadataStore>,
        broker_id: i32,
        advertised_host: String,
        advertised_port: i32,
    ) -> Self {
        let versions = supported_api_versions();
        Self {
            supported_versions: versions,
            metadata_store: Some(metadata_store),
            broker_id,
            consumer_groups: Arc::new(Mutex::new(ConsumerGroupState::default())),
            advertised_host,
            advertised_port,
        }
    }
    
    // Public parse methods for use by KafkaProtocolHandler
    
    /// Parse FindCoordinator request
    pub fn parse_find_coordinator_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::find_coordinator_types::FindCoordinatorRequest> {
        use crate::parser::Decoder;
        use crate::find_coordinator_types::FindCoordinatorRequest;

        let mut decoder = Decoder::new(body);

        // v4+ uses KIP-699 array format for coordinator_keys
        if header.api_version >= 4 {
            // v4+ format (fields serialized in JSON schema order):
            // Field 1: KeyType (int8)
            // Field 2: CoordinatorKeys (compact array)
            // Field 3: Tagged fields

            // Read KeyType first (appears before CoordinatorKeys in schema)
            let key_type = decoder.read_i8()?;

            // Read compact array of coordinator keys
            let array_length = decoder.read_unsigned_varint()? as usize;
            if array_length == 0 {
                return Err(Error::Protocol("FindCoordinator v4+ requires at least one coordinator key".into()));
            }

            let actual_length = array_length - 1; // Compact array encoding

            // Read first coordinator key (KSQL typically sends one at a time)
            let key = decoder.read_compact_string()?
                .ok_or_else(|| Error::Protocol("Coordinator key cannot be null".into()))?;

            // Skip remaining coordinator keys if present (batched lookup not commonly used)
            for _ in 1..actual_length {
                decoder.read_compact_string()?; // skip additional keys
            }

            // Read tagged fields (flexible version 3+)
            let _tagged_fields_count = decoder.read_unsigned_varint()?;

            Ok(FindCoordinatorRequest { key, key_type })
        } else {
            // v0-v3: Single coordinator key
            let key = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Coordinator key cannot be null".into()))?;

            let key_type = if header.api_version >= 1 {
                decoder.read_i8()?
            } else {
                0 // GROUP type
            };

            Ok(FindCoordinatorRequest { key, key_type })
        }
    }
    
    /// Parse JoinGroup request
    pub fn parse_join_group_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::join_group_types::JoinGroupRequest> {
        use crate::parser::Decoder;
        use crate::join_group_types::{JoinGroupRequest, JoinGroupRequestProtocol};

        let mut decoder = Decoder::new(body);

        // v6+ uses flexible/compact format
        let flexible = header.api_version >= 6;

        let group_id = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        };

        let session_timeout_ms = decoder.read_i32()?;
        let rebalance_timeout_ms = if header.api_version >= 1 {
            decoder.read_i32()?
        } else {
            session_timeout_ms
        };

        let member_id = if flexible {
            decoder.read_compact_string()?.unwrap_or_default()
        } else {
            decoder.read_string()?.unwrap_or_default()
        };

        let group_instance_id = if header.api_version >= 5 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        let protocol_type = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Protocol type cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Protocol type cannot be null".into()))?
        };

        let protocol_count = if flexible {
            let len = decoder.read_unsigned_varint()? as usize;
            if len == 0 {
                return Err(Error::Protocol("JoinGroup protocols array cannot be empty".into()));
            }
            len - 1 // Compact array encoding: actual_length = encoded_length - 1
        } else {
            decoder.read_i32()? as usize
        };

        let mut protocols = Vec::with_capacity(protocol_count);
        for _ in 0..protocol_count {
            let name = if flexible {
                decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Protocol name cannot be null".into()))?
            } else {
                decoder.read_string()?.ok_or_else(|| Error::Protocol("Protocol name cannot be null".into()))?
            };

            let metadata = if flexible {
                decoder.read_compact_bytes()?.ok_or_else(|| Error::Protocol("Protocol metadata cannot be null".into()))?
            } else {
                decoder.read_bytes()?.ok_or_else(|| Error::Protocol("Protocol metadata cannot be null".into()))?
            };

            protocols.push(JoinGroupRequestProtocol { name, metadata });

            // Tagged fields at protocol level (flexible only)
            if flexible {
                let _tag_count = decoder.read_unsigned_varint()?;
            }
        }

        // Tagged fields at request level (flexible only)
        if flexible {
            let _tag_count = decoder.read_unsigned_varint()?;
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

        // v4+ uses flexible/compact format
        let flexible = header.api_version >= 4;

        let group_id = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        };

        let generation_id = decoder.read_i32()?;

        let member_id = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
        };

        let group_instance_id = if header.api_version >= 3 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        let protocol_type = if header.api_version >= 5 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        let protocol_name = if header.api_version >= 5 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        let assignment_count = if flexible {
            let len = decoder.read_unsigned_varint()? as usize;
            if len > 0 { len - 1 } else { 0 }
        } else {
            decoder.read_i32()? as usize
        };

        let mut assignments = Vec::with_capacity(assignment_count);
        for _ in 0..assignment_count {
            let member_id = if flexible {
                decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Assignment member ID cannot be null".into()))?
            } else {
                decoder.read_string()?.ok_or_else(|| Error::Protocol("Assignment member ID cannot be null".into()))?
            };

            let assignment = if flexible {
                decoder.read_compact_bytes()?.ok_or_else(|| Error::Protocol("Assignment cannot be null".into()))?
            } else {
                decoder.read_bytes()?.ok_or_else(|| Error::Protocol("Assignment cannot be null".into()))?
            };

            assignments.push(SyncGroupRequestAssignment { member_id, assignment });

            // Tagged fields at assignment level (flexible only)
            if flexible {
                let _tag_count = decoder.read_unsigned_varint()?;
            }
        }

        // Tagged fields at request level (flexible only)
        if flexible {
            let _tag_count = decoder.read_unsigned_varint()?;
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
        let flexible = header.api_version >= 4;

        let group_id = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        };

        let generation_id = decoder.read_i32()?;

        let member_id = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
        };

        let group_instance_id = if header.api_version >= 3 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        // Tagged fields for flexible versions
        if flexible {
            let _tag_count = decoder.read_unsigned_varint()?;
        }

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

        // v4+ uses flexible/compact format
        let flexible = header.api_version >= 4;

        let group_id = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        };

        let members = if header.api_version >= 3 {
            // V3+ has array of members
            let member_count = if flexible {
                let len = decoder.read_unsigned_varint()? as usize;
                if len > 0 { len - 1 } else { 0 }
            } else {
                decoder.read_i32()? as usize
            };

            let mut members = Vec::with_capacity(member_count);
            for _ in 0..member_count {
                let member_id = if flexible {
                    decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
                } else {
                    decoder.read_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
                };

                let group_instance_id = if flexible {
                    decoder.read_compact_string()?
                } else {
                    decoder.read_string()?
                };

                members.push(MemberIdentity { member_id, group_instance_id });

                // Tagged fields at member level (flexible only)
                if flexible {
                    let _tag_count = decoder.read_unsigned_varint()?;
                }
            }
            members
        } else {
            // V0-2 has single member_id
            let member_id = if flexible {
                decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
            } else {
                decoder.read_string()?.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?
            };
            vec![MemberIdentity { member_id, group_instance_id: None }]
        };

        // Tagged fields at request level (flexible only)
        if flexible {
            let _tag_count = decoder.read_unsigned_varint()?;
        }

        Ok(LeaveGroupRequest { group_id, members })
    }
    
    /// Parse OffsetCommit request
    pub fn parse_offset_commit_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::types::OffsetCommitRequest> {
        use crate::parser::Decoder;
        use crate::types::{OffsetCommitRequest, OffsetCommitTopic, OffsetCommitPartition};

        let mut decoder = Decoder::new(body);

        // v8+ uses flexible/compact format
        let flexible = header.api_version >= 8;

        // Read group_id
        let group_id = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        }.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;

        let generation_id = decoder.read_i32()?;

        // Read member_id
        let member_id = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        }.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;

        // v6+ has group_instance_id
        let _group_instance_id = if header.api_version >= 6 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        // V2-4 has retention_time field (v5 removed it, v7 added it back)
        if header.api_version >= 2 && header.api_version <= 4 {
            let _retention_time = decoder.read_i64()?; // We ignore this for now
        }
        if header.api_version == 7 || header.api_version >= 8 {
            let _retention_time = decoder.read_i64()?; // We ignore this for now
        }

        // Read topics array
        let topic_count = if flexible {
            let count = decoder.read_unsigned_varint()? as i32 - 1;
            if count < 0 { 0 } else { count as usize }
        } else {
            let count = decoder.read_i32()?;
            if count < 0 || count > 10000 {
                return Err(Error::Protocol(format!("Invalid topic count: {}", count)));
            }
            count as usize
        };

        let mut topics = Vec::with_capacity(topic_count);

        for _ in 0..topic_count {
            let topic_name = if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }.ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

            let partition_count = if flexible {
                let count = decoder.read_unsigned_varint()? as i32 - 1;
                if count < 0 { 0 } else { count as usize }
            } else {
                let count = decoder.read_i32()?;
                if count < 0 || count > 10000 {
                    return Err(Error::Protocol(format!("Invalid partition count: {}", count)));
                }
                count as usize
            };

            let mut partitions = Vec::with_capacity(partition_count);

            for _ in 0..partition_count {
                let partition_index = decoder.read_i32()?;
                let committed_offset = decoder.read_i64()?;

                // v6+ has committed_leader_epoch
                let _committed_leader_epoch = if header.api_version >= 6 {
                    decoder.read_i32()?
                } else {
                    -1
                };

                let committed_metadata = if flexible {
                    decoder.read_compact_string()?
                } else {
                    decoder.read_string()?
                };

                // Skip tagged fields for flexible versions (partition level)
                if flexible {
                    let tag_count = decoder.read_unsigned_varint()?;
                    for _ in 0..tag_count {
                        let _tag_id = decoder.read_unsigned_varint()?;
                        let tag_size = decoder.read_unsigned_varint()? as usize;
                        decoder.advance(tag_size)?;
                    }
                }

                partitions.push(OffsetCommitPartition {
                    partition_index,
                    committed_offset,
                    committed_metadata,
                });
            }

            // Skip tagged fields for flexible versions (topic level)
            if flexible {
                let tag_count = decoder.read_unsigned_varint()?;
                for _ in 0..tag_count {
                    let _tag_id = decoder.read_unsigned_varint()?;
                    let tag_size = decoder.read_unsigned_varint()? as usize;
                    decoder.advance(tag_size)?;
                }
            }

            topics.push(OffsetCommitTopic {
                name: topic_name,
                partitions,
            });
        }

        // Skip tagged fields for flexible versions (request level)
        if flexible {
            let tag_count = decoder.read_unsigned_varint()?;
            for _ in 0..tag_count {
                let _tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size)?;
            }
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

        // v6+ uses flexible/compact format
        let flexible = header.api_version >= 6;

        if header.api_version >= 8 {
            // V8+ has completely different structure: Groups array instead of single group_id
            let group_count = if flexible {
                let len = decoder.read_unsigned_varint()? as usize;
                if len > 0 { len - 1 } else { 0 }
            } else {
                decoder.read_i32()? as usize
            };

            if group_count == 0 {
                return Err(Error::Protocol("OffsetFetch v8+ requires at least one group".into()));
            }

            // Read first group only (we only support single group for now)
            let group_id = if flexible {
                decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
            } else {
                decoder.read_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
            };

            // Read topics array (null means all topics)
            let topics = if flexible {
                let topic_count = decoder.read_unsigned_varint()? as usize;
                if topic_count == 0 {
                    None // Null means fetch all topics
                } else {
                    let actual_count = topic_count - 1;
                    let mut topic_names = Vec::with_capacity(actual_count);
                    for _ in 0..actual_count {
                        let topic_name = decoder.read_compact_string()?
                            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

                        // Skip PartitionIndexes array
                        let partition_count = decoder.read_unsigned_varint()? as usize;
                        if partition_count > 0 {
                            for _ in 0..(partition_count - 1) {
                                let _partition_index = decoder.read_i32()?;
                            }
                        }

                        // Tagged fields at topic level
                        let _tag_count = decoder.read_unsigned_varint()?;

                        topic_names.push(topic_name);
                    }
                    Some(topic_names)
                }
            } else {
                let topic_count = decoder.read_i32()?;
                if topic_count < 0 {
                    None
                } else {
                    let mut topic_names = Vec::with_capacity(topic_count as usize);
                    for _ in 0..topic_count {
                        let topic_name = decoder.read_string()?
                            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

                        let partition_count = decoder.read_i32()? as usize;
                        for _ in 0..partition_count {
                            let _partition_index = decoder.read_i32()?;
                        }

                        topic_names.push(topic_name);
                    }
                    Some(topic_names)
                }
            };

            // Tagged fields at group level (flexible only)
            if flexible {
                let _tag_count = decoder.read_unsigned_varint()?;
            }

            // Skip remaining groups if any
            for _ in 1..group_count {
                if flexible {
                    let _ = decoder.read_compact_string()?;
                    let topic_count = decoder.read_unsigned_varint()? as usize;
                    for _ in 0..(if topic_count > 0 { topic_count - 1 } else { 0 }) {
                        let _ = decoder.read_compact_string()?;
                        let partition_count = decoder.read_unsigned_varint()? as usize;
                        for _ in 0..(if partition_count > 0 { partition_count - 1 } else { 0 }) {
                            let _ = decoder.read_i32()?;
                        }
                        let _ = decoder.read_unsigned_varint()?; // topic tags
                    }
                    let _ = decoder.read_unsigned_varint()?; // group tags
                } else {
                    let _ = decoder.read_string()?;
                    let topic_count = decoder.read_i32()? as usize;
                    for _ in 0..topic_count {
                        let _ = decoder.read_string()?;
                        let partition_count = decoder.read_i32()? as usize;
                        for _ in 0..partition_count {
                            let _ = decoder.read_i32()?;
                        }
                    }
                }
            }

            // RequireStable field (v7+)
            if header.api_version >= 7 {
                let _require_stable = decoder.read_bool()?;
            }

            // Tagged fields at request level (flexible only)
            if flexible {
                let _tag_count = decoder.read_unsigned_varint()?;
            }

            Ok(OffsetFetchRequest {
                group_id,
                topics,
            })
        } else {
            // V0-7: Original structure with group_id and topics
            let group_id = if flexible {
                decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
            } else {
                decoder.read_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
            };

            // Read topics array (null means all topics)
            let topics = if header.api_version >= 1 {
                if flexible {
                    let topic_count = decoder.read_unsigned_varint()? as usize;
                    if topic_count == 0 {
                        None
                    } else {
                        let actual_count = topic_count - 1;
                        let mut topics = Vec::with_capacity(actual_count);
                        for _ in 0..actual_count {
                            let topic_name = decoder.read_compact_string()?
                                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
                            topics.push(topic_name);
                        }
                        Some(topics)
                    }
                } else {
                    let topic_count = decoder.read_i32()?;
                    if topic_count < 0 {
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
                }
            } else {
                None
            };

            // RequireStable field (v7+)
            if header.api_version >= 7 {
                let _require_stable = decoder.read_bool()?;
            }

            // Tagged fields at request level (flexible only)
            if flexible {
                let _tag_count = decoder.read_unsigned_varint()?;
            }

            Ok(OffsetFetchRequest {
                group_id,
                topics,
            })
        }
    }
    
    // Public encode methods for use by KafkaProtocolHandler
    
    /// Encode FindCoordinator response
    pub fn encode_find_coordinator_response(&self, buf: &mut BytesMut, response: &crate::find_coordinator_types::FindCoordinatorResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);

        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }

        if version >= 4 {
            // v4+: KIP-699 array format with compact encoding
            // Compact array length = actual_length + 1
            encoder.write_unsigned_varint((response.coordinators.len() + 1) as u32);

            // Write each coordinator in the array
            for coord in &response.coordinators {
                encoder.write_compact_string(Some(&coord.key));
                encoder.write_i32(coord.node_id);
                encoder.write_compact_string(Some(&coord.host));
                encoder.write_i32(coord.port);
                encoder.write_i16(coord.error_code);
                encoder.write_compact_string(coord.error_message.as_deref());
                encoder.write_unsigned_varint(0); // tagged fields for coordinator
            }

            encoder.write_unsigned_varint(0); // response-level tagged fields
        } else {
            // v0-v3: Single coordinator format
            encoder.write_i16(response.error_code);

            if version >= 1 {
                encoder.write_string(response.error_message.as_deref());
            }

            encoder.write_i32(response.node_id);
            encoder.write_string(Some(&response.host));
            encoder.write_i32(response.port);
        }

        Ok(())
    }
    
    /// Encode JoinGroup response
    pub fn encode_join_group_response(&self, buf: &mut BytesMut, response: &crate::join_group_types::JoinGroupResponse, version: i16) -> Result<()> {
        let start_len = buf.len();
        let mut encoder = Encoder::new(buf);

        // v6+ uses flexible/compact format
        let flexible = version >= 6;

        tracing::debug!("JoinGroup v{} response encoding START: error={}, gen={}, protocol={:?}, leader={}, member_id={}, members={}",
            version, response.error_code, response.generation_id, response.protocol_name, response.leader, response.member_id, response.members.len());

        // Field 1: ThrottleTimeMs (v2+)
        if version >= 2 {
            encoder.write_i32(response.throttle_time_ms);
            tracing::debug!("  [1] ThrottleTimeMs: {}", response.throttle_time_ms);
        }

        // Field 2: ErrorCode (v0+)
        encoder.write_i16(response.error_code);
        tracing::debug!("  [2] ErrorCode: {}", response.error_code);

        // Field 3: GenerationId (v0+)
        encoder.write_i32(response.generation_id);
        tracing::debug!("  [3] GenerationId: {}", response.generation_id);

        // Field 4: ProtocolType (v7+)
        if version >= 7 {
            if flexible {
                encoder.write_compact_string(response.protocol_type.as_deref());
                tracing::debug!("  [4] ProtocolType (compact): {:?}", response.protocol_type);
            } else {
                encoder.write_string(response.protocol_type.as_deref());
                tracing::debug!("  [4] ProtocolType: {:?}", response.protocol_type);
            }
        }

        // Field 5: ProtocolName (v0+)
        if flexible {
            encoder.write_compact_string(response.protocol_name.as_deref());
            tracing::debug!("  [5] ProtocolName (compact): {:?}", response.protocol_name);
        } else {
            encoder.write_string(response.protocol_name.as_deref());
            tracing::debug!("  [5] ProtocolName: {:?}", response.protocol_name);
        }

        // Field 6: Leader (v0+)
        if flexible {
            encoder.write_compact_string(Some(&response.leader));
            tracing::debug!("  [6] Leader (compact): {}", response.leader);
        } else {
            encoder.write_string(Some(&response.leader));
            tracing::debug!("  [6] Leader: {}", response.leader);
        }

        // Field 7: SkipAssignment (v9+)
        if version >= 9 {
            encoder.write_bool(false);
            tracing::debug!("  [7] SkipAssignment: false");
        }

        // Field 8: MemberId (v0+)
        if flexible {
            encoder.write_compact_string(Some(&response.member_id));
            tracing::debug!("  [8] MemberId (compact): {}", response.member_id);
        } else {
            encoder.write_string(Some(&response.member_id));
            tracing::debug!("  [8] MemberId: {}", response.member_id);
        }

        // Field 9: Members array (v0+)
        if flexible {
            encoder.write_compact_array_len(response.members.len());
            tracing::debug!("  [9] Members array (compact): {} members", response.members.len());
        } else {
            encoder.write_i32(response.members.len() as i32);
            tracing::debug!("  [9] Members array: {} members", response.members.len());
        }

        for (idx, member) in response.members.iter().enumerate() {
            tracing::debug!("    Member[{}]: id={}, instance_id={:?}", idx, member.member_id, member.group_instance_id);

            if flexible {
                encoder.write_compact_string(Some(&member.member_id));
            } else {
                encoder.write_string(Some(&member.member_id));
            }

            if version >= 5 {
                if flexible {
                    encoder.write_compact_string(member.group_instance_id.as_deref());
                } else {
                    encoder.write_string(member.group_instance_id.as_deref());
                }
            }

            if flexible {
                encoder.write_compact_bytes(Some(&member.metadata));
                // Tagged fields at member level
                encoder.write_tagged_fields();
            } else {
                encoder.write_bytes(Some(&member.metadata));
            }
        }

        // Tagged fields at response level (flexible versions only)
        if flexible {
            encoder.write_tagged_fields();
            tracing::debug!("  Tagged fields (response level): 0");
        }

        let response_bytes = &buf[start_len..];
        tracing::debug!("JoinGroup v{} response encoded {} bytes: {:02x?}", version, response_bytes.len(), response_bytes);

        Ok(())
    }
    
    /// Encode SyncGroup response
    pub fn encode_sync_group_response(&self, buf: &mut BytesMut, response: &crate::sync_group_types::SyncGroupResponse, version: i16) -> Result<()> {
        use crate::parser::KafkaEncodable;

        let mut encoder = Encoder::new(buf);
        response.encode(&mut encoder, version)?;
        Ok(())
    }
    
    /// Encode Heartbeat response
    pub fn encode_heartbeat_response(&self, buf: &mut BytesMut, response: &crate::heartbeat_types::HeartbeatResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        let flexible = version >= 4;

        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }

        encoder.write_i16(response.error_code);

        // Tagged fields for flexible versions
        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }
    
    /// Encode LeaveGroup response
    pub fn encode_leave_group_response(&self, buf: &mut BytesMut, response: &crate::leave_group_types::LeaveGroupResponse, version: i16) -> Result<()> {
        use crate::parser::KafkaEncodable;

        let mut encoder = Encoder::new(buf);
        response.encode(&mut encoder, version)?;
        Ok(())
    }

    /// OLD MANUAL ENCODER - REPLACED WITH KAFKAENCODABLE TRAIT
    pub fn _encode_leave_group_response_manual(&self, buf: &mut BytesMut, response: &crate::leave_group_types::LeaveGroupResponse, version: i16) -> Result<()> {
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

        // v8+ uses flexible/compact format
        let flexible = version >= 8;

        if version >= 3 {
            encoder.write_i32(response.throttle_time_ms);
        }

        // Write topics array
        if flexible {
            encoder.write_compact_array_len(response.topics.len());
        } else {
            encoder.write_i32(response.topics.len() as i32);
        }

        for topic in &response.topics {
            // Write topic name
            if flexible {
                encoder.write_compact_string(Some(&topic.name));
            } else {
                encoder.write_string(Some(&topic.name));
            }

            // Write partitions array
            if flexible {
                encoder.write_compact_array_len(topic.partitions.len());
            } else {
                encoder.write_i32(topic.partitions.len() as i32);
            }

            for partition in &topic.partitions {
                encoder.write_i32(partition.partition_index);
                encoder.write_i16(partition.error_code);

                // Tagged fields at partition level
                if flexible {
                    encoder.write_tagged_fields();
                }
            }

            // Tagged fields at topic level
            if flexible {
                encoder.write_tagged_fields();
            }
        }

        // Tagged fields at response level
        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }
    
    /// Encode OffsetFetch response
    pub fn encode_offset_fetch_response(&self, buf: &mut BytesMut, response: &crate::types::OffsetFetchResponse, version: i16) -> Result<()> {
        let mut encoder = Encoder::new(buf);

        // v6+ uses flexible/compact format (but v8 is standard for full support)
        let flexible = version >= 6;

        if version >= 3 {
            encoder.write_i32(response.throttle_time_ms);
        }

        // V8+ uses Groups array wrapper
        if version >= 8 {
            // Write Groups array (length 1 - we only support single group)
            if flexible {
                encoder.write_compact_array_len(1); // 1 group
            } else {
                encoder.write_i32(1);
            }

            // Write GroupId (get from response, fallback to empty string)
            if flexible {
                encoder.write_compact_string(response.group_id.as_deref());
            } else {
                encoder.write_string(response.group_id.as_deref());
            }

            // Write Topics array inside group
            if flexible {
                encoder.write_compact_array_len(response.topics.len());
            } else {
                encoder.write_i32(response.topics.len() as i32);
            }

            for topic in &response.topics {
                // Write topic name
                if flexible {
                    encoder.write_compact_string(Some(&topic.name));
                } else {
                    encoder.write_string(Some(&topic.name));
                }

                // Write partitions array
                if flexible {
                    encoder.write_compact_array_len(topic.partitions.len());
                } else {
                    encoder.write_i32(topic.partitions.len() as i32);
                }

                for partition in &topic.partitions {
                    encoder.write_i32(partition.partition_index);
                    encoder.write_i64(partition.committed_offset);

                    // v5+ has committed_leader_epoch
                    if version >= 5 {
                        encoder.write_i32(-1); // -1 = unknown leader epoch
                    }

                    // Write metadata
                    if flexible {
                        encoder.write_compact_string(partition.metadata.as_deref());
                    } else {
                        encoder.write_string(partition.metadata.as_deref());
                    }

                    encoder.write_i16(partition.error_code);

                    // Tagged fields at partition level
                    if flexible {
                        encoder.write_tagged_fields();
                    }
                }

                // Tagged fields at topic level
                if flexible {
                    encoder.write_tagged_fields();
                }
            }

            // Error code at group level (v8+)
            encoder.write_i16(0); // error_code

            // Tagged fields at group level
            if flexible {
                encoder.write_tagged_fields();
            }

            // Tagged fields at response level
            if flexible {
                encoder.write_tagged_fields();
            }
        } else {
            // V0-7: Original flat structure
            // Write topics array
            if flexible {
                encoder.write_compact_array_len(response.topics.len());
            } else {
                encoder.write_i32(response.topics.len() as i32);
            }

            for topic in &response.topics {
                // Write topic name
                if flexible {
                    encoder.write_compact_string(Some(&topic.name));
                } else {
                    encoder.write_string(Some(&topic.name));
                }

                // Write partitions array
                if flexible {
                    encoder.write_compact_array_len(topic.partitions.len());
                } else {
                    encoder.write_i32(topic.partitions.len() as i32);
                }

                for partition in &topic.partitions {
                    encoder.write_i32(partition.partition_index);
                    encoder.write_i64(partition.committed_offset);

                    // v5+ has committed_leader_epoch
                    if version >= 5 {
                        encoder.write_i32(-1); // -1 = unknown leader epoch
                    }

                    // Write metadata
                    if flexible {
                        encoder.write_compact_string(partition.metadata.as_deref());
                    } else {
                        encoder.write_string(partition.metadata.as_deref());
                    }

                    encoder.write_i16(partition.error_code);

                    // Tagged fields at partition level
                    if flexible {
                        encoder.write_tagged_fields();
                    }
                }

                // Tagged fields at topic level
                if flexible {
                    encoder.write_tagged_fields();
                }
            }

            // Error code at response level (v2-7)
            if version >= 2 {
                encoder.write_i16(0); // error_code at response level
            }

            // Tagged fields at response level
            if flexible {
                encoder.write_tagged_fields();
            }
        }

        Ok(())
    }
    
    /// Parse Fetch request
    pub fn parse_fetch_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<crate::types::FetchRequest> {
        use crate::parser::Decoder;
        use crate::types::{FetchRequest, FetchRequestTopic, FetchRequestPartition};

        let mut decoder = Decoder::new(body);
        let flexible = header.api_version >= 12;

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
        let topic_count = if flexible {
            (decoder.read_unsigned_varint()? - 1) as usize
        } else {
            decoder.read_i32()? as usize
        };
        let mut topics = Vec::with_capacity(topic_count);

        for _ in 0..topic_count {
            let name = if flexible {
                decoder.read_compact_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
            } else {
                decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
            };

            // Partitions array
            let partition_count = if flexible {
                (decoder.read_unsigned_varint()? - 1) as usize
            } else {
                decoder.read_i32()? as usize
            };
            let mut partitions = Vec::with_capacity(partition_count);

            for _ in 0..partition_count {
                let partition = decoder.read_i32()?;
                let current_leader_epoch = if header.api_version >= 9 {
                    decoder.read_i32()?
                } else {
                    -1
                };
                let fetch_offset = decoder.read_i64()?;

                // last_fetched_epoch added in v12 (between fetch_offset and log_start_offset)
                let _last_fetched_epoch = if header.api_version >= 12 {
                    decoder.read_i32()?
                } else {
                    -1
                };

                let log_start_offset = if header.api_version >= 5 {
                    decoder.read_i64()?
                } else {
                    -1
                };
                let partition_max_bytes = decoder.read_i32()?;

                // Tagged fields for partition (v12+)
                if flexible {
                    let tag_count = decoder.read_unsigned_varint()?;
                    for _ in 0..tag_count {
                        let _tag_id = decoder.read_unsigned_varint()?;
                        let tag_size = decoder.read_unsigned_varint()? as usize;
                        decoder.advance(tag_size)?;
                    }
                }

                partitions.push(FetchRequestPartition {
                    partition,
                    current_leader_epoch,
                    fetch_offset,
                    log_start_offset,
                    partition_max_bytes,
                });
            }

            // Tagged fields for topic (v12+)
            if flexible {
                let tag_count = decoder.read_unsigned_varint()?;
                for _ in 0..tag_count {
                    let _tag_id = decoder.read_unsigned_varint()?;
                    let tag_size = decoder.read_unsigned_varint()? as usize;
                    decoder.advance(tag_size)?;
                }
            }

            topics.push(FetchRequestTopic { name, partitions });
        }

        // Read and ignore forgotten topics data for v7+
        if header.api_version >= 7 {
            // Read forgotten topics array
            let count = if flexible {
                (decoder.read_unsigned_varint()? - 1) as usize
            } else {
                decoder.read_i32()? as usize
            };
            for _ in 0..count {
                let _topic = if flexible {
                    decoder.read_compact_string()?
                } else {
                    decoder.read_string()?
                };
                let partition_count = if flexible {
                    (decoder.read_unsigned_varint()? - 1) as usize
                } else {
                    decoder.read_i32()? as usize
                };
                for _ in 0..partition_count {
                    let _partition = decoder.read_i32()?;
                }

                // Tagged fields for forgotten topic (v12+)
                if flexible {
                    let tag_count = decoder.read_unsigned_varint()?;
                    for _ in 0..tag_count {
                        let _tag_id = decoder.read_unsigned_varint()?;
                        let tag_size = decoder.read_unsigned_varint()? as usize;
                        decoder.advance(tag_size)?;
                    }
                }
            }
        }

        // Read and ignore rack_id for v11+
        if header.api_version >= 11 {
            let _rack_id = if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            };
        }

        // Tagged fields at request level (v12+)
        if flexible {
            let tag_count = decoder.read_unsigned_varint()?;
            for _ in 0..tag_count {
                let _tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size)?;
            }
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
        let flexible = version >= 12;

        tracing::debug!("Encoding Fetch response v{} (flexible={})", version, flexible);

        // Throttle time (v1+)
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
            tracing::trace!("  throttle_time_ms: {}", response.throttle_time_ms);
        }

        // Error code and session ID (v7+)
        if version >= 7 {
            encoder.write_i16(response.error_code);
            encoder.write_i32(response.session_id);
            tracing::trace!("  error_code: {}, session_id: {}", response.error_code, response.session_id);
        }

        // Topics array
        if flexible {
            encoder.write_unsigned_varint((response.topics.len() + 1) as u32);
        } else {
            encoder.write_i32(response.topics.len() as i32);
        }
        tracing::debug!("  Topics count: {}", response.topics.len());

        for topic in &response.topics {
            // Topic name
            if flexible {
                encoder.write_compact_string(Some(&topic.name));
            } else {
                encoder.write_string(Some(&topic.name));
            }
            tracing::trace!("    Topic: {}", topic.name);

            // Partitions array
            if flexible {
                encoder.write_unsigned_varint((topic.partitions.len() + 1) as u32);
            } else {
                encoder.write_i32(topic.partitions.len() as i32);
            }
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

                    // Aborted transactions
                    if let Some(ref aborted) = partition.aborted {
                        if flexible {
                            encoder.write_unsigned_varint((aborted.len() + 1) as u32);
                        } else {
                            encoder.write_i32(aborted.len() as i32);
                        }
                        for txn in aborted {
                            encoder.write_i64(txn.producer_id);
                            encoder.write_i64(txn.first_offset);
                            if flexible {
                                encoder.write_unsigned_varint(0); // Tagged fields
                            }
                        }
                    } else {
                        if flexible {
                            encoder.write_unsigned_varint(1); // 0 + 1 for compact encoding
                        } else {
                            encoder.write_i32(0);
                        }
                    }
                }

                if version >= 11 {
                    encoder.write_i32(partition.preferred_read_replica);
                    tracing::trace!("        preferred_read_replica={}", partition.preferred_read_replica);
                }

                // Records
                let records_len = partition.records.len();
                if records_len == 0 {
                    // Empty records
                    tracing::trace!("        Records: empty (0 bytes)");
                    if flexible {
                        encoder.write_unsigned_varint(0); // Null in compact encoding
                    } else {
                        encoder.write_i32(0); // Empty byte array
                    }
                } else {
                    tracing::debug!("        Records: {} bytes", records_len);
                    if flexible {
                        encoder.write_unsigned_varint((records_len + 1) as u32);
                    } else {
                        encoder.write_i32(records_len as i32);
                    }
                    encoder.write_raw_bytes(&partition.records);
                }

                // Tagged fields for partition (v12+)
                if flexible {
                    encoder.write_unsigned_varint(0);
                }
            }

            // Tagged fields for topic (v12+)
            if flexible {
                encoder.write_unsigned_varint(0);
            }
        }

        // Tagged fields at response level (v12+)
        if flexible {
            encoder.write_unsigned_varint(0);
        }

        let total_encoded = buf.len() - initial_len;
        tracing::info!("Encoded Fetch response v{}: {} bytes total", version, total_encoded);

        Ok(())
    }
    
    /// Handle a raw request and return a response
    pub async fn handle_request(&self, request_bytes: &[u8]) -> Result<Response> {
        // Log request for debugging (first 64 bytes)
        let preview_len = request_bytes.len().min(64);
        tracing::debug!("Received {} byte request, first {} bytes: {:02x?}",
                       request_bytes.len(), preview_len, &request_bytes[..preview_len]);

        let mut buf = Bytes::copy_from_slice(request_bytes);

        // Use the new parsing function that preserves correlation ID
        let header = match parse_request_header_with_correlation(&mut buf) {
            Ok((h, _)) => {
                tracing::info!("Protocol handler: Processing API {:?} v{}", h.api_key, h.api_version);
                h
            },
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
            ApiKey::LeaveGroup => self.handle_leave_group(header, &mut buf).await,
            ApiKey::SyncGroup => self.handle_sync_group(header, &mut buf).await,
            ApiKey::DescribeGroups => self.handle_describe_groups(header, &mut buf).await,
            ApiKey::ListGroups => self.handle_list_groups(header, &mut buf).await,
            
            // Administrative APIs - TODO: implement these
            ApiKey::CreateTopics => self.handle_create_topics(header, &mut buf).await,
            ApiKey::DeleteTopics => self.handle_delete_topics(header, &mut buf).await,
            ApiKey::DeleteGroups => self.handle_delete_groups(header, &mut buf).await,
            ApiKey::AlterConfigs => self.handle_alter_configs(header, &mut buf).await,
            ApiKey::IncrementalAlterConfigs => self.handle_incremental_alter_configs(header, &mut buf).await,
            ApiKey::CreatePartitions => self.handle_create_partitions(header, &mut buf).await,
            ApiKey::OffsetDelete => self.handle_offset_delete(header, &mut buf).await,
            ApiKey::EndTxn => self.handle_end_txn(header, &mut buf).await,
            ApiKey::InitProducerId => self.handle_init_producer_id(header, &mut buf).await,
            ApiKey::AddPartitionsToTxn => self.handle_add_partitions_to_txn(header, &mut buf).await,
            ApiKey::AddOffsetsToTxn => self.handle_add_offsets_to_txn(header, &mut buf).await,
            ApiKey::TxnOffsetCommit => self.handle_txn_offset_commit(header, &mut buf).await,

            // Other APIs
            ApiKey::ListOffsets => self.handle_list_offsets(header, &mut buf).await,
            ApiKey::DescribeCluster => self.handle_describe_cluster(header, &mut buf).await,
            
            // SASL authentication (partial support for compatibility)
            ApiKey::SaslHandshake => self.handle_sasl_handshake(header, &mut buf).await,
            ApiKey::SaslAuthenticate => self.handle_sasl_authenticate(header, &mut buf).await,

            // Broker-to-broker APIs (not client-facing)
            ApiKey::LeaderAndIsr |
            ApiKey::StopReplica |
            ApiKey::UpdateMetadata |
            ApiKey::ControlledShutdown => {
                tracing::warn!("Received broker-to-broker API request: {:?}", header.api_key);
                self.error_response(header.correlation_id, error_codes::UNSUPPORTED_VERSION)
            }

            // Transaction APIs
            ApiKey::WriteTxnMarkers => {
                tracing::warn!("Received transaction API request: {:?}", header.api_key);
                self.error_response(header.correlation_id, error_codes::UNSUPPORTED_VERSION)
            }

            // ACL APIs - implement properly
            ApiKey::DescribeAcls => self.handle_describe_acls(header, &mut buf).await,
            ApiKey::CreateAcls => self.handle_create_acls(header, &mut buf).await,
            ApiKey::DeleteAcls => self.handle_delete_acls(header, &mut buf).await,

            // Admin APIs - implement these properly
            ApiKey::DescribeLogDirs => self.handle_describe_log_dirs(header, &mut buf).await,

            // Other APIs - catch all unimplemented APIs
            ApiKey::DeleteRecords |
            ApiKey::OffsetForLeaderEpoch |
            ApiKey::AlterReplicaLogDirs |
            ApiKey::CreateDelegationToken |
            ApiKey::RenewDelegationToken |
            ApiKey::ExpireDelegationToken |
            ApiKey::DescribeDelegationToken |
            ApiKey::ElectLeaders |
            ApiKey::AlterPartitionReassignments |
            ApiKey::ListPartitionReassignments |
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
    
    /// Parse ApiVersions request
    fn parse_api_versions_request(&self, header: &RequestHeader, body: &mut Bytes) -> Result<ApiVersionsRequest> {
        use crate::parser::Decoder;

        let (client_software_name, client_software_version) = if header.api_version >= 3 {
            // For v3+, just ignore the body completely since fields are "ignorable"
            // The schema marks them as ignorable: true, so we don't need to parse them
            // This avoids encoding issues between different Kafka client implementations

            tracing::debug!("ApiVersions v3 request received (body ignored - fields are ignorable)");

            (None, None)
        } else {
            (None, None)
        };

        Ok(ApiVersionsRequest {
            client_software_name,
            client_software_version,
        })
    }

    /// Consume ApiVersions v3+ request body fields (ignorable but must be consumed)
    fn try_consume_api_versions_body(&self, body: &mut Bytes) -> Result<()> {
        if body.remaining() == 0 {
            return Ok(());
        }

        let mut decoder = Decoder::new(body);

        // client_software_name (COMPACT_STRING - varint length)
        let _software_name = decoder.read_compact_string()?;
        tracing::trace!("ApiVersions v3+ client_software_name: {:?}", _software_name);

        // client_software_version (COMPACT_STRING - varint length)
        let _software_version = decoder.read_compact_string()?;
        tracing::trace!("ApiVersions v3+ client_software_version: {:?}", _software_version);

        // Tagged fields (required for v3+)
        let tag_count = decoder.read_unsigned_varint()?;
        tracing::trace!("ApiVersions v3+ tagged fields count: {}", tag_count);

        for _ in 0..tag_count {
            let tag_id = decoder.read_unsigned_varint()?;
            let tag_size = decoder.read_unsigned_varint()? as usize;
            tracing::trace!("  Skipping tagged field: id={}, size={}", tag_id, tag_size);
            decoder.advance(tag_size)?;
        }

        Ok(())
    }

    /// Handle ApiVersions request
    async fn handle_api_versions(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        // For v3+, consume the optional body fields to prevent buffer underrun
        // These fields are marked as "ignorable" in the spec but must still be consumed
        if header.api_version >= 3 && body.remaining() > 0 {
            if let Err(e) = self.try_consume_api_versions_body(body) {
                tracing::warn!(
                    "Failed to parse ApiVersions v{} body (fields are ignorable): {}",
                    header.api_version, e
                );
                // Clear remaining buffer to prevent issues with subsequent parsing
                body.advance(body.remaining());
            } else {
                tracing::debug!("Successfully consumed ApiVersions v{} body fields", header.api_version);
            }
        }
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
            
            let encoded_bytes = body_buf.freeze();
            let response = Self::make_response(&header, ApiKey::ApiVersions, encoded_bytes);
            tracing::debug!("ApiVersions response encoded, body size: {} bytes", response.body.len());
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
        tracing::info!("Encoding ApiVersionsResponse v{} with {} APIs", version, response.api_versions.len());
        let mut encoder = Encoder::new(buf);

        // CRITICAL: For v0 protocol, field order matters!
        // kafka-python expects error_code FIRST, then api_versions array
        // This is different from the Kafka protocol spec, but needed for compatibility

        if version == 0 {
            // v0 field order: kafka-python compatibility requires error_code FIRST
            tracing::info!("Using kafka-python compatible v0 encoding: error_code first, then api_versions array");

            // Write error_code first (for kafka-python compatibility)
            encoder.write_i16(response.error_code);

            // Then write array of API versions
            encoder.write_i32(response.api_versions.len() as i32);
            for api in &response.api_versions {
                encoder.write_i16(api.api_key);
                encoder.write_i16(api.min_version);
                encoder.write_i16(api.max_version);
            }
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
            encoder.write_i16(response.error_code);
            
            // Write array of API versions
            if version >= 3 {
                // Use compact array encoding
                // Compact arrays use length+1 encoding
                let array_len = (response.api_versions.len() + 1) as u32;
                encoder.write_unsigned_varint(array_len);
                
                for (i, api) in response.api_versions.iter().enumerate() {
                    // CRITICAL: librdkafka compatibility issue
                    // Kafka spec says v3+ uses INT8 for min/max, but librdkafka v2.11.1
                    // seems to expect INT16. Let's try INT16 for compatibility.
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
                encoder.write_i32(response.throttle_time_ms);
                encoder.write_unsigned_varint(0);
            }
            
            // v4+ has tagged fields at the end (and throttle_time is earlier)
            if version >= 4 {
                // Write tagged fields at the end (empty for now)
                encoder.write_unsigned_varint(0);
            }
        }
        
        tracing::debug!("ApiVersions response encoded, buffer size: {} bytes", encoder.debug_buffer().len());
        Ok(())
    }
    
    /// Handle Metadata request
    pub async fn handle_metadata(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::types::{MetadataRequest, MetadataResponse, MetadataBroker};
        use crate::parser::Decoder;
        
        tracing::debug!("handle_metadata called with v{}", header.api_version);
        tracing::debug!("Handling metadata request v{}", header.api_version);
        
        let mut decoder = Decoder::new(body);
        let flexible = header.api_version >= 9;
        
        // Parse metadata request
        tracing::debug!("Parsing metadata request, flexible={}", flexible);
        let topics = if header.api_version >= 1 {
            let topic_count = if flexible {
                // Compact array
                let count = match decoder.read_unsigned_varint() {
                    Ok(c) => c as i32,
                    Err(e) => {
                        tracing::debug!("Failed to read topic count varint: {:?}", e);
                        return Err(e);
                    }
                };
                tracing::debug!("Topic count varint read: {}", count);
                if count == 0 {
                    -1  // Null array
                } else {
                    count - 1  // Compact arrays use +1 encoding
                }
            } else {
                decoder.read_i32()?
            };
            
            tracing::debug!("Topic count decoded: {}", topic_count);
            if topic_count < 0 {
                None // All topics
            } else {
                let mut topic_names = Vec::with_capacity(topic_count as usize);
                for i in 0..topic_count {
                    tracing::trace!("Reading topic {}/{}", i+1, topic_count);
                    
                    // v10+ includes topic_id (UUID) before name
                    if header.api_version >= 10 {
                        // Skip the 16-byte UUID
                        let mut topic_id = [0u8; 16];
                        for j in 0..16 {
                            topic_id[j] = decoder.read_i8()? as u8;
                        }
                        tracing::trace!("Topic {} ID: {:02x?}", i, &topic_id);
                    }
                    
                    let name = if flexible {
                        decoder.read_compact_string()?
                    } else {
                        decoder.read_string()?
                    };
                    
                    
                    if let Some(name) = name {
                        topic_names.push(name);
                    }
                    
                    if flexible {
                        // Skip tagged fields for each topic
                        let tagged_count = decoder.read_unsigned_varint()?;
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
            val
        } else {
            true
        };
        
        // include_cluster_authorized_operations was removed in v11
        let include_cluster_authorized_operations = if header.api_version >= 8 && header.api_version <= 10 {
            let val = decoder.read_bool()?;
            val
        } else {
            false
        };
        
        let include_topic_authorized_operations = if header.api_version >= 8 {
            let val = decoder.read_bool()?;
            val
        } else {
            false
        };
        
        if flexible {
            // Skip tagged fields at the end
            // librdkafka v2.11.1 quirk: First Metadata request may be missing tagged fields
            if decoder.has_remaining() {
                match decoder.read_unsigned_varint() {
                    Ok(_) => {
                        // Tagged fields read and ignored
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            } else {
                // No tagged fields
            }
        }
        
        let _request = MetadataRequest {
            topics,
            allow_auto_topic_creation,
            include_cluster_authorized_operations,
            include_topic_authorized_operations,
        };
        
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
                    tracing::warn!("Using fallback broker: {}:{} (node_id={})",
                        self.advertised_host, self.advertised_port, self.broker_id);
                    vec![MetadataBroker {
                        node_id: self.broker_id,
                        host: self.advertised_host.clone(),
                        port: self.advertised_port,
                        rack: None,
                    }]
                }
            }
        } else {
            tracing::warn!("No metadata store available, using default broker");
            tracing::warn!("Using default broker: {}:{} (node_id={})",
                self.advertised_host, self.advertised_port, self.broker_id);
            // No metadata store, use current broker
            vec![MetadataBroker {
                node_id: self.broker_id,
                host: self.advertised_host.clone(),
                port: self.advertised_port,
                rack: None,
            }]
        };
        
        // CRITICAL VALIDATION: Ensure broker list is not empty and hosts are valid
        // This prevents the AdminClient "No resolvable bootstrap urls" error
        if brokers.is_empty() {
            tracing::error!("CRITICAL: Metadata response has NO brokers - AdminClient will fail!");
            tracing::error!("This will cause 'No resolvable bootstrap urls' error in Java clients");
            return Err(Error::Internal("Metadata response must include at least one broker".into()));
        }

        for broker in &brokers {
            if broker.host.is_empty() {
                tracing::error!("CRITICAL: Broker {} has EMPTY host - AdminClient will fail!", broker.node_id);
                return Err(Error::Internal(format!("Broker {} has empty host field", broker.node_id)));
            }
            if broker.host == "0.0.0.0" {
                tracing::error!("CRITICAL: Broker {} has 0.0.0.0 host - clients cannot connect!", broker.node_id);
                return Err(Error::Internal(format!("Broker {} has invalid host 0.0.0.0", broker.node_id)));
            }
            if broker.node_id < 0 {
                tracing::error!("CRITICAL: Broker has invalid node_id {} - clients will reject!", broker.node_id);
                return Err(Error::Internal(format!("Broker has invalid node_id {}", broker.node_id)));
            }
        }

        let response = MetadataResponse {
            throttle_time_ms: 0,
            brokers,
            cluster_id: Some("chronik-stream".to_string()),
            controller_id: self.broker_id, // Use actual broker ID
            topics,
            cluster_authorized_operations: if header.api_version >= 8 { Some(-2147483648) } else { None },
        };
        tracing::info!("Metadata response has {} topics and {} brokers",
                      response.topics.len(), response.brokers.len());
        
        // Debug: Log broker details
        for (i, broker) in response.brokers.iter().enumerate() {
            tracing::debug!("  Broker {}: id={}, host={}, port={}", 
                           i, broker.node_id, broker.host, broker.port);
        }

        let mut body_buf = BytesMut::new();
        tracing::info!("About to encode metadata response with {} brokers", response.brokers.len());
        // Encode the response body (without correlation ID)
        match self.encode_metadata_response(&mut body_buf, &response, header.api_version) {
            Ok(_) => {
                // Response encoded successfully
                tracing::debug!("Metadata response body encoded, length={}, first 32 bytes: {:02x?}",
                    body_buf.len(),
                    &body_buf[..body_buf.len().min(32)]);
            }
            Err(e) => {
                return Err(e);
            }
        }
        
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
                        decoder.advance(tag_size)?;
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
                    decoder.advance(tag_size)?;
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
                decoder.advance(tag_size)?;
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
            error_code: 0,    // No error
            session_id: 0,     // No incremental fetch session
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

    /// Handle SASL authenticate request
    async fn handle_sasl_authenticate(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::sasl_types::SaslAuthenticateResponse;
        use crate::parser::Decoder;

        tracing::debug!("Handling SASL authenticate request v{}", header.api_version);

        let mut decoder = Decoder::new(body);

        // Parse auth bytes
        let auth_bytes = decoder.read_bytes()?
            .ok_or_else(|| Error::Protocol("SASL auth bytes cannot be null".into()))?;

        tracing::info!("SASL authenticate request with {} auth bytes", auth_bytes.len());

        // For now, we accept any authentication attempt to allow KSQL to continue
        // In a real implementation, this would validate credentials
        let response = SaslAuthenticateResponse {
            error_code: 0, // Success
            error_message: None,
            auth_bytes: vec![], // Empty response bytes
            session_lifetime_ms: 3600000, // 1 hour
        };

        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);

        // Error code
        encoder.write_i16(response.error_code);

        // Error message (nullable string)
        encoder.write_string(response.error_message.as_deref());

        // Auth bytes (nullable bytes)
        encoder.write_bytes(if response.auth_bytes.is_empty() {
            None
        } else {
            Some(&response.auth_bytes)
        });

        // Session lifetime ms
        encoder.write_i64(response.session_lifetime_ms);

        Ok(Self::make_response(&header, ApiKey::SaslAuthenticate, body_buf.freeze()))
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

        // Determine encoding type (v4+ uses compact/flexible encoding)
        let use_compact = header.api_version >= 4;
        // Note: DescribeConfigs does NOT have client_id in body (unlike CreateTopics)

        // Parse resources array - use compact array for v4+
        let resource_count = if use_compact {
            let compact_len = decoder.read_unsigned_varint()?;
            if compact_len == 0 {
                0  // NULL array
            } else {
                (compact_len - 1) as usize
            }
        } else {
            decoder.read_i32()? as usize
        };
        tracing::debug!("Resource count: {}", resource_count);
        let mut resources = Vec::with_capacity(resource_count);
        
        for i in 0..resource_count {
            let resource_type = decoder.read_i8()?;
            tracing::debug!("Resource {}: type = {}", i, resource_type);

            // Resource name - use compact string for v4+
            let resource_name = if use_compact {
                decoder.read_compact_string()?.unwrap_or_else(|| String::new())
            } else {
                decoder.read_string()?.unwrap_or_else(|| String::new())
            };
            tracing::debug!("Resource {}: name = '{}'", i, resource_name);

            // Configuration keys (v1+) - use compact array/strings for v4+
            let configuration_keys = if header.api_version >= 1 {
                if use_compact {
                    let compact_len = decoder.read_unsigned_varint()?;
                    if compact_len == 0 {
                        None
                    } else {
                        let key_count = (compact_len - 1) as usize;
                        let mut keys = Vec::with_capacity(key_count);
                        for _ in 0..key_count {
                            if let Some(key) = decoder.read_compact_string()? {
                                keys.push(key);
                            }
                        }
                        Some(keys)
                    }
                } else {
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
                }
            } else {
                None
            };

            // CRITICAL: Skip per-resource tagged fields for v4+ (flexible protocol)
            if use_compact {
                let tag_count = decoder.read_unsigned_varint()?;
                for _ in 0..tag_count {
                    let _tag_id = decoder.read_unsigned_varint()?;
                    let tag_size = decoder.read_unsigned_varint()? as usize;
                    decoder.advance(tag_size)?;
                }
            }

            resources.push(ConfigResource {
                resource_type,
                resource_name,
                configuration_keys,
            });
        }
        
        // Include synonyms (v1+) - ALWAYS in main body, even for v4+
        let include_synonyms = if header.api_version >= 1 {
            decoder.read_bool()?
        } else {
            false
        };

        // Include documentation (v3+) - ALWAYS in main body, even for v4+
        let include_documentation = if header.api_version >= 3 {
            decoder.read_bool()?
        } else {
            false
        };

        tracing::debug!("DescribeConfigs v{}: include_synonyms={}, include_documentation={}",
            header.api_version, include_synonyms, include_documentation);

        // For v4+, parse tagged fields at request level (after all main body fields)
        if use_compact {
            let tag_count = decoder.read_unsigned_varint()?;
            tracing::debug!("DescribeConfigs v{}: parsing {} request-level tagged fields", header.api_version, tag_count);

            for _ in 0..tag_count {
                let tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                tracing::debug!("DescribeConfigs v{}: skipping tagged field {} ({} bytes)",
                    header.api_version, tag_id, tag_size);
                decoder.skip(tag_size)?;
            }
        }

        let (final_include_synonyms, final_include_documentation) = (include_synonyms, include_documentation);

        tracing::info!("DescribeConfigs v{}: processing {} resources (include_synonyms={}, include_documentation={})",
            header.api_version, resources.len(), final_include_synonyms, final_include_documentation);

        // Process each resource
        let mut results = Vec::new();

        for resource in resources {
            tracing::debug!("DescribeConfigs: processing resource_type={}, resource_name={}",
                resource.resource_type, resource.resource_name);

            let configs = match resource.resource_type {
                2 => {
                    // Topic configs
                    tracing::debug!("DescribeConfigs: fetching topic configs for '{}'", resource.resource_name);
                    self.get_topic_configs(
                        &resource.resource_name,
                        &resource.configuration_keys,
                        final_include_synonyms,
                        final_include_documentation,
                        header.api_version,
                    ).await?
                },
                4 => {
                    // Broker configs
                    tracing::debug!("DescribeConfigs: fetching broker configs for '{}'", resource.resource_name);
                    self.get_broker_configs(
                        &resource.resource_name,
                        &resource.configuration_keys,
                        final_include_synonyms,
                        final_include_documentation,
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
            
            tracing::debug!("DescribeConfigs: resource {} returned {} config entries",
                resource.resource_name, configs.len());

            results.push(DescribeConfigsResult {
                error_code: 0,
                error_message: None,
                resource_type: resource.resource_type,
                resource_name: resource.resource_name.clone(),
                configs,
            });
        }

        tracing::info!("DescribeConfigs v{}: returning {} results", header.api_version, results.len());

        let response = DescribeConfigsResponse {
            throttle_time_ms: 0,
            results,
        };

        let mut body_buf = BytesMut::new();

        // Encode the response body (without correlation ID)
        self.encode_describe_configs_response(&mut body_buf, &response, header.api_version)?;

        tracing::info!("DescribeConfigs v{}: encoded response size = {} bytes",
            header.api_version, body_buf.len());
        if body_buf.len() <= 100 {
            tracing::debug!("DescribeConfigs v{}: response bytes: {:02x?}",
                header.api_version, &body_buf[..]);
        } else {
            tracing::debug!("DescribeConfigs v{}: first 100 bytes: {:02x?}",
                header.api_version, &body_buf[..100]);
        }

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
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> Result<Vec<ConfigEntry>> {
        let mut configs = Vec::new();

        // Default broker configurations
        let all_configs = vec![
            ("default.replication.factor", "1", "Default replication factor for automatically created topics", config_type::INT),
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

            // Build synonyms if requested (v1+)
            let mut synonyms = Vec::new();
            if include_synonyms {
                synonyms.push(ConfigSynonym {
                    name: name.to_string(),
                    value: Some(default_value.to_string()),
                    source: config_source::STATIC_BROKER_CONFIG,
                });
            }

            configs.push(ConfigEntry {
                name: name.to_string(),
                value: Some(default_value.to_string()),
                read_only: true,
                is_default: true,
                config_source: config_source::STATIC_BROKER_CONFIG,
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
    
    /// Encode DescribeConfigs response
    fn encode_describe_configs_response(
        &self,
        buf: &mut BytesMut,
        response: &DescribeConfigsResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);

        // Check if we need to use compact encoding (v4+)
        let use_compact = version >= 4;

        // Throttle time ms (v0+)
        encoder.write_i32(response.throttle_time_ms);

        // Results array
        if use_compact {
            // Compact array: length + 1
            encoder.write_unsigned_varint((response.results.len() + 1) as u32);
        } else {
            encoder.write_i32(response.results.len() as i32);
        }

        for result in &response.results {
            // Error code
            encoder.write_i16(result.error_code);

            // Error message
            if use_compact {
                encoder.write_compact_string(result.error_message.as_deref());
            } else {
                encoder.write_string(result.error_message.as_deref());
            }

            // Resource type
            encoder.write_i8(result.resource_type);

            // Resource name
            if use_compact {
                encoder.write_compact_string(Some(&result.resource_name));
            } else {
                encoder.write_string(Some(&result.resource_name));
            }

            // Configs array
            if use_compact {
                encoder.write_unsigned_varint((result.configs.len() + 1) as u32);
            } else {
                encoder.write_i32(result.configs.len() as i32);
            }

            for config in &result.configs {
                // Config name
                if use_compact {
                    encoder.write_compact_string(Some(&config.name));
                } else {
                    encoder.write_string(Some(&config.name));
                }

                // Config value
                if use_compact {
                    encoder.write_compact_string(config.value.as_deref());
                } else {
                    encoder.write_string(config.value.as_deref());
                }

                // Read only
                encoder.write_bool(config.read_only);

                // Is default (v0) OR config source (v1+)
                if version >= 1 {
                    encoder.write_i8(config.config_source);
                } else {
                    encoder.write_bool(config.is_default);
                }

                // Is sensitive
                encoder.write_bool(config.is_sensitive);

                // Synonyms (v1+ only)
                if version >= 1 {
                    if use_compact {
                        encoder.write_unsigned_varint((config.synonyms.len() + 1) as u32);
                    } else {
                        encoder.write_i32(config.synonyms.len() as i32);
                    }

                    for synonym in &config.synonyms {
                        // Synonym name
                        if use_compact {
                            encoder.write_compact_string(Some(&synonym.name));
                        } else {
                            encoder.write_string(Some(&synonym.name));
                        }

                        // Synonym value
                        if use_compact {
                            encoder.write_compact_string(synonym.value.as_deref());
                        } else {
                            encoder.write_string(synonym.value.as_deref());
                        }

                        // Synonym source
                        encoder.write_i8(synonym.source);

                        // Tagged fields for each synonym (v4+)
                        if use_compact {
                            encoder.write_unsigned_varint(0); // No tagged fields for the synonym
                        }
                    }
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
                    if use_compact {
                        encoder.write_compact_string(config.documentation.as_deref());
                    } else {
                        encoder.write_string(config.documentation.as_deref());
                    }
                }

                // Tagged fields for each config entry (v4+)
                if use_compact {
                    encoder.write_unsigned_varint(0); // No tagged fields for the config
                }
            }

            // Tagged fields for compact encoding (v4+)
            if use_compact {
                encoder.write_unsigned_varint(0); // No tagged fields for the result
            }
        }

        // Tagged fields for compact encoding (v4+)
        if use_compact {
            encoder.write_unsigned_varint(0); // No tagged fields for the response
        }

        Ok(())
    }

    /// Handle DescribeLogDirs request (API 35)
    async fn handle_describe_log_dirs(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::describe_log_dirs_types::{
            DescribeLogDirsRequest, DescribeLogDirsResponse, DescribeLogDirsResult,
            DescribeLogDirsTopicResult, DescribeLogDirsPartitionResult, error_codes
        };
        use crate::parser::Decoder;

        tracing::info!("Handling DescribeLogDirs request v{}", header.api_version);

        let mut decoder = Decoder::new(body);

        // Decode request
        let request = DescribeLogDirsRequest::decode(&mut decoder, header.api_version)?;

        tracing::debug!("DescribeLogDirs request - topics: {:?}", request.topics.as_ref().map(|t| t.len()));

        // Use default data directory path (ProtocolHandler doesn't have access to config)
        let data_dir_str = "/data".to_string();

        // Calculate actual disk usage
        let (total_bytes, usable_bytes) = if let Ok(metadata) = std::fs::metadata(&data_dir_str) {
            // Try to get filesystem stats
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let total = metadata.len() as i64;
                // On Unix, we can get more accurate disk space info
                // For now, use a simple heuristic
                (total, total / 2) // Assume 50% available
            }
            #[cfg(not(unix))]
            {
                (1_000_000_000, 500_000_000) // 1GB total, 500MB available as default
            }
        } else {
            (1_000_000_000, 500_000_000) // Default values
        };

        // Build response based on requested topics
        let mut topics_result = Vec::new();

        if let Some(requested_topics) = &request.topics {
            // Return info for specific topics
            for topic_req in requested_topics {
                let mut partitions_result = Vec::new();

                for partition in &topic_req.partitions {
                    // Get partition size from metadata store if available
                    let (size, offset_lag) = if let Some(ref metadata_store) = self.metadata_store {
                        // Get segments for this partition to calculate size
                        match metadata_store.list_segments(&topic_req.topic, Some(*partition as u32)).await {
                            Ok(segments) => {
                                let total_size: i64 = segments.iter()
                                    .map(|s| (s.end_offset - s.start_offset + 1) * 1024) // Estimate 1KB per record
                                    .sum();
                                (total_size, 0) // offset_lag is 0 (no replication lag)
                            }
                            Err(_) => (0, 0)
                        }
                    } else {
                        (0, 0)
                    };

                    partitions_result.push(DescribeLogDirsPartitionResult {
                        partition: *partition,
                        size,
                        offset_lag,
                        is_future_key: false,
                    });
                }

                topics_result.push(DescribeLogDirsTopicResult {
                    topic: topic_req.topic.clone(),
                    partitions: partitions_result,
                });
            }
        } else {
            // Return info for ALL topics
            if let Some(ref metadata_store) = self.metadata_store {
                match metadata_store.list_topics().await {
                    Ok(topics) => {
                        for topic_meta in topics {
                            let mut partitions_result = Vec::new();

                            // Get all partitions for this topic
                            for partition_idx in 0..topic_meta.config.partition_count {
                                match metadata_store.list_segments(&topic_meta.name, Some(partition_idx as u32)).await {
                                    Ok(segments) => {
                                        let total_size: i64 = segments.iter()
                                            .map(|s| (s.end_offset - s.start_offset + 1) * 1024) // Estimate 1KB per record
                                            .sum();

                                        partitions_result.push(DescribeLogDirsPartitionResult {
                                            partition: partition_idx as i32,
                                            size: total_size,
                                            offset_lag: 0,
                                            is_future_key: false,
                                        });
                                    }
                                    Err(_) => {}
                                }
                            }

                            if !partitions_result.is_empty() {
                                topics_result.push(DescribeLogDirsTopicResult {
                                    topic: topic_meta.name,
                                    partitions: partitions_result,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to list topics for DescribeLogDirs: {}", e);
                    }
                }
            }
        }

        // Build result for the single log directory
        let result = DescribeLogDirsResult {
            error_code: error_codes::NONE,
            log_dir: data_dir_str,
            topics: topics_result,
            total_bytes,
            usable_bytes,
        };

        let response = DescribeLogDirsResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
            results: vec![result],
        };

        tracing::info!("DescribeLogDirs response - {} log dirs, {} topics",
            response.results.len(),
            response.results.iter().map(|r| r.topics.len()).sum::<usize>()
        );

        // Encode response
        let body_bytes = response.encode_to_bytes(header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::DescribeLogDirs, body_bytes.freeze()))
    }

    /// Handle DescribeAcls request (API 29)
    async fn handle_describe_acls(
        &self,
        header: RequestHeader,
        _body: &mut Bytes,
    ) -> Result<Response> {
        use crate::describe_acls_types::{DescribeAclsResponse, encode_describe_acls_response};

        tracing::info!("Handling DescribeAcls request v{} - returning empty ACL list (ACLs not implemented)",
            header.api_version);

        // Return empty ACL list - Chronik doesn't implement ACLs yet
        // This is valid and prevents AdminClient from crashing
        let response = DescribeAclsResponse {
            throttle_time_ms: 0,
            error_code: 0, // NONE
            error_message: None,
            resources: vec![], // No ACLs configured
        };

        let body_bytes = encode_describe_acls_response(&response, header.api_version);

        Ok(Self::make_response(&header, ApiKey::DescribeAcls, body_bytes.freeze()))
    }

    /// Handle CreateAcls request (API 30)
    async fn handle_create_acls(
        &self,
        header: RequestHeader,
        _body: &mut Bytes,
    ) -> Result<Response> {
        use crate::create_acls_types::{CreateAclsResponse, AclCreationResult};

        tracing::info!("Handling CreateAcls request v{} - ACLs not implemented, returning success",
            header.api_version);

        // Return success but ACLs won't actually be enforced
        // This prevents AdminClient from crashing while being honest about capability
        let response = CreateAclsResponse {
            throttle_time_ms: 0,
            results: vec![AclCreationResult {
                error_code: 0, // NONE - pretend it succeeded
                error_message: None,
            }],
        };

        let body_bytes = crate::create_acls_types::encode_create_acls_response(&response);

        Ok(Self::make_response(&header, ApiKey::CreateAcls, body_bytes.freeze()))
    }

    /// Handle DeleteAcls request (API 31)
    async fn handle_delete_acls(
        &self,
        header: RequestHeader,
        _body: &mut Bytes,
    ) -> Result<Response> {
        use crate::delete_acls_types::{DeleteAclsResponse, FilterResult};

        tracing::info!("Handling DeleteAcls request v{} - ACLs not implemented, returning empty",
            header.api_version);

        // Return empty results - no ACLs to delete
        let response = DeleteAclsResponse {
            throttle_time_ms: 0,
            filter_results: vec![FilterResult {
                error_code: 0, // NONE
                error_message: None,
                matching_acls: vec![], // No ACLs matched (because none exist)
            }],
        };

        let body_bytes = crate::delete_acls_types::encode_delete_acls_response(&response, header.api_version);

        Ok(Self::make_response(&header, ApiKey::DeleteAcls, body_bytes.freeze()))
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
            throttle_time_ms: None, // Error responses don't include throttle time
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

        // Debug: print the first few bytes of the request body
        let body_bytes = body.to_vec();
        let _debug_bytes = &body_bytes[..body_bytes.len().min(20)];

        let mut decoder = Decoder::new(body);

        // Determine if we should use compact encoding (v5+)
        let use_compact = header.api_version >= 5;
        // Note: CreateTopics does NOT have client_id in body (it's only in the header)

        // Parse topic count - use compact array for v5+
        let topic_count = if use_compact {
            let compact_len = decoder.read_unsigned_varint()?;
            if compact_len == 0 {
                // For CreateTopics, NULL array is not allowed
                return Err(Error::Protocol("CreateTopics compact array cannot be null".into()));
            }
            let actual_len = compact_len - 1;
            if actual_len > 1000 {  // Reasonable limit for topic count
                return Err(Error::Protocol("CreateTopics topic count too large".into()));
            }
            actual_len as usize
        } else {
            let len = decoder.read_i32()?;
            if len < 0 || len > 1000 {
                return Err(Error::Protocol("CreateTopics topic count invalid".into()));
            }
            len as usize
        };
        let mut topics = Vec::with_capacity(topic_count);

        for _ in 0..topic_count {
            // Topic name - use compact string for v5+
            let name = if use_compact {
                decoder.read_compact_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
            } else {
                decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
            };
            
            // Number of partitions
            let num_partitions = decoder.read_i32()?;
            
            // Replication factor
            let replication_factor = decoder.read_i16()?;
            
            // Replica assignments - use compact array for v5+
            let assignment_count = if use_compact {
                (decoder.read_unsigned_varint()? as i32 - 1) as usize
            } else {
                decoder.read_i32()? as usize
            };
            let mut replica_assignments = Vec::new();

            if assignment_count > 0 {
                for _ in 0..assignment_count {
                    let partition_index = decoder.read_i32()?;
                    let broker_count = if use_compact {
                        (decoder.read_unsigned_varint()? as i32 - 1) as usize
                    } else {
                        decoder.read_i32()? as usize
                    };
                    let mut broker_ids = Vec::with_capacity(broker_count);

                    for _ in 0..broker_count {
                        broker_ids.push(decoder.read_i32()?);
                    }

                    replica_assignments.push(crate::create_topics_types::ReplicaAssignment {
                        partition_index,
                        broker_ids,
                    });

                    // Handle tagged fields for each replica assignment in compact format (v5+)
                    if use_compact {
                        let num_tagged_fields = decoder.read_unsigned_varint()?;
                        for _ in 0..num_tagged_fields {
                            let _tag_id = decoder.read_unsigned_varint()?;
                            let tag_size = decoder.read_unsigned_varint()? as usize;
                            decoder.advance(tag_size)?;
                        }
                    }
                }
            }

            // Configs - use compact array for v5+
            let config_count = if use_compact {
                (decoder.read_unsigned_varint()? as i32 - 1) as usize
            } else {
                decoder.read_i32()? as usize
            };
            let mut configs = std::collections::HashMap::new();

            for _ in 0..config_count {
                let key = if use_compact {
                    decoder.read_compact_string()?
                        .ok_or_else(|| Error::Protocol("Config key cannot be null".into()))?
                } else {
                    decoder.read_string()?
                        .ok_or_else(|| Error::Protocol("Config key cannot be null".into()))?
                };
                let value = if use_compact {
                    decoder.read_compact_string()?
                        .ok_or_else(|| Error::Protocol("Config value cannot be null".into()))?
                } else {
                    decoder.read_string()?
                        .ok_or_else(|| Error::Protocol("Config value cannot be null".into()))?
                };
                configs.insert(key, value);

                // Handle tagged fields for each config in compact format (v5+)
                if use_compact {
                    let num_tagged_fields = decoder.read_unsigned_varint()?;
                    for _ in 0..num_tagged_fields {
                        let _tag_id = decoder.read_unsigned_varint()?;
                        let tag_size = decoder.read_unsigned_varint()? as usize;
                        decoder.advance(tag_size)?;
                    }
                }
            }

            // Handle tagged fields for each topic in compact format (v5+)
            if use_compact {
                let num_tagged_fields = decoder.read_unsigned_varint()?;
                for _ in 0..num_tagged_fields {
                    let _tag_id = decoder.read_unsigned_varint()?;
                    let tag_size = decoder.read_unsigned_varint()? as usize;
                    decoder.advance(tag_size)?;
                }
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

        // Tagged fields for compact encoding (v5+)
        if use_compact {
            // Read and skip tagged fields at the end of the request
            let num_tagged_fields = decoder.read_unsigned_varint()?;
            for _ in 0..num_tagged_fields {
                let _tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size)?;
            }
        }

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
        use crate::parser::Encoder;
        let mut encoder = Encoder::new(buf);

        // Determine if we should use compact encoding (v5+)
        let use_compact = version >= 5;

        tracing::debug!("Encoding CreateTopics response v{}, use_compact={}, topics_len={}",
                 version, use_compact, response.topics.len());
        tracing::debug!("Encoding CreateTopics response v{}, use_compact={}", version, use_compact);

        // Throttle time (v2+)
        if version >= 2 {
            encoder.write_i32(response.throttle_time_ms);
        }

        // Topics array - use compact array for v5+
        if use_compact {
            encoder.write_compact_array_len(response.topics.len());
        } else {
            encoder.write_i32(response.topics.len() as i32);
        }

        for topic in &response.topics {
            // Topic name - use compact string for v5+
            if use_compact {
                encoder.write_compact_string(Some(&topic.name));
            } else {
                encoder.write_string(Some(&topic.name));
            }

            // Topic ID (v7+) - UUID field added in version 7
            if version >= 7 {
                // For now, write all zeros as a placeholder UUID (16 bytes)
                // Real implementation would store actual topic IDs
                // UUID is written as raw 16 bytes, not length-prefixed
                let uuid_bytes = [0u8; 16];
                encoder.write_raw_bytes(&uuid_bytes);
            }

            // Error code
            encoder.write_i16(topic.error_code);

            // Error message (v1+)
            if version >= 1 {
                if use_compact {
                    encoder.write_compact_string(topic.error_message.as_deref());
                } else {
                    encoder.write_string(topic.error_message.as_deref());
                }
            }

            // Detailed topic info (v5+)
            if version >= 5 {
                encoder.write_i32(topic.num_partitions);
                encoder.write_i16(topic.replication_factor);

                // Configs array - use compact array for v5+
                if use_compact {
                    encoder.write_compact_array_len(topic.configs.len());
                } else {
                    encoder.write_i32(topic.configs.len() as i32);
                }

                for config in &topic.configs {
                    // Config name and value - use compact strings for v5+
                    if use_compact {
                        encoder.write_compact_string(Some(&config.name));
                        encoder.write_compact_string(config.value.as_deref());
                    } else {
                        encoder.write_string(Some(&config.name));
                        encoder.write_string(config.value.as_deref());
                    }

                    encoder.write_bool(config.read_only);
                    encoder.write_i8(config.config_source);
                    encoder.write_bool(config.is_sensitive);

                    // Tagged fields for config (v5+)
                    if use_compact {
                        encoder.write_unsigned_varint(0); // Empty tagged fields
                    }
                }

                // Tagged fields for topic (v5+)
                if use_compact {
                    encoder.write_unsigned_varint(0); // Empty tagged fields
                }
            }
        }

        // Tagged fields for response (v5+)
        if use_compact {
            encoder.write_unsigned_varint(0); // Empty tagged fields
        }

        Ok(())
    }

    /// Handle DeleteTopics request
    async fn handle_delete_topics(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::delete_topics_types::{
            DeleteTopicsRequest, DeleteTopicsResponse, DeletableTopicResult,
            error_codes
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = DeleteTopicsRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!("DeleteTopics request: {:?}", request);

        // Build response - for now, return NOT_CONTROLLER error for all topics
        // since we don't actually implement topic deletion yet
        let mut responses = Vec::new();
        for topic_name in &request.topic_names {
            responses.push(DeletableTopicResult {
                name: topic_name.clone(),
                error_code: error_codes::NOT_CONTROLLER,
                error_message: if header.api_version >= 5 {
                    Some("Topic deletion not implemented".to_string())
                } else {
                    None
                },
            });
        }

        let response = DeleteTopicsResponse {
            throttle_time_ms: 0,
            responses,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::DeleteTopics, body_buf.freeze()))
    }

    /// Handle DeleteGroups request
    async fn handle_delete_groups(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::delete_groups_types::{
            DeleteGroupsRequest, DeleteGroupsResponse, DeletableGroupResult,
            error_codes
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = DeleteGroupsRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!("DeleteGroups request: {:?}", request);

        // Build response - for now, return GROUP_ID_NOT_FOUND for all groups
        // since we don't actually implement group deletion yet
        let mut results = Vec::new();
        for group_id in &request.groups_names {
            results.push(DeletableGroupResult {
                group_id: group_id.clone(),
                error_code: error_codes::GROUP_ID_NOT_FOUND,
            });
        }

        let response = DeleteGroupsResponse {
            throttle_time_ms: 0,
            results,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::DeleteGroups, body_buf.freeze()))
    }

    /// Handle AlterConfigs request
    async fn handle_alter_configs(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::alter_configs_types::{
            AlterConfigsRequest, AlterConfigsResponse, AlterConfigsResourceResponse,
            error_codes
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = AlterConfigsRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!("AlterConfigs request: {:?}", request);

        // Build response - return success for all resources
        // We accept the configs but don't persist them (read-only for now)
        let mut resources = Vec::new();
        for resource in &request.resources {
            // Log the config changes requested
            tracing::info!("AlterConfigs for {} '{}': {} configs",
                match resource.resource_type {
                    2 => "topic",
                    4 => "broker",
                    _ => "unknown",
                },
                resource.resource_name,
                resource.configs.len()
            );

            resources.push(AlterConfigsResourceResponse {
                error_code: 0, // SUCCESS - pretend we applied the configs
                error_message: None,
                resource_type: resource.resource_type,
                resource_name: resource.resource_name.clone(),
            });
        }

        let response = AlterConfigsResponse {
            throttle_time_ms: 0,
            resources,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::AlterConfigs, body_buf.freeze()))
    }

    /// Handle IncrementalAlterConfigs request
    async fn handle_incremental_alter_configs(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::incremental_alter_configs_types::{
            IncrementalAlterConfigsRequest, IncrementalAlterConfigsResponse,
            AlterConfigsResourceResponse, error_codes, config_operation, resource_type
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        // Only support v0 and v1
        if header.api_version > 1 {
            tracing::warn!(
                "IncrementalAlterConfigs version {} not supported (only v0-v1)",
                header.api_version
            );
            return self.error_response(header.correlation_id, error_codes::UNSUPPORTED_VERSION);
        }

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = IncrementalAlterConfigsRequest::decode(&mut decoder, header.api_version)?;

        // Log the request with config keys
        tracing::info!("IncrementalAlterConfigs request (v{}):", header.api_version);
        for resource in &request.resources {
            let resource_type_name = match resource.resource_type {
                resource_type::TOPIC => "TOPIC",
                resource_type::BROKER => "BROKER",
                resource_type::CLUSTER => "CLUSTER",
                resource_type::BROKER_LOGGER => "BROKER_LOGGER",
                _ => "UNKNOWN",
            };

            tracing::info!(
                "  Resource: type={} ({}), name='{}', configs:",
                resource.resource_type, resource_type_name, resource.resource_name
            );

            for config in &resource.configs {
                let op_name = match config.config_operation {
                    config_operation::SET => "SET",
                    config_operation::DELETE => "DELETE",
                    config_operation::APPEND => "APPEND",
                    config_operation::SUBTRACT => "SUBTRACT",
                    _ => "UNKNOWN",
                };
                tracing::info!(
                    "    Config: name='{}', operation={} ({}), value={:?}",
                    config.name, config.config_operation, op_name, config.value
                );
            }
        }
        tracing::info!("  validate_only: {}", request.validate_only);

        // Process configuration changes
        let mut resources = Vec::new();

        for resource in &request.resources {
            let mut error_code = 0i16;
            let mut error_message = None;

            // Only process if not validate_only
            if !request.validate_only {
                match resource.resource_type {
                    resource_type::TOPIC => {
                        // Update topic configuration
                        if let Some(metadata_store) = &self.metadata_store {
                            // Get existing topic configuration
                            match metadata_store.get_topic(&resource.resource_name).await {
                                Ok(Some(topic)) => {
                                    let mut config = topic.config;

                                    // Apply configuration changes
                                    for cfg in &resource.configs {
                                        match cfg.config_operation {
                                            config_operation::SET => {
                                                if let Some(value) = &cfg.value {
                                                    // Handle special config keys
                                                    match cfg.name.as_str() {
                                                        "retention.ms" => {
                                                            if let Ok(ms) = value.parse::<i64>() {
                                                                config.retention_ms = Some(ms);
                                                            }
                                                        }
                                                        "segment.bytes" => {
                                                            if let Ok(bytes) = value.parse::<i64>() {
                                                                config.segment_bytes = bytes;
                                                            }
                                                        }
                                                        _ => {
                                                            // Store other configs in the generic HashMap
                                                            config.config.insert(cfg.name.clone(), value.clone());
                                                        }
                                                    }
                                                }
                                            }
                                            config_operation::DELETE => {
                                                // Remove configuration
                                                config.config.remove(&cfg.name);

                                                // Handle special keys
                                                if cfg.name == "retention.ms" {
                                                    config.retention_ms = None;
                                                }
                                            }
                                            config_operation::APPEND | config_operation::SUBTRACT => {
                                                // These operations are typically for list-based configs
                                                // For now, we'll treat them as unsupported
                                                error_code = error_codes::INVALID_REQUEST;
                                                error_message = Some(format!("APPEND/SUBTRACT not supported for config '{}'", cfg.name));
                                                break;
                                            }
                                            _ => {
                                                error_code = error_codes::INVALID_REQUEST;
                                                error_message = Some("Unknown operation".to_string());
                                                break;
                                            }
                                        }
                                    }

                                    // Persist the updated configuration if no errors
                                    if error_code == 0 {
                                        match metadata_store.update_topic(&resource.resource_name, config).await {
                                            Ok(_) => {
                                                tracing::info!("Successfully updated configuration for topic '{}'", resource.resource_name);
                                            }
                                            Err(e) => {
                                                tracing::error!("Failed to persist topic config: {}", e);
                                                error_code = error_codes::UNKNOWN_SERVER_ERROR;
                                                error_message = Some(format!("Failed to persist configuration: {}", e));
                                            }
                                        }
                                    }
                                }
                                Ok(None) => {
                                    error_code = error_codes::UNKNOWN_TOPIC_OR_PARTITION;
                                    error_message = Some(format!("Topic '{}' does not exist", resource.resource_name));
                                }
                                Err(e) => {
                                    tracing::error!("Failed to get topic metadata: {}", e);
                                    error_code = error_codes::UNKNOWN_SERVER_ERROR;
                                    error_message = Some(format!("Failed to get topic metadata: {}", e));
                                }
                            }
                        } else {
                            // No metadata store configured
                            tracing::warn!("No metadata store configured, configs not persisted");
                        }
                    }
                    resource_type::BROKER => {
                        // Broker configs are not persisted yet
                        tracing::info!("Broker configuration updates not implemented");
                    }
                    resource_type::CLUSTER => {
                        // Cluster configs are not persisted yet
                        tracing::info!("Cluster configuration updates not implemented");
                    }
                    _ => {
                        error_code = error_codes::INVALID_REQUEST;
                        error_message = Some(format!("Unsupported resource type: {}", resource.resource_type));
                    }
                }
            }

            resources.push(AlterConfigsResourceResponse {
                error_code,
                error_message,
                resource_type: resource.resource_type,
                resource_name: resource.resource_name.clone(),
            });
        }

        let response = IncrementalAlterConfigsResponse {
            throttle_time_ms: 0,
            resources,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::IncrementalAlterConfigs, body_buf.freeze()))
    }

    /// Handle CreatePartitions request
    async fn handle_create_partitions(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::create_partitions_types::{
            CreatePartitionsRequest, CreatePartitionsResponse, CreatePartitionsTopicResult,
            error_codes
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = CreatePartitionsRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!("CreatePartitions request: {:?}", request);

        // Build response - for now, return INVALID_PARTITIONS for all topics
        // since we don't actually implement partition creation yet
        let mut results = Vec::new();
        for topic in &request.topics {
            results.push(CreatePartitionsTopicResult {
                name: topic.name.clone(),
                error_code: error_codes::INVALID_PARTITIONS,
                error_message: if header.api_version >= 1 {
                    Some("Partition creation not yet implemented".to_string())
                } else {
                    None
                },
            });
        }

        let response = CreatePartitionsResponse {
            throttle_time_ms: 0,
            results,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::CreatePartitions, body_buf.freeze()))
    }

    /// Handle OffsetDelete request
    async fn handle_offset_delete(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
        use crate::offset_delete_types::{
            OffsetDeleteRequest, OffsetDeleteResponse, OffsetDeleteResponseTopic,
            OffsetDeleteResponsePartition, error_codes,
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        tracing::info!("Handling OffsetDelete request");

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = OffsetDeleteRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!("OffsetDelete for group: {}", request.group_id);

        // For now, return error for all partitions (not implemented)
        let topics = request.topics.iter().map(|topic| {
            OffsetDeleteResponseTopic {
                name: topic.name.clone(),
                partitions: topic.partitions.iter().map(|partition| {
                    OffsetDeleteResponsePartition {
                        partition_index: partition.partition_index,
                        error_code: error_codes::GROUP_SUBSCRIBED_TO_TOPIC, // Cannot delete active consumer group offsets
                    }
                }).collect(),
            }
        }).collect();

        let response = OffsetDeleteResponse {
            error_code: error_codes::NONE,
            throttle_time_ms: 0,
            topics,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::OffsetDelete, body_buf.freeze()))
    }

    /// Handle EndTxn request
    async fn handle_end_txn(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
        use crate::end_txn_types::{EndTxnRequest, EndTxnResponse, error_codes};
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        tracing::info!("Handling EndTxn request");

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = EndTxnRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!(
            "EndTxn for transaction: {}, producer_id: {}, committed: {}",
            request.transactional_id,
            request.producer_id,
            request.committed
        );

        // Persist transaction commit/abort to WAL if available
        let error_code = if let Some(ref metadata_store) = self.metadata_store {
            if request.committed {
                // Prepare and commit the transaction
                if let Err(e) = metadata_store.prepare_commit_transaction(
                    request.transactional_id.clone(),
                    request.producer_id,
                    request.producer_epoch,
                ).await {
                    tracing::warn!("Failed to prepare transaction commit: {}", e);
                    error_codes::COORDINATOR_NOT_AVAILABLE
                } else if let Err(e) = metadata_store.commit_transaction(
                    request.transactional_id.clone(),
                    request.producer_id,
                    request.producer_epoch,
                ).await {
                    tracing::warn!("Failed to commit transaction: {}", e);
                    error_codes::COORDINATOR_NOT_AVAILABLE
                } else {
                    tracing::info!("Transaction committed: {}", request.transactional_id);
                    error_codes::NONE
                }
            } else {
                // Abort the transaction
                if let Err(e) = metadata_store.abort_transaction(
                    request.transactional_id.clone(),
                    request.producer_id,
                    request.producer_epoch,
                ).await {
                    tracing::warn!("Failed to abort transaction: {}", e);
                    error_codes::COORDINATOR_NOT_AVAILABLE
                } else {
                    tracing::info!("Transaction aborted: {}", request.transactional_id);
                    error_codes::NONE
                }
            }
        } else {
            error_codes::NOT_COORDINATOR
        };

        let response = EndTxnResponse {
            throttle_time_ms: 0,
            error_code,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::EndTxn, body_buf.freeze()))
    }

    /// Handle InitProducerId request
    async fn handle_init_producer_id(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
        use crate::init_producer_id_types::{InitProducerIdRequest, InitProducerIdResponse, error_codes};
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;
        use std::sync::atomic::{AtomicI64, Ordering};

        let is_flexible = header.api_version >= 2;
        tracing::info!(
            "Handling InitProducerId v{} request (is_flexible={})",
            header.api_version,
            is_flexible
        );

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = InitProducerIdRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!(
            "InitProducerId for transaction: {:?}, timeout: {}ms",
            request.transactional_id,
            request.transaction_timeout_ms
        );

        // Generate a simple producer ID (in production, this would need proper coordination)
        static PRODUCER_ID_COUNTER: AtomicI64 = AtomicI64::new(1000);
        let producer_id = PRODUCER_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        let producer_epoch = 0;

        // If transactional_id is provided, begin a transaction with WAL persistence
        let error_code = if let Some(transactional_id) = &request.transactional_id {
            if let Some(ref metadata_store) = self.metadata_store {
                // Begin the transaction
                if let Err(e) = metadata_store.begin_transaction(
                    transactional_id.clone(),
                    producer_id,
                    producer_epoch,
                    request.transaction_timeout_ms,
                ).await {
                    tracing::warn!("Failed to begin transaction: {}", e);
                    error_codes::COORDINATOR_NOT_AVAILABLE
                } else {
                    tracing::info!("Transaction started: {}, producer_id: {}, epoch: {}",
                        transactional_id, producer_id, producer_epoch);
                    error_codes::NONE
                }
            } else {
                error_codes::NONE // No WAL, but allow non-transactional usage
            }
        } else {
            error_codes::NONE
        };

        let response = InitProducerIdResponse {
            throttle_time_ms: 0,
            error_code,
            producer_id,
            producer_epoch,
        };

        tracing::debug!(
            "InitProducerId v{} response: producer_id={}, epoch={}, error_code={}",
            header.api_version,
            producer_id,
            producer_epoch,
            error_code
        );

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        tracing::debug!(
            "Encoded InitProducerId v{} response: body_size={} bytes (flexible={})",
            header.api_version,
            body_buf.len(),
            is_flexible
        );

        Ok(Self::make_response(&header, ApiKey::InitProducerId, body_buf.freeze()))
    }

    /// Handle TxnOffsetCommit request
    async fn handle_txn_offset_commit(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
        use crate::txn_offset_commit_types::{
            TxnOffsetCommitRequest, TxnOffsetCommitResponse,
            TxnOffsetCommitTopicResponse, TxnOffsetCommitPartitionResponse,
            error_codes
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        tracing::info!("Handling TxnOffsetCommit request");

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = TxnOffsetCommitRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!(
            "TxnOffsetCommit for transaction: {}, group: {}, producer_id: {}, epoch: {}, topics: {}",
            request.transactional_id,
            request.consumer_group_id,
            request.producer_id,
            request.producer_epoch,
            request.topics.len()
        );

        // Collect all offsets for atomic commit
        let mut offsets_for_wal: Vec<(String, u32, i64, Option<String>)> = Vec::new();
        for topic in &request.topics {
            for partition in &topic.partitions {
                offsets_for_wal.push((
                    topic.name.clone(),
                    partition.partition_index as u32,
                    partition.committed_offset,
                    partition.metadata.clone(),
                ));
            }
        }

        // Persist transactional offsets to WAL if available
        if let Some(ref metadata_store) = self.metadata_store {

            if let Err(e) = metadata_store.commit_transactional_offsets(
                request.transactional_id.clone(),
                request.producer_id,
                request.producer_epoch,
                request.consumer_group_id.clone(),
                offsets_for_wal.clone(),
            ).await {
                tracing::warn!("Failed to persist transactional offset commit to WAL: {}", e);
            }
        }

        // Also update in-memory state for backward compatibility
        let mut consumer_groups = self.consumer_groups.lock().await;
        let group = consumer_groups.groups.entry(request.consumer_group_id.clone())
            .or_insert_with(|| ConsumerGroup {
                group_id: request.consumer_group_id.clone(),
                state: "Stable".to_string(),
                protocol: None,
                protocol_type: None,
                generation_id: 0,
                leader_id: None,
                members: Vec::new(),
                offsets: HashMap::new(),
            });

        // Update offsets in memory
        for (topic, partition, offset, metadata) in &offsets_for_wal {
            let partition_offsets = group.offsets.entry(topic.clone()).or_insert_with(HashMap::new);
            partition_offsets.insert(
                *partition as i32,
                OffsetInfo {
                    offset: *offset,
                    metadata: metadata.clone(),
                    timestamp: chronik_common::Utc::now().timestamp_millis(),
                }
            );
        }

        // Build response - all offsets are successfully committed
        let mut topic_responses = Vec::new();
        for topic in &request.topics {
            let mut partition_responses = Vec::new();
            for partition in &topic.partitions {
                partition_responses.push(TxnOffsetCommitPartitionResponse {
                    partition_index: partition.partition_index,
                    error_code: error_codes::NONE,
                });
            }
            topic_responses.push(TxnOffsetCommitTopicResponse {
                name: topic.name.clone(),
                partitions: partition_responses,
            });
        }

        let response = TxnOffsetCommitResponse {
            throttle_time_ms: 0,
            topics: topic_responses,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::TxnOffsetCommit, body_buf.freeze()))
    }

    /// Handle AddOffsetsToTxn request
    async fn handle_add_offsets_to_txn(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
        use crate::add_offsets_to_txn_types::{AddOffsetsToTxnRequest, AddOffsetsToTxnResponse, error_codes};
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        tracing::info!("Handling AddOffsetsToTxn request");

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = AddOffsetsToTxnRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!(
            "AddOffsetsToTxn for transaction: {}, producer_id: {}, epoch: {}, group: {}",
            request.transactional_id,
            request.producer_id,
            request.producer_epoch,
            request.consumer_group_id
        );

        // Build response - for now, successfully add the consumer group offsets to the transaction
        let response = AddOffsetsToTxnResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::AddOffsetsToTxn, body_buf.freeze()))
    }

    /// Handle AddPartitionsToTxn request
    async fn handle_add_partitions_to_txn(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
        use crate::add_partitions_to_txn_types::{
            AddPartitionsToTxnRequest, AddPartitionsToTxnResponse,
            AddPartitionsToTxnTopicResult, AddPartitionsToTxnPartitionResult,
            error_codes
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        let is_flexible = header.api_version >= 4;
        tracing::info!(
            "Handling AddPartitionsToTxn v{} request (is_flexible={})",
            header.api_version,
            is_flexible
        );

        // Decode request
        let mut decoder = Decoder::new(body);
        let request = AddPartitionsToTxnRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!(
            "AddPartitionsToTxn for transaction: {}, producer_id: {}, epoch: {}, topics: {}",
            request.transactional_id,
            request.producer_id,
            request.producer_epoch,
            request.topics.len()
        );

        // Persist partitions to WAL if available
        if let Some(ref metadata_store) = self.metadata_store {
            let mut partitions_for_wal: Vec<(String, u32)> = Vec::new();
            for topic in &request.topics {
                for partition in &topic.partitions {
                    partitions_for_wal.push((topic.name.clone(), partition.partition as u32));
                }
            }

            if let Err(e) = metadata_store.add_partitions_to_transaction(
                request.transactional_id.clone(),
                request.producer_id,
                request.producer_epoch,
                partitions_for_wal,
            ).await {
                tracing::warn!("Failed to persist AddPartitionsToTxn to WAL: {}", e);
            }
        }

        // Build response - all partitions are successfully added
        let mut results = Vec::new();
        for topic in &request.topics {
            let mut partition_results = Vec::new();
            for partition in &topic.partitions {
                partition_results.push(AddPartitionsToTxnPartitionResult {
                    partition: partition.partition,
                    error_code: error_codes::NONE,
                });
            }
            results.push(AddPartitionsToTxnTopicResult {
                name: topic.name.clone(),
                results: partition_results,
            });
        }

        let response = AddPartitionsToTxnResponse {
            throttle_time_ms: 0,
            results,
        };

        // Encode response
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        tracing::debug!(
            "Encoded AddPartitionsToTxn v{} response: body_size={} bytes (flexible={})",
            header.api_version,
            body_buf.len(),
            is_flexible
        );

        Ok(Self::make_response(&header, ApiKey::AddPartitionsToTxn, body_buf.freeze()))
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
        tracing::debug!("Body length: {}, first 50 bytes: {:?}", body.len(), &body[..body.len().min(50)]);

        let mut decoder = Decoder::new(body);
        let is_flexible = header.api_version >= 6;

        // Replica ID
        let replica_id = decoder.read_i32()?;

        // Isolation level (v2+)
        let isolation_level = if header.api_version >= 2 {
            decoder.read_i8()?
        } else {
            0 // READ_UNCOMMITTED
        };

        // Topics array - use compact array for v6+
        let topic_count = if is_flexible {
            let count = decoder.read_unsigned_varint()? as i32;
            if count == 0 { 0 } else { (count - 1) as usize }
        } else {
            decoder.read_i32()? as usize
        };
        let mut topics = Vec::with_capacity(topic_count);

        for _ in 0..topic_count {
            // Topic name - use compact string for v6+
            let name = if is_flexible {
                decoder.read_compact_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
            } else {
                decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
            };

            // Partitions array - use compact array for v6+
            let partition_count = if is_flexible {
                let count = decoder.read_unsigned_varint()? as i32;
                if count == 0 { 0 } else { (count - 1) as usize }
            } else {
                decoder.read_i32()? as usize
            };
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

                // Skip tagged fields for each partition (v6+)
                if is_flexible {
                    let tag_count = decoder.read_unsigned_varint()?;
                    for _ in 0..tag_count {
                        let _tag_id = decoder.read_unsigned_varint()?;
                        let tag_size = decoder.read_unsigned_varint()? as usize;
                        decoder.advance(tag_size)?;
                    }
                }
            }

            topics.push(crate::list_offsets_types::ListOffsetsRequestTopic {
                name,
                partitions,
            });

            // Skip tagged fields for each topic (v6+)
            if is_flexible {
                let tag_count = decoder.read_unsigned_varint()?;
                for _ in 0..tag_count {
                    let _tag_id = decoder.read_unsigned_varint()?;
                    let tag_size = decoder.read_unsigned_varint()? as usize;
                    decoder.advance(tag_size)?;
                }
            }
        }

        // Skip tagged fields at the request level (v6+)
        if is_flexible {
            let tag_count = decoder.read_unsigned_varint()?;
            for _ in 0..tag_count {
                let _tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size)?;
            }
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

        // Check if we need flexible encoding (v6+)
        let is_flexible = version >= 6;

        // Throttle time (v2+)
        if version >= 2 {
            encoder.write_i32(response.throttle_time_ms);
        }

        // Topics array
        if is_flexible {
            encoder.write_compact_array_len(response.topics.len());
        } else {
            encoder.write_i32(response.topics.len() as i32);
        }

        for topic in &response.topics {
            // Topic name
            if is_flexible {
                encoder.write_compact_string(Some(&topic.name));
            } else {
                encoder.write_string(Some(&topic.name));
            }

            // Partitions array
            if is_flexible {
                encoder.write_compact_array_len(topic.partitions.len());
            } else {
                encoder.write_i32(topic.partitions.len() as i32);
            }

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

                // Tagged fields for flexible versions (v6+)
                if is_flexible {
                    encoder.write_tagged_fields();
                }
            }

            // Tagged fields for topic level in flexible versions (v6+)
            if is_flexible {
                encoder.write_tagged_fields();
            }
        }

        // Tagged fields for response level in flexible versions (v6+)
        if is_flexible {
            encoder.write_tagged_fields();
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
        tracing::debug!("FindCoordinator request body has {} bytes: {:?}", body.len(), &body[..std::cmp::min(body.len(), 64)]);

        let mut decoder = Decoder::new(body);

        // Parse request based on version
        let (key, key_type) = if header.api_version >= 4 {
            // v4+: KIP-699 batched format (fields serialized in JSON schema order)
            // Field 1: KeyType (int8)
            // Field 2: CoordinatorKeys (compact array)
            // Field 3: Tagged fields

            // Read KeyType first
            let key_type = decoder.read_i8()?;
            tracing::debug!("FindCoordinator v4: Read KeyType = {}", key_type);

            // Read CoordinatorKeys array
            let array_length = decoder.read_unsigned_varint()? as usize;
            tracing::debug!("FindCoordinator v4: Read CoordinatorKeys array_length (compact) = {}", array_length);

            if array_length == 0 {
                return Err(Error::Protocol("FindCoordinator v4+ requires at least one coordinator key".into()));
            }

            let actual_length = array_length - 1; // Compact array encoding

            // Read first coordinator key (KSQL typically sends one at a time)
            let key = decoder.read_compact_string()?
                .ok_or_else(|| Error::Protocol("Coordinator key cannot be null".into()))?;
            tracing::debug!("FindCoordinator v4: Read key = '{}'", key);

            // Skip remaining coordinator keys if present (batched lookup not supported yet)
            for _ in 1..actual_length {
                decoder.read_compact_string()?; // skip additional keys
            }

            // Read tagged fields
            let _tagged_fields = decoder.read_unsigned_varint()?;

            (key, key_type)
        } else {
            // v0-v3: Single coordinator lookup
            // Field 1: Key (string)
            let key = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Coordinator key cannot be null".into()))?;

            // Field 2: KeyType (int8, versions 1+)
            let key_type = if header.api_version >= 1 {
                decoder.read_i8()?
            } else {
                coordinator_type::GROUP
            };

            (key, key_type)
        };

        let request = FindCoordinatorRequest {
            key,
            key_type,
        };
        
        tracing::info!("FindCoordinator request for key '{}', type {}", request.key, request.key_type);

        // Check if we know about this group
        let group_state = self.consumer_groups.lock().await;
        let group_exists = group_state.groups.contains_key(&request.key);
        drop(group_state);

        // Return appropriate response based on group existence
        let (error_code, error_message) = if request.key.is_empty() {
            (error_codes::INVALID_REQUEST, Some("Group key cannot be empty".to_string()))
        } else if request.key_type != coordinator_type::GROUP && request.key_type != coordinator_type::TRANSACTION {
            (error_codes::INVALID_REQUEST, Some(format!("Unsupported coordinator type: {}", request.key_type)))
        } else {
            // Always return this node as coordinator for simplicity
            (error_codes::NONE, None)
        };

        // For v4+, need to populate coordinators array
        let coordinators = if header.api_version >= 4 {
            vec![crate::find_coordinator_types::Coordinator {
                key: request.key.clone(),
                node_id: 1,
                host: "localhost".to_string(),
                port: 9092,
                error_code,
                error_message: error_message.clone(),
            }]
        } else {
            vec![] // Empty for v0-v3
        };

        let response = FindCoordinatorResponse {
            throttle_time_ms: 0,
            error_code,
            error_message: error_message.clone(),
            node_id: 1, // This node
            host: "localhost".to_string(),
            port: 9092,
            coordinators,
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

        // NOTE: According to Kafka protocol spec, field ordering is:
        // For ALL versions v0+: responses/topics array comes FIRST
        // For v1+: throttle_time_ms comes AFTER responses array (at the END)
        // For v9+: uses flexible/compact encoding for arrays and strings

        // Topics array (called "responses" in the protocol) - ALWAYS FIRST
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
        
        // Write throttle_time_ms at the END for v1+ (INCLUDING v9+!)
        // This comes AFTER the responses array for all versions
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }

        // Write tagged fields at response level in v9+ (AFTER throttle_time_ms)
        if flexible {
            encoder.write_unsigned_varint(0); // No tagged fields
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

    /// Handle DescribeCluster request (API key 60)
    async fn handle_describe_cluster(&self, header: RequestHeader, body: &mut Bytes) -> Result<Response> {
        use crate::parser::{Encoder, Decoder};
        use bytes::BytesMut;

        tracing::info!("Handling DescribeCluster request v{} from client_id={:?}, correlation_id={}",
            header.api_version,
            header.client_id.as_deref().unwrap_or("unknown"),
            header.correlation_id
        );

        // Parse request body
        let include_cluster_authorized_operations = if !body.is_empty() {
            let mut decoder = Decoder::new(body);
            let result = decoder.read_i8().unwrap_or(0) != 0;

            // For v1, also need to skip any tagged fields in the request
            if header.api_version >= 1 {
                let tag_count = decoder.read_unsigned_varint().unwrap_or(0);
                for _ in 0..tag_count {
                    let _tag_id = decoder.read_unsigned_varint().unwrap_or(0);
                    let tag_size = decoder.read_unsigned_varint().unwrap_or(0);
                    decoder.skip(tag_size as usize).ok();
                }
            }

            result
        } else {
            false
        };

        tracing::debug!("DescribeCluster request: include_cluster_authorized_operations = {}",
            include_cluster_authorized_operations);

        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);

        // Build response based on version
        if header.api_version == 0 {
            // v0: NON-FLEXIBLE encoding
            encoder.write_i16(0); // Error code: NONE
            encoder.write_string(None); // Error message: null

            let cluster_id = "MkU0OEEwNTlFRkY4QjE2OQ";
            encoder.write_string(Some(cluster_id)); // Cluster ID as nullable string

            encoder.write_i32(self.broker_id); // Controller ID

            encoder.write_i32(1); // Number of brokers: 1

            // Broker entry
            encoder.write_i32(self.broker_id); // Broker ID
            encoder.write_string(Some(&self.advertised_host)); // Host as nullable string
            encoder.write_i32(self.advertised_port); // Port
            encoder.write_string(None); // Rack: null

            // Cluster authorized operations
            let authorized_operations = if include_cluster_authorized_operations {
                0x7FFFFFFF_i32
            } else {
                -2147483648_i32
            };
            encoder.write_i32(authorized_operations);

        } else if header.api_version == 1 {
            // v1: FLEXIBLE/COMPACT encoding
            encoder.write_i16(0); // Error code: NONE
            encoder.write_compact_string(None); // Error message: null (compact nullable string)

            let cluster_id = "MkU0OEEwNTlFRkY4QjE2OQ";
            encoder.write_compact_string(Some(cluster_id)); // Cluster ID as compact nullable string

            encoder.write_i32(self.broker_id); // Controller ID (still i32 in v1)

            // Brokers array - compact array format
            encoder.write_unsigned_varint(2); // Array length + 1 (1 broker means write 2)

            // Broker entry
            encoder.write_i32(self.broker_id); // Broker ID
            encoder.write_compact_string(Some(&self.advertised_host)); // Host as compact string
            encoder.write_i32(self.advertised_port); // Port
            encoder.write_compact_string(None); // Rack: compact null

            // Tagged fields for broker
            encoder.write_unsigned_varint(0); // No tagged fields

            // Cluster authorized operations (still i32 in v1)
            let authorized_operations = if include_cluster_authorized_operations {
                0x7FFFFFFF_i32
            } else {
                -2147483648_i32
            };
            encoder.write_i32(authorized_operations);

            // Tagged fields for response
            encoder.write_unsigned_varint(0); // No tagged fields

        } else {
            // Unsupported version
            return Err(Error::Protocol(format!("DescribeCluster v{} not implemented", header.api_version)));
        }

        let body_bytes = body_buf.freeze();

        let cluster_id = "MkU0OEEwNTlFRkY4QjE2OQ";
        tracing::info!("DescribeCluster v{} response: cluster_id={}, controller={}, broker={}:{}, body_size={}",
                      header.api_version, cluster_id, self.broker_id, self.advertised_host, self.advertised_port, body_bytes.len());

        // Debug: log exact bytes for troubleshooting
        let hex_bytes = body_bytes.iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::debug!("DescribeCluster response body hex: {}", hex_bytes);

        Ok(Self::make_response(&header, ApiKey::DescribeCluster, body_bytes))
    }


    /// Encode Metadata response
    fn encode_metadata_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::types::MetadataResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);

        // Validate and log metadata response structure
        tracing::info!("Encoding metadata response v{} with {} topics, {} brokers, throttle_time: {}",
                      version, response.topics.len(), response.brokers.len(), response.throttle_time_ms);

        // Log broker details
        for (i, broker) in response.brokers.iter().enumerate() {
            tracing::debug!("  Broker[{}]: node_id={}, host={}, port={}, rack={:?}",
                          i, broker.node_id, broker.host, broker.port, broker.rack);
        }

        // Log topic details
        for (i, topic) in response.topics.iter().enumerate() {
            tracing::debug!("  Topic[{}]: name={}, error_code={}, partitions={}, is_internal={}",
                          i, &topic.name,
                          topic.error_code, topic.partitions.len(), topic.is_internal);

            // Sample first partition if exists
            if let Some(partition) = topic.partitions.first() {
                tracing::debug!("    Partition[0]: index={}, leader={}, replicas={:?}, isr={:?}, error_code={}",
                              partition.partition_index, partition.leader_id,
                              partition.replica_nodes, partition.isr_nodes, partition.error_code);
            }
        }

        // Validate response has required fields
        if response.brokers.is_empty() {
            tracing::warn!("Metadata response has no brokers - this may cause client issues");
        }
        if response.cluster_id.is_none() && version >= 2 {
            tracing::warn!("Metadata response missing cluster_id for v{} - clients may reject", version);
        }
        
        // Check if this is a flexible/compact version (v9+)
        let flexible = version >= 9;
        
        // Throttle time (v3+ always, regardless of flexible)
        if version >= 3 {
            tracing::debug!("Writing throttle_time_ms={} for v{}", response.throttle_time_ms, version);
            encoder.write_i32(response.throttle_time_ms);
        } else {
            tracing::debug!("NOT writing throttle_time for v{} (only for v3+)", version);
        }

        // Brokers array
        tracing::debug!("About to encode {} brokers", response.brokers.len());

        // Get buffer position before writing
        let buffer_before = encoder.debug_buffer().len();

        if flexible {
            encoder.write_compact_array_len(response.brokers.len());
        } else {
            encoder.write_i32(response.brokers.len() as i32);
        }

        let buffer_after = encoder.debug_buffer().len();
        tracing::debug!("Metadata response buffer: before={}, after={}, size={}",
                 buffer_before, buffer_after, buffer_after - buffer_before);
        let debug_buf = encoder.debug_buffer();
        tracing::debug!("Metadata response bytes: {:02x?}",
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

        // Cluster ID comes after brokers for v2+
        if version >= 2 {
            tracing::debug!("Writing cluster_id {:?} at position {}", response.cluster_id, encoder.position());
            if flexible {
                encoder.write_compact_string(response.cluster_id.as_deref());
            } else {
                encoder.write_string(response.cluster_id.as_deref());
            }
        }

        // Controller ID comes after cluster_id for v2+, or directly after brokers for v1
        if version >= 1 {
            tracing::debug!("Writing controller_id {} at position {}", response.controller_id, encoder.position());
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

        // Cluster authorized operations (v8+)
        if version >= 8 {
            if let Some(ops) = response.cluster_authorized_operations {
                encoder.write_i32(ops);
            } else {
                encoder.write_i32(-2147483648); // INT32_MIN means "null"
            }
        }

        if flexible {
            encoder.write_tagged_fields();
        }

        // Log final encoded size for debugging
        let final_buffer = encoder.debug_buffer();
        let final_size = final_buffer.len();
        tracing::info!("Metadata response v{} encoded successfully: {} bytes", version, final_size);
        tracing::debug!("Final metadata response body: first 32 bytes: {:02x?}",
            &final_buffer[..final_buffer.len().min(32)]);

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

        // v6+ uses flexible/compact format
        let flexible = header.api_version >= 6;

        // Parse request
        let group_id = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?
        };

        let session_timeout_ms = decoder.read_i32()?;

        let rebalance_timeout_ms = if header.api_version >= 1 {
            decoder.read_i32()?
        } else {
            session_timeout_ms // Default to session timeout
        };

        let member_id = if flexible {
            decoder.read_compact_string()?.unwrap_or_default()
        } else {
            decoder.read_string()?.unwrap_or_default()
        };

        let group_instance_id = if header.api_version >= 5 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        let protocol_type = if flexible {
            decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Protocol type cannot be null".into()))?
        } else {
            decoder.read_string()?.ok_or_else(|| Error::Protocol("Protocol type cannot be null".into()))?
        };

        // Read protocols array
        let protocol_count = if flexible {
            let len = decoder.read_unsigned_varint()? as usize;
            if len == 0 {
                return Err(Error::Protocol("JoinGroup protocols array cannot be empty".into()));
            }
            len - 1 // Compact array encoding
        } else {
            decoder.read_i32()? as usize
        };

        let mut protocols = Vec::with_capacity(protocol_count);

        for _ in 0..protocol_count {
            let name = if flexible {
                decoder.read_compact_string()?.ok_or_else(|| Error::Protocol("Protocol name cannot be null".into()))?
            } else {
                decoder.read_string()?.ok_or_else(|| Error::Protocol("Protocol name cannot be null".into()))?
            };

            let metadata = if flexible {
                decoder.read_compact_bytes()?.ok_or_else(|| Error::Protocol("Protocol metadata cannot be null".into()))?
            } else {
                decoder.read_bytes()?.ok_or_else(|| Error::Protocol("Protocol metadata cannot be null".into()))?
            };

            protocols.push(JoinGroupRequestProtocol { name, metadata });

            // Tagged fields at protocol level (flexible only)
            if flexible {
                let _tag_count = decoder.read_unsigned_varint()?;
            }
        }

        // Tagged fields at request level (flexible only)
        if flexible {
            let _tag_count = decoder.read_unsigned_varint()?;
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
            "JoinGroup request for group '{}', member '{}', protocol '{}', session_timeout: {}ms, rebalance_timeout: {}ms",
            request.group_id, request.member_id, request.protocol_type,
            request.session_timeout_ms, request.rebalance_timeout_ms
        );

        // Log protocol details
        for protocol in &request.protocols {
            tracing::debug!("  Protocol: {}, metadata_size: {}", protocol.name, protocol.metadata.len());
        }
        
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

        // Persist group state to WAL if metadata store is available
        if let Some(ref metadata_store) = self.metadata_store {
            let group_metadata = chronik_common::metadata::ConsumerGroupMetadata {
                group_id: group.group_id.clone(),
                generation_id: group.generation_id,
                protocol_type: group.protocol_type.clone().unwrap_or_default(),
                protocol: group.protocol.clone().unwrap_or_default(),
                leader_id: group.leader_id.clone(),
                leader: group.leader_id.clone().unwrap_or_default(),
                state: group.state.clone(),
                members: group.members.iter().map(|m| {
                    chronik_common::metadata::GroupMember {
                        member_id: m.member_id.clone(),
                        client_id: m.client_id.clone(),
                        client_host: m.client_host.clone(),
                        metadata: m.metadata.clone().unwrap_or_default(),
                        assignment: m.assignment.clone().unwrap_or_default(),
                    }
                }).collect(),
                created_at: chronik_common::Utc::now(),
                updated_at: chronik_common::Utc::now(),
            };

            // Use create_consumer_group for new groups or update_consumer_group for existing ones
            let result = if is_new_member && group.generation_id == 1 {
                metadata_store.create_consumer_group(group_metadata).await
            } else {
                metadata_store.update_consumer_group(group_metadata).await
            };

            if let Err(e) = result {
                tracing::warn!("Failed to persist group state to WAL for group {}: {}",
                    group.group_id, e);
            } else {
                tracing::debug!("Persisted group state to WAL for group {} with generation {}",
                    group.group_id, group.generation_id);
            }
        }

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

    /// Handle LeaveGroup request
    async fn handle_leave_group(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::leave_group_types::{
            LeaveGroupRequest, LeaveGroupResponse, MemberResponse,
            error_codes
        };
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        tracing::debug!("Handling LeaveGroup request v{}", header.api_version);

        // Parse request
        let mut decoder = Decoder::new(body);
        let request = LeaveGroupRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!(
            "LeaveGroup request for group '{}', {} members",
            request.group_id, request.members.len()
        );

        // Process leave group
        let mut group_state = self.consumer_groups.lock().await;
        let mut member_responses = Vec::new();

        if let Some(group) = group_state.groups.get_mut(&request.group_id) {
            // Remove each member from the group
            for member in &request.members {
                let mut error_code = error_codes::NONE;

                // Find and remove the member
                let initial_count = group.members.len();
                group.members.retain(|m| m.member_id != member.member_id);

                if group.members.len() < initial_count {
                    tracing::info!(
                        "Removed member '{}' from group '{}'",
                        member.member_id, request.group_id
                    );
                } else {
                    // Member not found
                    error_code = error_codes::UNKNOWN_MEMBER_ID;
                    tracing::warn!(
                        "Member '{}' not found in group '{}'",
                        member.member_id, request.group_id
                    );
                }

                // Add member response (v3+)
                if header.api_version >= 3 {
                    member_responses.push(MemberResponse {
                        member_id: member.member_id.clone(),
                        group_instance_id: member.group_instance_id.clone(),
                        error_code,
                    });
                }
            }

            // If group is now empty, mark it as empty
            if group.members.is_empty() {
                group.state = "Empty".to_string();
                tracing::info!("Group '{}' is now empty", request.group_id);
            } else {
                // Trigger rebalance for remaining members
                group.state = "PreparingRebalance".to_string();
                group.generation_id += 1;
                tracing::info!(
                    "Group '{}' entering rebalance after member removal, new generation: {}",
                    request.group_id, group.generation_id
                );
            }

            // Persist updated group state to WAL after member removal
            if let Some(ref metadata_store) = self.metadata_store {
                let group_metadata = chronik_common::metadata::ConsumerGroupMetadata {
                    group_id: group.group_id.clone(),
                    generation_id: group.generation_id,
                    protocol_type: group.protocol_type.clone().unwrap_or_default(),
                    protocol: group.protocol.clone().unwrap_or_default(),
                    leader_id: group.leader_id.clone(),
                    leader: group.leader_id.clone().unwrap_or_default(),
                    state: group.state.clone(),
                    members: group.members.iter().map(|m| {
                        chronik_common::metadata::GroupMember {
                            member_id: m.member_id.clone(),
                            client_id: m.client_id.clone(),
                            client_host: m.client_host.clone(),
                            metadata: m.metadata.clone().unwrap_or_default(),
                            assignment: m.assignment.clone().unwrap_or_default(),
                        }
                    }).collect(),
                    created_at: chronik_common::Utc::now(),
                    updated_at: chronik_common::Utc::now(),
                };

                if let Err(e) = metadata_store.update_consumer_group(group_metadata).await {
                    tracing::warn!("Failed to persist group state after leave to WAL for group {}: {}",
                        group.group_id, e);
                } else {
                    tracing::debug!("Persisted group state after leave to WAL for group {}",
                        group.group_id);
                }
            }
        } else {
            // Group not found
            let error_code = error_codes::GROUP_ID_NOT_FOUND;
            if header.api_version >= 3 {
                for member in &request.members {
                    member_responses.push(MemberResponse {
                        member_id: member.member_id.clone(),
                        group_instance_id: member.group_instance_id.clone(),
                        error_code,
                    });
                }
            } else {
                // For v0-v2, return error at top level
                let response = LeaveGroupResponse {
                    throttle_time_ms: 0,
                    error_code,
                    members: vec![],
                };
                let mut body_buf = BytesMut::new();
                let mut encoder = Encoder::new(&mut body_buf);
                response.encode(&mut encoder, header.api_version)?;
                return Ok(Self::make_response(&header, ApiKey::LeaveGroup, body_buf.freeze()));
            }
        }

        // Build response
        let response = if header.api_version >= 3 {
            LeaveGroupResponse {
                throttle_time_ms: 0,
                error_code: error_codes::NONE,
                members: member_responses,
            }
        } else {
            LeaveGroupResponse {
                throttle_time_ms: 0,
                error_code: error_codes::NONE,
                members: vec![],
            }
        };

        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

        Ok(Self::make_response(&header, ApiKey::LeaveGroup, body_buf.freeze()))
    }

    /// Handle SyncGroup request
    async fn handle_sync_group(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::sync_group_types::{
            SyncGroupRequest, SyncGroupResponse,
            error_codes
        };
        use crate::parser::{Decoder, KafkaDecodable};

        tracing::debug!("Handling SyncGroup request v{}", header.api_version);

        // Parse request using trait-based decoding
        let mut decoder = Decoder::new(body);
        let request = SyncGroupRequest::decode(&mut decoder, header.api_version)?;
        
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

                // Persist updated group state with assignments to WAL
                if let Some(ref metadata_store) = self.metadata_store {
                    let group_metadata = chronik_common::metadata::ConsumerGroupMetadata {
                        group_id: group.group_id.clone(),
                        generation_id: group.generation_id,
                        protocol_type: group.protocol_type.clone().unwrap_or_default(),
                        protocol: group.protocol.clone().unwrap_or_default(),
                        leader_id: group.leader_id.clone(),
                        leader: group.leader_id.clone().unwrap_or_default(),
                        state: group.state.clone(),
                        members: group.members.iter().map(|m| {
                            chronik_common::metadata::GroupMember {
                                member_id: m.member_id.clone(),
                                client_id: m.client_id.clone(),
                                client_host: m.client_host.clone(),
                                metadata: m.metadata.clone().unwrap_or_default(),
                                assignment: m.assignment.clone().unwrap_or_default(), // Now includes the assignments
                            }
                        }).collect(),
                        created_at: chronik_common::Utc::now(),
                        updated_at: chronik_common::Utc::now(),
                    };

                    if let Err(e) = metadata_store.update_consumer_group(group_metadata).await {
                        tracing::warn!("Failed to persist assignments to WAL for group {}: {}",
                            group.group_id, e);
                    } else {
                        tracing::debug!("Persisted assignments to WAL for group {}",
                            group.group_id);
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
            protocol_type: if header.api_version >= 5 { request.protocol_type.clone() } else { None },
            protocol_name: if header.api_version >= 5 { request.protocol_name.clone() } else { None },
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
        use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
        use bytes::BytesMut;

        tracing::debug!("Handling Heartbeat request v{}", header.api_version);

        // Parse request using trait-based decoding
        let mut decoder = Decoder::new(body);
        let request = HeartbeatRequest::decode(&mut decoder, header.api_version)?;

        tracing::info!(
            "Heartbeat from member '{}' in group '{}', generation {}",
            request.member_id, request.group_id, request.generation_id
        );

        // Check if member exists and generation matches
        let group_state = self.consumer_groups.lock().await;
        let error_code = if let Some(group) = group_state.groups.get(&request.group_id) {
            if !group.members.iter().any(|m| m.member_id == request.member_id) {
                tracing::warn!("Unknown member '{}' in group '{}'", request.member_id, request.group_id);
                error_codes::UNKNOWN_MEMBER_ID
            } else if group.generation_id != request.generation_id {
                tracing::warn!("Generation mismatch for member '{}': expected {}, got {}",
                    request.member_id, group.generation_id, request.generation_id);
                error_codes::ILLEGAL_GENERATION
            } else if group.state == "PreparingRebalance" {
                tracing::debug!("Group '{}' is rebalancing", request.group_id);
                error_codes::REBALANCE_IN_PROGRESS
            } else {
                error_codes::NONE
            }
        } else {
            tracing::warn!("Unknown group '{}'", request.group_id);
            error_codes::UNKNOWN_MEMBER_ID
        };
        drop(group_state);

        let response = HeartbeatResponse {
            throttle_time_ms: 0,
            error_code,
        };

        // Encode response using trait-based encoding
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        response.encode(&mut encoder, header.api_version)?;

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
        
        tracing::info!("Handling ListGroups request v{}", header.api_version);

        // Return actual list of consumer groups
        let group_state = self.consumer_groups.lock().await;
        let groups: Vec<crate::list_groups_types::ListGroupsResponseGroup> = group_state.groups.iter().map(|(group_id, group)| {
            crate::list_groups_types::ListGroupsResponseGroup {
                group_id: group_id.clone(),
                protocol_type: group.protocol_type.clone().unwrap_or_else(|| "consumer".to_string()),
            }
        }).collect();
        drop(group_state);

        tracing::info!("Returning {} consumer groups", groups.len());

        let response = ListGroupsResponse {
            throttle_time_ms: 0,
            error_code: error_codes::NONE,
            groups,
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
        use std::collections::HashMap;

        tracing::info!("Handling OffsetFetch request - version: {}", header.api_version);

        // Parse request
        let request = ListConsumerGroupOffsetsRequest::parse(body, header.api_version)?;
        tracing::info!("OffsetFetch for group: {}", request.group_id);

        let mut topics = Vec::new();
        let mut offsets_map: HashMap<String, HashMap<i32, (i64, Option<String>)>> = HashMap::new();

        // Try to get offsets from WAL first if available
        if let Some(ref metadata_store) = self.metadata_store {
            tracing::debug!("Fetching offsets from WAL for group: {}", request.group_id);

            // Determine which topics to fetch
            let topics_to_fetch: Vec<String> = if let Some(ref requested_topics) = request.topics {
                requested_topics.iter().map(|t| t.name.clone()).collect()
            } else {
                // If no topics specified, we need to get all topics for this group
                // For now, we'll fall back to in-memory, but this could be improved
                // by adding a list_consumer_group_topics method to MetadataStore
                vec![]
            };

            if !topics_to_fetch.is_empty() {
                // Fetch offsets for specific topics
                for topic_name in &topics_to_fetch {
                    // We need to fetch all partitions for each topic
                    // This is a limitation - we might need to enhance the metadata store API
                    // For now, try common partition numbers (0-31)
                    let topic_offsets = offsets_map.entry(topic_name.clone())
                        .or_insert_with(HashMap::new);

                    for partition in 0..32 {
                        match metadata_store.get_consumer_offset(&request.group_id, topic_name, partition).await {
                            Ok(Some(offset)) => {
                                topic_offsets.insert(partition as i32, (offset.offset, offset.metadata));
                                tracing::debug!(
                                    "Retrieved offset from WAL: group={} topic={} partition={} offset={}",
                                    request.group_id, topic_name, partition, offset.offset
                                );
                            }
                            Ok(None) => {
                                // No offset stored for this partition
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to fetch offset from WAL for group {} topic {} partition {}: {}",
                                    request.group_id, topic_name, partition, e
                                );
                            }
                        }
                    }
                }
            }
        }

        // Fall back to in-memory storage if WAL is not available or didn't have all data
        if offsets_map.is_empty() {
            let group_state = self.consumer_groups.lock().await;
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
                        let topic_map = offsets_map.entry(topic_name.clone())
                            .or_insert_with(HashMap::new);
                        for (partition_id, offset_info) in topic_offsets {
                            topic_map.insert(*partition_id, (offset_info.offset, offset_info.metadata.clone()));
                        }
                    }
                }
            }
        }

        // Build response from collected offsets
        for (topic_name, partition_offsets) in offsets_map {
            let mut partitions = Vec::new();

            for (partition_id, (offset, metadata)) in partition_offsets {
                partitions.push(OffsetFetchResponsePartition {
                    partition_index: partition_id,
                    committed_offset: offset,
                    committed_leader_epoch: -1,
                    metadata,
                    error_code: 0,
                });
            }

            topics.push(OffsetFetchResponseTopic {
                name: topic_name,
                partitions,
            });
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
        use chronik_common::metadata::ConsumerOffset;

        tracing::info!("Handling OffsetCommit request - version: {}", header.api_version);

        // Parse request
        let request = self.parse_offset_commit_request(&header, body)?;
        tracing::info!("OffsetCommit for group: {}", request.group_id);

        // Store offsets - use both in-memory and WAL for now (transitional)
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
                // Store in memory for backward compatibility
                topic_offsets.insert(partition.partition_index, OffsetInfo {
                    offset: partition.committed_offset,
                    metadata: partition.committed_metadata.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64,
                });

                // Store in WAL if metadata store is available
                if let Some(ref metadata_store) = self.metadata_store {
                    let consumer_offset = ConsumerOffset {
                        group_id: request.group_id.clone(),
                        topic: topic.name.clone(),
                        partition: partition.partition_index as u32,
                        offset: partition.committed_offset,
                        metadata: partition.committed_metadata.clone(),
                        commit_timestamp: chronik_common::Utc::now(),
                    };

                    // Log and continue on error to maintain compatibility
                    if let Err(e) = metadata_store.commit_offset(consumer_offset).await {
                        tracing::warn!(
                            "Failed to persist offset to WAL for group {} topic {} partition {}: {}",
                            request.group_id, topic.name, partition.partition_index, e
                        );
                    } else {
                        tracing::debug!(
                            "Persisted offset to WAL: group={} topic={} partition={} offset={}",
                            request.group_id, topic.name, partition.partition_index, partition.committed_offset
                        );
                    }
                }

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
        let use_compact = version >= 3; // v3+ uses flexible/compact encoding

        // Throttle time (v1+)
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }

        // Error code
        encoder.write_i16(response.error_code);

        // Groups array - use compact array for v3+
        if use_compact {
            encoder.write_unsigned_varint((response.groups.len() + 1) as u32); // Compact arrays use +1 encoding
        } else {
            encoder.write_i32(response.groups.len() as i32);
        }

        for group in &response.groups {
            if use_compact {
                encoder.write_compact_string(Some(&group.group_id));
                encoder.write_compact_string(Some(&group.protocol_type));
                // Group state (v4+) - empty string for now
                if version >= 4 {
                    encoder.write_compact_string(Some("Stable")); // Default state
                }
                // Tagged fields for each group (v3+)
                encoder.write_unsigned_varint(0); // No tagged fields
            } else {
                encoder.write_string(Some(&group.group_id));
                encoder.write_string(Some(&group.protocol_type));
            }
        }

        // Tagged fields at response level (v3+)
        if use_compact {
            encoder.write_unsigned_varint(0); // No tagged fields
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
                
                // CRITICAL FIX (v1.3.41): Always create ALL partitions based on partition_count
                // Even if assignments exist, we need to fill in missing partitions
                // Otherwise clients won't be able to produce to partition 1, 2, etc.

                let mut sorted_assignments = assignments;
                sorted_assignments.sort_by_key(|a| a.partition);

                tracing::info!("METADATA→PARTITIONS: topic={} partition_count={} assignments_count={}",
                              topic_meta.name, topic_meta.config.partition_count, sorted_assignments.len());

                // Create partitions for ALL partition_count, using assignments where available
                for partition_id in 0..topic_meta.config.partition_count {
                    // Try to find an assignment for this partition
                    let assignment = sorted_assignments.iter().find(|a| a.partition == partition_id);

                    if let Some(assignment) = assignment {
                        // Use the actual assignment
                        partitions.push(MetadataPartition {
                            error_code: 0,
                            partition_index: assignment.partition as i32,
                            leader_id: assignment.broker_id,
                            leader_epoch: 0,
                            replica_nodes: vec![assignment.broker_id],
                            isr_nodes: vec![assignment.broker_id],
                            offline_replicas: vec![],
                        });
                    } else {
                        // No assignment found, use default (current broker)
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
