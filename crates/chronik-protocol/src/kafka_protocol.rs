//! Clean Kafka protocol implementation without external dependencies.

use bytes::{Buf, BufMut, BytesMut};
use chronik_common::{Result, Error};

/// Kafka error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i16)]
pub enum ErrorCode {
    None = 0,
    OffsetOutOfRange = 1,
    CorruptMessage = 2,
    UnknownTopicOrPartition = 3,
    InvalidFetchSize = 4,
    LeaderNotAvailable = 5,
    NotLeaderForPartition = 6,
    RequestTimedOut = 7,
    BrokerNotAvailable = 8,
    ReplicaNotAvailable = 9,
    MessageTooLarge = 10,
    StaleControllerEpoch = 11,
    OffsetMetadataTooLarge = 12,
    NetworkException = 13,
    CoordinatorLoadInProgress = 14,
    CoordinatorNotAvailable = 15,
    NotCoordinator = 16,
    InvalidTopicException = 17,
    RecordListTooLarge = 18,
    NotEnoughReplicas = 19,
    NotEnoughReplicasAfterAppend = 20,
    InvalidRequiredAcks = 21,
    IllegalGeneration = 22,
    InconsistentGroupProtocol = 23,
    InvalidGroupId = 24,
    UnknownMemberId = 25,
    InvalidSessionTimeout = 26,
    RebalanceInProgress = 27,
    InvalidCommitOffsetSize = 28,
    TopicAuthorizationFailed = 29,
    GroupAuthorizationFailed = 30,
    ClusterAuthorizationFailed = 31,
    InvalidTimestamp = 32,
    UnsupportedSaslMechanism = 33,
    IllegalSaslState = 34,
    UnsupportedVersion = 35,
    TopicAlreadyExists = 36,
    InvalidPartitions = 37,
    InvalidReplicationFactor = 38,
    InvalidReplicaAssignment = 39,
    InvalidConfig = 40,
    NotController = 41,
    InvalidRequest = 42,
    UnsupportedForMessageFormat = 43,
    PolicyViolation = 44,
    OutOfOrderSequenceNumber = 45,
    DuplicateSequenceNumber = 46,
    InvalidProducerEpoch = 47,
    InvalidTxnState = 48,
    InvalidProducerIdMapping = 49,
    InvalidTransactionTimeout = 50,
    ConcurrentTransactions = 51,
    TransactionCoordinatorFenced = 52,
    TransactionalIdAuthorizationFailed = 53,
    SecurityDisabled = 54,
    OperationNotAttempted = 55,
    KafkaStorageError = 56,
    LogDirNotFound = 57,
    SaslAuthenticationFailed = 58,
    UnknownProducerId = 59,
    ReassignmentInProgress = 60,
    DelegationTokenAuthDisabled = 61,
    DelegationTokenNotFound = 62,
    DelegationTokenOwnerMismatch = 63,
    DelegationTokenRequestNotAllowed = 64,
    DelegationTokenAuthorizationFailed = 65,
    DelegationTokenExpired = 66,
    InvalidPrincipalType = 67,
    NonEmptyGroup = 68,
    GroupIdNotFound = 69,
    FetchSessionIdNotFound = 70,
    InvalidFetchSessionEpoch = 71,
    ListenerNotFound = 72,
    TopicDeletionDisabled = 73,
    FencedLeaderEpoch = 74,
    UnknownLeaderEpoch = 75,
    UnsupportedCompressionType = 76,
    StaleBrokerEpoch = 77,
    OffsetNotAvailable = 78,
    MemberIdRequired = 79,
    PreferredLeaderNotAvailable = 80,
    GroupMaxSizeReached = 81,
    FencedInstanceId = 82,
    EligibleLeadersNotAvailable = 83,
    ElectionNotNeeded = 84,
    NoReassignmentInProgress = 85,
    GroupSubscribedToTopic = 86,
    InvalidRecord = 87,
    UnstableOffsetCommit = 88,
}

impl ErrorCode {
    /// Get error code value
    pub fn code(&self) -> i16 {
        *self as i16
    }
}

/// API keys
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i16)]
pub enum ApiKey {
    Produce = 0,
    Fetch = 1,
    ListOffsets = 2,
    Metadata = 3,
    LeaderAndIsr = 4,
    StopReplica = 5,
    UpdateMetadata = 6,
    ControlledShutdown = 7,
    OffsetCommit = 8,
    OffsetFetch = 9,
    FindCoordinator = 10,
    JoinGroup = 11,
    Heartbeat = 12,
    LeaveGroup = 13,
    SyncGroup = 14,
    DescribeGroups = 15,
    ListGroups = 16,
    SaslHandshake = 17,
    ApiVersions = 18,
    CreateTopics = 19,
    DeleteTopics = 20,
    DeleteRecords = 21,
    InitProducerId = 22,
    OffsetForLeaderEpoch = 23,
    AddPartitionsToTxn = 24,
    AddOffsetsToTxn = 25,
    EndTxn = 26,
    WriteTxnMarkers = 27,
    TxnOffsetCommit = 28,
    DescribeAcls = 29,
    CreateAcls = 30,
    DeleteAcls = 31,
    DescribeConfigs = 32,
    AlterConfigs = 33,
    AlterReplicaLogDirs = 34,
    DescribeLogDirs = 35,
    SaslAuthenticate = 36,
    CreatePartitions = 37,
    CreateDelegationToken = 38,
    RenewDelegationToken = 39,
    ExpireDelegationToken = 40,
    DescribeDelegationToken = 41,
    DeleteGroups = 42,
    ElectLeaders = 43,
    IncrementalAlterConfigs = 44,
    AlterPartitionReassignments = 45,
    ListPartitionReassignments = 46,
    OffsetDelete = 47,
}

impl ApiKey {
    /// Try to create from i16
    pub fn from_i16(value: i16) -> Option<Self> {
        match value {
            0 => Some(ApiKey::Produce),
            1 => Some(ApiKey::Fetch),
            2 => Some(ApiKey::ListOffsets),
            3 => Some(ApiKey::Metadata),
            4 => Some(ApiKey::LeaderAndIsr),
            5 => Some(ApiKey::StopReplica),
            6 => Some(ApiKey::UpdateMetadata),
            7 => Some(ApiKey::ControlledShutdown),
            8 => Some(ApiKey::OffsetCommit),
            9 => Some(ApiKey::OffsetFetch),
            10 => Some(ApiKey::FindCoordinator),
            11 => Some(ApiKey::JoinGroup),
            12 => Some(ApiKey::Heartbeat),
            13 => Some(ApiKey::LeaveGroup),
            14 => Some(ApiKey::SyncGroup),
            15 => Some(ApiKey::DescribeGroups),
            16 => Some(ApiKey::ListGroups),
            17 => Some(ApiKey::SaslHandshake),
            18 => Some(ApiKey::ApiVersions),
            19 => Some(ApiKey::CreateTopics),
            20 => Some(ApiKey::DeleteTopics),
            21 => Some(ApiKey::DeleteRecords),
            22 => Some(ApiKey::InitProducerId),
            23 => Some(ApiKey::OffsetForLeaderEpoch),
            24 => Some(ApiKey::AddPartitionsToTxn),
            25 => Some(ApiKey::AddOffsetsToTxn),
            26 => Some(ApiKey::EndTxn),
            27 => Some(ApiKey::WriteTxnMarkers),
            28 => Some(ApiKey::TxnOffsetCommit),
            29 => Some(ApiKey::DescribeAcls),
            30 => Some(ApiKey::CreateAcls),
            31 => Some(ApiKey::DeleteAcls),
            32 => Some(ApiKey::DescribeConfigs),
            33 => Some(ApiKey::AlterConfigs),
            34 => Some(ApiKey::AlterReplicaLogDirs),
            35 => Some(ApiKey::DescribeLogDirs),
            36 => Some(ApiKey::SaslAuthenticate),
            37 => Some(ApiKey::CreatePartitions),
            38 => Some(ApiKey::CreateDelegationToken),
            39 => Some(ApiKey::RenewDelegationToken),
            40 => Some(ApiKey::ExpireDelegationToken),
            41 => Some(ApiKey::DescribeDelegationToken),
            42 => Some(ApiKey::DeleteGroups),
            43 => Some(ApiKey::ElectLeaders),
            44 => Some(ApiKey::IncrementalAlterConfigs),
            45 => Some(ApiKey::AlterPartitionReassignments),
            46 => Some(ApiKey::ListPartitionReassignments),
            47 => Some(ApiKey::OffsetDelete),
            _ => None,
        }
    }
}

/// Protocol utilities
pub struct ProtocolUtils;

impl ProtocolUtils {
    /// Read a compact string
    pub fn read_compact_string(buf: &mut dyn Buf) -> Result<Option<String>> {
        let len = Self::read_unsigned_varint(buf)? as i32 - 1;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for compact string of length {}", len)));
        }
        
        let mut bytes = vec![0u8; len];
        buf.copy_to_slice(&mut bytes);
        
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| Error::Protocol(format!("Invalid UTF-8 in string: {}", e)))
    }
    
    /// Write a compact string
    pub fn write_compact_string(buf: &mut dyn BufMut, value: Option<&str>) {
        match value {
            Some(s) => {
                Self::write_unsigned_varint(buf, (s.len() + 1) as u32);
                buf.put_slice(s.as_bytes());
            }
            None => {
                Self::write_unsigned_varint(buf, 0);
            }
        }
    }
    
    /// Read a compact array length
    pub fn read_compact_array_len(buf: &mut dyn Buf) -> Result<usize> {
        let len = Self::read_unsigned_varint(buf)? as i32 - 1;
        if len < 0 {
            Ok(0)
        } else {
            Ok(len as usize)
        }
    }
    
    /// Write a compact array length
    pub fn write_compact_array_len(buf: &mut dyn BufMut, len: usize) {
        Self::write_unsigned_varint(buf, (len + 1) as u32);
    }
    
    /// Read an unsigned varint
    pub fn read_unsigned_varint(buf: &mut dyn Buf) -> Result<u32> {
        let mut value = 0u32;
        let mut i = 0;
        
        loop {
            if !buf.has_remaining() {
                return Err(Error::Protocol("Incomplete varint".into()));
            }
            
            let byte = buf.get_u8();
            value |= ((byte & 0x7F) as u32) << (i * 7);
            
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            
            i += 1;
            if i >= 5 {
                return Err(Error::Protocol("Varint too long".into()));
            }
        }
    }
    
    /// Write an unsigned varint
    pub fn write_unsigned_varint(buf: &mut dyn BufMut, mut value: u32) {
        while (value & !0x7F) != 0 {
            buf.put_u8((value & 0x7F) as u8 | 0x80);
            value >>= 7;
        }
        buf.put_u8(value as u8);
    }
    
    /// Read tagged fields (skip for now)
    pub fn read_tagged_fields(buf: &mut dyn Buf) -> Result<()> {
        let num_fields = Self::read_unsigned_varint(buf)?;
        for _ in 0..num_fields {
            let _tag = Self::read_unsigned_varint(buf)?;
            let size = Self::read_unsigned_varint(buf)?;
            buf.advance(size as usize);
        }
        Ok(())
    }
    
    /// Write empty tagged fields
    pub fn write_tagged_fields(buf: &mut dyn BufMut) {
        Self::write_unsigned_varint(buf, 0);
    }
}

/// Request header with flexible versions support
#[derive(Debug, Clone)]
pub struct RequestHeader {
    pub api_key: ApiKey,
    pub api_version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
}

impl RequestHeader {
    /// Decode request header
    pub fn decode(buf: &mut dyn Buf, flexible_version: bool) -> Result<Self> {
        let api_key_raw = buf.get_i16();
        let api_key = ApiKey::from_i16(api_key_raw)
            .ok_or_else(|| Error::Protocol(format!("Unknown API key: {}", api_key_raw)))?;
        
        let api_version = buf.get_i16();
        let correlation_id = buf.get_i32();
        
        let client_id = if flexible_version {
            ProtocolUtils::read_compact_string(buf)?
        } else {
            // Standard string
            let len = buf.get_i16();
            if len < 0 {
                None
            } else {
                let mut bytes = vec![0u8; len as usize];
                buf.copy_to_slice(&mut bytes);
                Some(String::from_utf8(bytes)
                    .map_err(|e| Error::Protocol(format!("Invalid UTF-8 in client_id: {}", e)))?)
            }
        };
        
        if flexible_version {
            ProtocolUtils::read_tagged_fields(buf)?;
        }
        
        Ok(RequestHeader {
            api_key,
            api_version,
            correlation_id,
            client_id,
        })
    }
    
    /// Encode request header
    pub fn encode(&self, buf: &mut dyn BufMut, flexible_version: bool) {
        buf.put_i16(self.api_key as i16);
        buf.put_i16(self.api_version);
        buf.put_i32(self.correlation_id);
        
        if flexible_version {
            ProtocolUtils::write_compact_string(buf, self.client_id.as_deref());
            ProtocolUtils::write_tagged_fields(buf);
        } else {
            // Standard string
            match &self.client_id {
                Some(id) => {
                    buf.put_i16(id.len() as i16);
                    buf.put_slice(id.as_bytes());
                }
                None => {
                    buf.put_i16(-1);
                }
            }
        }
    }
}

/// Response header
#[derive(Debug, Clone)]
pub struct ResponseHeader {
    pub correlation_id: i32,
}

impl ResponseHeader {
    /// Decode response header
    pub fn decode(buf: &mut dyn Buf, flexible_version: bool) -> Result<Self> {
        let correlation_id = buf.get_i32();
        
        if flexible_version {
            ProtocolUtils::read_tagged_fields(buf)?;
        }
        
        Ok(ResponseHeader { correlation_id })
    }
    
    /// Encode response header
    pub fn encode(&self, buf: &mut dyn BufMut, flexible_version: bool) {
        buf.put_i32(self.correlation_id);
        
        if flexible_version {
            ProtocolUtils::write_tagged_fields(buf);
        }
    }
}

/// API version info
#[derive(Debug, Clone)]
pub struct ApiVersion {
    pub api_key: i16,
    pub min_version: i16,
    pub max_version: i16,
}

/// Protocol message encoder/decoder
pub struct ProtocolCodec;

impl ProtocolCodec {
    /// Encode an ApiVersions response
    pub fn encode_api_versions_response(
        buf: &mut BytesMut,
        correlation_id: i32,
        api_versions: &[ApiVersion],
        version: i16,
    ) -> Result<()> {
        let flexible = version >= 3;
        
        // Response header
        let header = ResponseHeader { correlation_id };
        header.encode(buf, flexible);
        
        // CRITICAL: For v0 protocol, field order matters!
        // v0: api_versions array comes BEFORE error_code
        // v1+: error_code comes first (standard field ordering)
        
        if version == 0 {
            // v0 field order: api_versions array, then error_code
            
            // API versions array first
            buf.put_i32(api_versions.len() as i32);
            
            for api in api_versions {
                buf.put_i16(api.api_key);
                buf.put_i16(api.min_version);
                buf.put_i16(api.max_version);
            }
            
            // Then error code
            buf.put_i16(ErrorCode::None.code());
        } else {
            // v1+ field order: throttle_time, error_code, then api_versions array
            
            // Throttle time (v1+) - MUST come first!
            buf.put_i32(0);
            
            // Error code
            buf.put_i16(ErrorCode::None.code());
            
            // API versions array
            if flexible {
                ProtocolUtils::write_compact_array_len(buf, api_versions.len());
            } else {
                buf.put_i32(api_versions.len() as i32);
            }
            
            for api in api_versions {
                buf.put_i16(api.api_key);
                // IMPORTANT: librdkafka always reads min/max as INT16, regardless of version
                // Even though Kafka protocol v3+ specifies INT8, we use INT16 for compatibility
                buf.put_i16(api.min_version);
                buf.put_i16(api.max_version);
                
                if flexible {
                    ProtocolUtils::write_tagged_fields(buf);
                }
            }
            
            if flexible {
                ProtocolUtils::write_tagged_fields(buf);
                // Write throttle_time_ms (required in v3+)
                buf.put_i32(0);
            }
        }
        
        Ok(())
    }
    
    /// Encode a Metadata response
    pub fn encode_metadata_response(
        buf: &mut BytesMut,
        correlation_id: i32,
        cluster_id: &str,
        controller_id: i32,
        brokers: &[(i32, String, i32)], // (node_id, host, port)
        topics: &[TopicMetadata],
        version: i16,
    ) -> Result<()> {
        let flexible = version >= 9;
        
        // Response header
        let header = ResponseHeader { correlation_id };
        header.encode(buf, flexible);
        
        // Throttle time (v3+)
        if version >= 3 {
            buf.put_i32(0);
        }
        
        // Brokers array
        if flexible {
            ProtocolUtils::write_compact_array_len(buf, brokers.len());
        } else {
            buf.put_i32(brokers.len() as i32);
        }
        
        for (node_id, host, port) in brokers {
            buf.put_i32(*node_id);
            
            if flexible {
                ProtocolUtils::write_compact_string(buf, Some(host));
            } else {
                buf.put_i16(host.len() as i16);
                buf.put_slice(host.as_bytes());
            }
            
            buf.put_i32(*port);
            
            // Rack (v1+)
            if version >= 1 {
                if flexible {
                    ProtocolUtils::write_compact_string(buf, None);
                } else {
                    buf.put_i16(-1);
                }
            }
            
            if flexible {
                ProtocolUtils::write_tagged_fields(buf);
            }
        }
        
        // Cluster ID (v2+)
        if version >= 2 {
            if flexible {
                ProtocolUtils::write_compact_string(buf, Some(cluster_id));
            } else {
                buf.put_i16(cluster_id.len() as i16);
                buf.put_slice(cluster_id.as_bytes());
            }
        }
        
        // Controller ID (v1+)
        if version >= 1 {
            buf.put_i32(controller_id);
        }
        
        // Topics array
        if flexible {
            ProtocolUtils::write_compact_array_len(buf, topics.len());
        } else {
            buf.put_i32(topics.len() as i32);
        }
        
        for topic in topics {
            // Error code
            buf.put_i16(ErrorCode::None.code());
            
            // Topic name
            if flexible {
                ProtocolUtils::write_compact_string(buf, Some(&topic.name));
            } else {
                buf.put_i16(topic.name.len() as i16);
                buf.put_slice(topic.name.as_bytes());
            }
            
            // Is internal (v1+)
            if version >= 1 {
                buf.put_u8(if topic.is_internal { 1 } else { 0 });
            }
            
            // Partitions array
            if flexible {
                ProtocolUtils::write_compact_array_len(buf, topic.partitions.len());
            } else {
                buf.put_i32(topic.partitions.len() as i32);
            }
            
            for partition in &topic.partitions {
                // Error code
                buf.put_i16(ErrorCode::None.code());
                
                // Partition index
                buf.put_i32(partition.id);
                
                // Leader
                buf.put_i32(partition.leader);
                
                // Leader epoch (v7+)
                if version >= 7 {
                    buf.put_i32(-1);
                }
                
                // Replicas
                if flexible {
                    ProtocolUtils::write_compact_array_len(buf, partition.replicas.len());
                } else {
                    buf.put_i32(partition.replicas.len() as i32);
                }
                for replica in &partition.replicas {
                    buf.put_i32(*replica);
                }
                
                // ISR
                if flexible {
                    ProtocolUtils::write_compact_array_len(buf, partition.isr.len());
                } else {
                    buf.put_i32(partition.isr.len() as i32);
                }
                for isr in &partition.isr {
                    buf.put_i32(*isr);
                }
                
                // Offline replicas (v5+)
                if version >= 5 {
                    if flexible {
                        ProtocolUtils::write_compact_array_len(buf, partition.offline_replicas.len());
                    } else {
                        buf.put_i32(partition.offline_replicas.len() as i32);
                    }
                    for offline in &partition.offline_replicas {
                        buf.put_i32(*offline);
                    }
                }
                
                if flexible {
                    ProtocolUtils::write_tagged_fields(buf);
                }
            }
            
            // Topic authorized operations (v8+)
            if version >= 8 {
                buf.put_i32(-2147483648); // INT32_MIN = no auth operations
            }
            
            if flexible {
                ProtocolUtils::write_tagged_fields(buf);
            }
        }
        
        // Cluster authorized operations (v8+)
        if version >= 8 && version < 10 {
            buf.put_i32(-2147483648); // INT32_MIN = no auth operations
        }
        
        if flexible {
            ProtocolUtils::write_tagged_fields(buf);
        }
        
        Ok(())
    }
}

/// Topic metadata for responses
#[derive(Debug, Clone)]
pub struct TopicMetadata {
    pub name: String,
    pub is_internal: bool,
    pub partitions: Vec<PartitionMetadata>,
}

/// Partition metadata for responses
#[derive(Debug, Clone)]
pub struct PartitionMetadata {
    pub id: i32,
    pub leader: i32,
    pub replicas: Vec<i32>,
    pub isr: Vec<i32>,
    pub offline_replicas: Vec<i32>,
}

/// Get default supported API versions
pub fn get_supported_apis() -> Vec<ApiVersion> {
    vec![
        ApiVersion { api_key: 0, min_version: 0, max_version: 9 },   // Produce
        ApiVersion { api_key: 1, min_version: 0, max_version: 13 },  // Fetch
        ApiVersion { api_key: 2, min_version: 0, max_version: 7 },   // ListOffsets
        ApiVersion { api_key: 3, min_version: 0, max_version: 12 },  // Metadata
        ApiVersion { api_key: 8, min_version: 0, max_version: 8 },   // OffsetCommit
        ApiVersion { api_key: 9, min_version: 0, max_version: 8 },   // OffsetFetch
        ApiVersion { api_key: 10, min_version: 0, max_version: 4 },  // FindCoordinator
        ApiVersion { api_key: 11, min_version: 0, max_version: 9 },  // JoinGroup
        ApiVersion { api_key: 12, min_version: 0, max_version: 4 },  // Heartbeat
        ApiVersion { api_key: 13, min_version: 0, max_version: 5 },  // LeaveGroup
        ApiVersion { api_key: 14, min_version: 0, max_version: 5 },  // SyncGroup
        ApiVersion { api_key: 15, min_version: 0, max_version: 5 },  // DescribeGroups
        ApiVersion { api_key: 16, min_version: 0, max_version: 4 },  // ListGroups
        ApiVersion { api_key: 17, min_version: 0, max_version: 1 },  // SaslHandshake
        ApiVersion { api_key: 18, min_version: 0, max_version: 3 },  // ApiVersions
        ApiVersion { api_key: 19, min_version: 0, max_version: 7 },  // CreateTopics
        ApiVersion { api_key: 20, min_version: 0, max_version: 6 },  // DeleteTopics
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_varint_encoding() {
        let mut buf = BytesMut::new();
        
        ProtocolUtils::write_unsigned_varint(&mut buf, 0);
        ProtocolUtils::write_unsigned_varint(&mut buf, 127);
        ProtocolUtils::write_unsigned_varint(&mut buf, 128);
        ProtocolUtils::write_unsigned_varint(&mut buf, 16383);
        ProtocolUtils::write_unsigned_varint(&mut buf, 16384);
        
        let mut frozen = buf.freeze();
        assert_eq!(ProtocolUtils::read_unsigned_varint(&mut frozen).unwrap(), 0);
        assert_eq!(ProtocolUtils::read_unsigned_varint(&mut frozen).unwrap(), 127);
        assert_eq!(ProtocolUtils::read_unsigned_varint(&mut frozen).unwrap(), 128);
        assert_eq!(ProtocolUtils::read_unsigned_varint(&mut frozen).unwrap(), 16383);
        assert_eq!(ProtocolUtils::read_unsigned_varint(&mut frozen).unwrap(), 16384);
    }
    
    #[test]
    fn test_compact_string() {
        let mut buf = BytesMut::new();
        
        ProtocolUtils::write_compact_string(&mut buf, Some("hello"));
        ProtocolUtils::write_compact_string(&mut buf, None);
        ProtocolUtils::write_compact_string(&mut buf, Some(""));
        
        let mut frozen = buf.freeze();
        assert_eq!(ProtocolUtils::read_compact_string(&mut frozen).unwrap(), Some("hello".to_string()));
        assert_eq!(ProtocolUtils::read_compact_string(&mut frozen).unwrap(), None);
        assert_eq!(ProtocolUtils::read_compact_string(&mut frozen).unwrap(), Some("".to_string()));
    }
    
    #[test]
    fn test_api_versions_response() {
        let mut buf = BytesMut::new();
        let apis = get_supported_apis();
        
        ProtocolCodec::encode_api_versions_response(&mut buf, 123, &apis, 3).unwrap();
        
        assert!(!buf.is_empty());
        
        // Verify header
        let mut frozen = buf.freeze();
        assert_eq!(frozen.get_i32(), 123); // correlation_id
    }
}