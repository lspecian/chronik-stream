//! Kafka wire protocol parser.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use chronik_common::{Result, Error};
use std::collections::HashMap;

/// Kafka API keys
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
    DescribeClientQuotas = 48,
    AlterClientQuotas = 49,
    DescribeUserScramCredentials = 50,
    AlterUserScramCredentials = 51,
    // Note: Skipping APIs 52-55 to match CP Kafka (they don't support these KRaft APIs)
    // Vote = 52,
    // BeginQuorumEpoch = 53,
    // EndQuorumEpoch = 54,
    // DescribeQuorum = 55,
    AlterPartition = 56,
    UpdateFeatures = 57,
    Envelope = 58,
    // Note: Skipping API 59 to match CP Kafka
    // FetchSnapshot = 59,
    DescribeCluster = 60,
    DescribeProducers = 61,
    // Note: Skipping APIs 62-64 to match CP Kafka (more KRaft APIs)
    // BrokerRegistration = 62,
    // BrokerHeartbeat = 63,
    // UnregisterBroker = 64,
    DescribeTransactions = 65,
    ListTransactions = 66,
    AllocateProducerIds = 67,
    // Note: Skipping API 68 to match CP Kafka
    // ConsumerGroupHeartbeat = 68,
}

impl ApiKey {
    /// Try to create an ApiKey from an i16
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
            48 => Some(ApiKey::DescribeClientQuotas),
            49 => Some(ApiKey::AlterClientQuotas),
            50 => Some(ApiKey::DescribeUserScramCredentials),
            51 => Some(ApiKey::AlterUserScramCredentials),
            // 52 => Some(ApiKey::Vote),  // Skipped to match CP Kafka
            // 53 => Some(ApiKey::BeginQuorumEpoch),  // Skipped to match CP Kafka
            // 54 => Some(ApiKey::EndQuorumEpoch),  // Skipped to match CP Kafka
            // 55 => Some(ApiKey::DescribeQuorum),  // Skipped to match CP Kafka
            56 => Some(ApiKey::AlterPartition),
            57 => Some(ApiKey::UpdateFeatures),
            58 => Some(ApiKey::Envelope),
            // 59 => Some(ApiKey::FetchSnapshot),  // Skipped to match CP Kafka
            60 => Some(ApiKey::DescribeCluster),
            61 => Some(ApiKey::DescribeProducers),
            // 62 => Some(ApiKey::BrokerRegistration),  // Skipped to match CP Kafka
            // 63 => Some(ApiKey::BrokerHeartbeat),  // Skipped to match CP Kafka
            // 64 => Some(ApiKey::UnregisterBroker),  // Skipped to match CP Kafka
            65 => Some(ApiKey::DescribeTransactions),
            66 => Some(ApiKey::ListTransactions),
            67 => Some(ApiKey::AllocateProducerIds),
            // 68 => Some(ApiKey::ConsumerGroupHeartbeat),  // Skipped to match CP Kafka
            _ => None,
        }
    }
}

/// Kafka request header
#[derive(Debug, Clone)]
pub struct RequestHeader {
    pub api_key: ApiKey,
    pub api_version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
}

/// Kafka response header
#[derive(Debug, Clone)]
pub struct ResponseHeader {
    pub correlation_id: i32,
}

/// Version range for an API
#[derive(Debug, Clone)]
pub struct VersionRange {
    pub min: i16,
    pub max: i16,
}

/// Protocol decoder for reading Kafka protocol primitives
pub struct Decoder<'a> {
    buf: &'a mut Bytes,
}

impl<'a> Decoder<'a> {
    /// Create a new decoder
    pub fn new(buf: &'a mut Bytes) -> Self {
        Self { buf }
    }
    
    /// Check if buffer has remaining bytes
    pub fn has_remaining(&self) -> bool {
        self.buf.has_remaining()
    }
    
    /// Get number of remaining bytes
    pub fn remaining(&self) -> usize {
        self.buf.remaining()
    }

    /// Skip n bytes
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.buf.remaining() < n {
            return Err(Error::Protocol(format!("Cannot skip {} bytes, only {} remaining", n, self.buf.remaining())));
        }
        self.buf.advance(n);
        Ok(())
    }
    
    /// Read a boolean
    pub fn read_bool(&mut self) -> Result<bool> {
        if self.buf.remaining() < 1 {
            return Err(Error::Protocol("Not enough bytes for bool".into()));
        }
        Ok(self.buf.get_u8() != 0)
    }
    
    /// Read an i8
    pub fn read_i8(&mut self) -> Result<i8> {
        if self.buf.remaining() < 1 {
            return Err(Error::Protocol("Not enough bytes for i8".into()));
        }
        Ok(self.buf.get_i8())
    }
    
    /// Read an i16
    pub fn read_i16(&mut self) -> Result<i16> {
        if self.buf.remaining() < 2 {
            return Err(Error::Protocol("Not enough bytes for i16".into()));
        }
        Ok(self.buf.get_i16())
    }
    
    /// Read an i32
    pub fn read_i32(&mut self) -> Result<i32> {
        if self.buf.remaining() < 4 {
            return Err(Error::Protocol("Not enough bytes for i32".into()));
        }
        Ok(self.buf.get_i32())
    }
    
    /// Read an i64
    pub fn read_i64(&mut self) -> Result<i64> {
        if self.buf.remaining() < 8 {
            return Err(Error::Protocol("Not enough bytes for i64".into()));
        }
        Ok(self.buf.get_i64())
    }
    
    /// Read a string (null = -1 length)
    pub fn read_string(&mut self) -> Result<Option<String>> {
        let len = self.read_i16()?;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if self.buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for string of length {}", len)));
        }
        
        let mut bytes = vec![0u8; len];
        self.buf.copy_to_slice(&mut bytes);
        
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| Error::Protocol(format!("Invalid UTF-8 in string: {}", e)))
    }
    
    /// Read a byte array (null = -1 length)
    pub fn read_bytes(&mut self) -> Result<Option<Bytes>> {
        let len = self.read_i32()?;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if self.buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for byte array of length {}", len)));
        }
        
        Ok(Some(self.buf.copy_to_bytes(len)))
    }
    
    /// Read a compact string (uses varint length)
    pub fn read_compact_string(&mut self) -> Result<Option<String>> {
        let len = self.read_unsigned_varint()? as i32 - 1;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if self.buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for compact string of length {}", len)));
        }
        
        let mut bytes = vec![0u8; len];
        self.buf.copy_to_slice(&mut bytes);
        
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| Error::Protocol(format!("Invalid UTF-8 in string: {}", e)))
    }
    
    /// Read compact bytes (uses varint length)
    pub fn read_compact_bytes(&mut self) -> Result<Option<Bytes>> {
        let len = self.read_unsigned_varint()? as i32 - 1;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if self.buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for compact byte array of length {}", len)));
        }
        
        Ok(Some(self.buf.copy_to_bytes(len)))
    }
    
    /// Advance the buffer by n bytes
    pub fn advance(&mut self, n: usize) -> Result<()> {
        if self.buf.remaining() < n {
            // Log the buffer contents for debugging
            let preview_len = self.buf.remaining().min(100);
            let preview_bytes: Vec<u8> = self.buf.chunk()[..preview_len].to_vec();
            tracing::error!(
                "Cannot advance {} bytes, only {} remaining. Buffer preview (first {} bytes): {:02x?}",
                n, self.buf.remaining(), preview_len, preview_bytes
            );
            return Err(Error::Protocol(format!(
                "Cannot advance {} bytes, only {} remaining",
                n, self.buf.remaining()
            )));
        }
        self.buf.advance(n);
        Ok(())
    }
    
    /// Read an unsigned varint
    pub fn read_unsigned_varint(&mut self) -> Result<u32> {
        let mut value = 0u32;
        let mut i = 0;
        let mut bytes_read = Vec::new();

        loop {
            if !self.buf.has_remaining() {
                tracing::error!("Incomplete varint after {} bytes: {:02x?}", bytes_read.len(), bytes_read);
                return Err(Error::Protocol("Incomplete varint".into()));
            }

            let byte = self.buf.get_u8();
            bytes_read.push(byte);
            value |= ((byte & 0x7F) as u32) << (i * 7);

            if byte & 0x80 == 0 {
                // Only log for suspiciously large values
                if value > 10000 {
                    tracing::warn!("Read varint value {} from bytes {:02x?}", value, bytes_read);
                }
                return Ok(value);
            }

            i += 1;
            if i >= 5 {
                tracing::error!("Varint too long, bytes read: {:02x?}, partial value: {}", bytes_read, value);
                return Err(Error::Protocol("Varint too long".into()));
            }
        }
    }
}

/// Protocol encoder for writing Kafka protocol primitives
pub struct Encoder<'a> {
    buf: &'a mut BytesMut,
}

impl<'a> Encoder<'a> {
    /// Create a new encoder
    pub fn new(buf: &'a mut BytesMut) -> Self {
        Self { buf }
    }
    
    /// Get a debug view of the buffer
    pub fn debug_buffer(&self) -> Vec<u8> {
        self.buf.to_vec()
    }

    /// Get the current position in the buffer
    pub fn position(&self) -> usize {
        self.buf.len()
    }
    
    /// Write a boolean
    pub fn write_bool(&mut self, value: bool) {
        self.buf.put_u8(if value { 1 } else { 0 });
    }
    
    /// Write an i8
    pub fn write_i8(&mut self, value: i8) {
        self.buf.put_i8(value);
    }
    
    /// Write an i16
    pub fn write_i16(&mut self, value: i16) {
        self.buf.put_i16(value);
    }
    
    /// Write an i32
    pub fn write_i32(&mut self, value: i32) {
        self.buf.put_i32(value);
    }
    
    /// Write an i64
    pub fn write_i64(&mut self, value: i64) {
        self.buf.put_i64(value);
    }
    
    /// Write a nullable string (null = None)
    pub fn write_string(&mut self, value: Option<&str>) {
        match value {
            Some(s) => {
                self.write_i16(s.len() as i16);
                self.buf.put_slice(s.as_bytes());
            }
            None => {
                self.write_i16(-1);
            }
        }
    }

    /// Write a non-nullable string (must not be None)
    /// This is used for fields that cannot be null in the Kafka protocol
    pub fn write_non_nullable_string(&mut self, value: &str) {
        self.write_i16(value.len() as i16);
        self.buf.put_slice(value.as_bytes());
    }
    
    /// Write a byte array (null = None)
    pub fn write_bytes(&mut self, value: Option<&[u8]>) {
        match value {
            Some(bytes) => {
                self.write_i32(bytes.len() as i32);
                self.buf.put_slice(bytes);
            }
            None => {
                self.write_i32(-1);
            }
        }
    }
    
    /// Write a compact string (uses varint length)
    pub fn write_compact_string(&mut self, value: Option<&str>) {
        match value {
            Some(s) => {
                self.write_unsigned_varint((s.len() + 1) as u32);
                self.buf.put_slice(s.as_bytes());
            }
            None => {
                self.write_unsigned_varint(0);
            }
        }
    }
    
    /// Write compact bytes (uses varint length)
    pub fn write_compact_bytes(&mut self, value: Option<&[u8]>) {
        match value {
            Some(bytes) => {
                self.write_unsigned_varint((bytes.len() + 1) as u32);
                self.buf.put_slice(bytes);
            }
            None => {
                self.write_unsigned_varint(0);
            }
        }
    }
    
    /// Write an unsigned varint
    pub fn write_unsigned_varint(&mut self, mut value: u32) {
        let original_value = value;
        let start_pos = self.buf.len();
        while (value & !0x7F) != 0 {
            self.buf.put_u8((value & 0x7F) as u8 | 0x80);
            value >>= 7;
        }
        self.buf.put_u8(value as u8);
    }
    
    /// Write a compact array length (uses varint with +1 offset)
    pub fn write_compact_array_len(&mut self, len: usize) {
        let start_pos = self.buf.len();
        self.write_unsigned_varint((len + 1) as u32);
        let end_pos = self.buf.len();
        tracing::trace!("write_compact_array_len: wrote {} bytes: {:02x?}",
                 end_pos - start_pos,
                 &self.buf.as_ref()[start_pos..end_pos]);
    }
    
    /// Write empty tagged fields (always writes 0 for now)
    pub fn write_tagged_fields(&mut self) {
        self.write_unsigned_varint(0);
    }
    
    /// Write raw bytes
    pub fn write_raw_bytes(&mut self, bytes: &[u8]) {
        self.buf.put_slice(bytes);
    }
}

/// Partial request header for cases where API key is unknown
#[derive(Debug, Clone)]
pub struct PartialRequestHeader {
    pub api_key_raw: i16,
    pub api_version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
}

/// Parse a request header from bytes, preserving correlation ID even on error
pub fn parse_request_header_with_correlation(buf: &mut Bytes) -> Result<(RequestHeader, Option<PartialRequestHeader>)> {
    let mut decoder = Decoder::new(buf);
    
    let api_key_raw = decoder.read_i16()?;
    let api_version = decoder.read_i16()?;
    let correlation_id = decoder.read_i32()?;
    
    // Determine if this API+version uses flexible/compact encoding
    // CRITICAL: ApiVersions is SPECIAL - request header is NEVER flexible (always v1)
    // even though the request body IS flexible for v3+. This is for backward compatibility.
    let flexible = if let Some(api_key) = ApiKey::from_i16(api_key_raw) {
        if api_key == ApiKey::ApiVersions {
            tracing::trace!("ApiVersions request header uses non-flexible encoding (spec requirement)");
            false  // ApiVersions NEVER has flexible request header
        } else {
            is_flexible_version(api_key, api_version)
        }
    } else {
        false
    };

    tracing::trace!("Request header: api_key={:?}, version={}, flexible={}", ApiKey::from_i16(api_key_raw), api_version, flexible);
    
    // Read client ID
    // CRITICAL: Despite using flexible header v2, Java client sends client_id as regular STRING (INT16 length)
    // NOT as COMPACT_STRING (varint length). This is inconsistent with the Kafka spec but matches Java behavior.
    // librdkafka does send COMPACT_STRING for flexible versions.
    // For maximum compatibility, we detect which encoding based on the bytes.
    let client_id = if flexible {
        // Peek at first 2 bytes to detect encoding
        if decoder.remaining() >= 2 {
            let first_byte = decoder.buf[0];
            let second_byte = decoder.buf[1];

            // If first byte is 0x00 and second byte >= 0x01, it's likely INT16 string length (Java style)
            // If first byte >= 0x01 and is a reasonable varint, it's likely COMPACT_STRING (librdkafka style)
            if first_byte == 0x00 {
                // Probably Java INT16-length string
                tracing::trace!("Using STRING encoding for client_id (Java client style)");
                decoder.read_string()?
            } else {
                // Probably librdkafka COMPACT_STRING
                tracing::trace!("Using COMPACT_STRING encoding for client_id (librdkafka client style)");
                decoder.read_compact_string()?
            }
        } else {
            // Not enough bytes, try compact string
            decoder.read_compact_string()?
        }
    } else {
        decoder.read_string()?
    };
    
    // Skip tagged fields if flexible
    if flexible {
        let tagged_field_count = decoder.read_unsigned_varint()?;
        tracing::trace!("Skipping {} tagged fields in request header", tagged_field_count);

        // Actually skip the tagged field data
        for i in 0..tagged_field_count {
            let tag_id = decoder.read_unsigned_varint()?;
            let tag_size = decoder.read_unsigned_varint()? as usize;

            // Add safety check for unreasonable tag sizes
            if tag_size > 1_000_000 {
                return Err(Error::Protocol(format!(
                    "Tagged field {} has unreasonable size: {} bytes",
                    tag_id, tag_size
                )));
            }

            tracing::trace!("Skipping tagged field {}: tag_id={}, size={}", i, tag_id, tag_size);
            decoder.advance(tag_size)?;
        }
    }
    
    let partial = PartialRequestHeader {
        api_key_raw,
        api_version,
        correlation_id,
        client_id: client_id.clone(),
    };
    
    match ApiKey::from_i16(api_key_raw) {
        Some(api_key) => Ok((RequestHeader {
            api_key,
            api_version,
            correlation_id,
            client_id,
        }, Some(partial))),
        None => Err(Error::ProtocolWithCorrelation {
            message: format!("Unknown API key: {}", api_key_raw),
            correlation_id,
        }),
    }
}

/// Parse a request header from bytes
pub fn parse_request_header(buf: &mut Bytes) -> Result<RequestHeader> {
    match parse_request_header_with_correlation(buf) {
        Ok((header, _)) => Ok(header),
        Err(e) => Err(e),
    }
}

/// Write a response header to bytes
pub fn write_response_header(buf: &mut BytesMut, header: &ResponseHeader) {
    let mut encoder = Encoder::new(buf);
    encoder.write_i32(header.correlation_id);
}

/// Write a response header to bytes with flexible version support and optional throttle time
pub fn write_response_header_flexible(buf: &mut BytesMut, header: &ResponseHeader, api_key: ApiKey, api_version: i16, throttle_time_ms: Option<i32>) {
    let mut encoder = Encoder::new(buf);
    encoder.write_i32(header.correlation_id);

    // Only write throttle_time_ms if it's provided
    // Note: DescribeCluster v0 does NOT include throttle_time_ms at all
    if let Some(throttle_time) = throttle_time_ms {
        encoder.write_i32(throttle_time);
    }

    // Check if this API version uses flexible versions
    if is_flexible_version(api_key, api_version) {
        // Write empty tagged fields
        encoder.write_unsigned_varint(0);
    }
}

/// Check if an API version uses flexible versions
pub fn is_flexible_version(api_key: ApiKey, api_version: i16) -> bool {
    match api_key {
        ApiKey::Produce => api_version >= 9,
        ApiKey::Fetch => api_version >= 12,
        ApiKey::ListOffsets => api_version >= 6,
        ApiKey::Metadata => api_version >= 9,
        ApiKey::OffsetCommit => api_version >= 8,
        ApiKey::OffsetFetch => api_version >= 6,
        ApiKey::FindCoordinator => api_version >= 3,
        ApiKey::JoinGroup => api_version >= 6,
        ApiKey::Heartbeat => api_version >= 4,
        ApiKey::LeaveGroup => api_version >= 4,
        ApiKey::SyncGroup => api_version >= 4,
        ApiKey::DescribeGroups => api_version >= 5,
        ApiKey::ListGroups => api_version >= 3,
        ApiKey::SaslHandshake => api_version >= 1,
        ApiKey::ApiVersions => api_version >= 3,
        ApiKey::CreateTopics => api_version >= 5,
        ApiKey::DeleteTopics => api_version >= 4,
        ApiKey::DescribeConfigs => api_version >= 4,
        ApiKey::DescribeCluster => api_version >= 1,  // v1 uses flexible encoding
        // Transaction APIs
        ApiKey::InitProducerId => api_version >= 2,  // v2+ uses flexible encoding
        ApiKey::AddPartitionsToTxn => api_version >= 3,  // v3+ uses flexible encoding
        ApiKey::AddOffsetsToTxn => api_version >= 3,  // v3+ uses flexible encoding
        ApiKey::EndTxn => api_version >= 3,  // v3+ uses flexible encoding
        ApiKey::TxnOffsetCommit => api_version >= 3,  // v3+ uses flexible encoding
        // ACL APIs
        ApiKey::DescribeAcls => api_version >= 2,  // v2+ uses flexible encoding
        ApiKey::CreateAcls => api_version >= 2,  // v2+ uses flexible encoding
        ApiKey::DeleteAcls => api_version >= 2,  // v2+ uses flexible encoding
        // All new APIs default to non-flexible for v0
        _ => false,
    }
}

/// Get supported API versions
pub fn supported_api_versions() -> HashMap<ApiKey, VersionRange> {
    let mut versions = HashMap::new();
    
    // Core APIs (fully implemented)
    versions.insert(ApiKey::Produce, VersionRange { min: 0, max: 9 });
    versions.insert(ApiKey::Fetch, VersionRange { min: 0, max: 13 });
    versions.insert(ApiKey::ListOffsets, VersionRange { min: 0, max: 7 });
    versions.insert(ApiKey::Metadata, VersionRange { min: 0, max: 12 });
    versions.insert(ApiKey::OffsetCommit, VersionRange { min: 0, max: 8 });
    versions.insert(ApiKey::OffsetFetch, VersionRange { min: 0, max: 8 });
    versions.insert(ApiKey::FindCoordinator, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::JoinGroup, VersionRange { min: 0, max: 9 });
    versions.insert(ApiKey::Heartbeat, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::LeaveGroup, VersionRange { min: 0, max: 5 });
    versions.insert(ApiKey::SyncGroup, VersionRange { min: 0, max: 5 });
    versions.insert(ApiKey::DescribeGroups, VersionRange { min: 0, max: 5 });
    versions.insert(ApiKey::ListGroups, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::SaslHandshake, VersionRange { min: 0, max: 1 });
    versions.insert(ApiKey::ApiVersions, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::CreateTopics, VersionRange { min: 0, max: 7 });
    versions.insert(ApiKey::DeleteTopics, VersionRange { min: 0, max: 6 });
    versions.insert(ApiKey::DescribeConfigs, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::AlterConfigs, VersionRange { min: 0, max: 2 });
    
    // Broker management APIs (NOT IMPLEMENTED - placeholder for librdkafka compatibility)
    versions.insert(ApiKey::LeaderAndIsr, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::StopReplica, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::UpdateMetadata, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::ControlledShutdown, VersionRange { min: 0, max: 0 });
    
    // Transaction APIs (IMPLEMENTED - full support for transactional producers)
    versions.insert(ApiKey::InitProducerId, VersionRange { min: 0, max: 4 });  // v0-v4 supported
    versions.insert(ApiKey::AddPartitionsToTxn, VersionRange { min: 0, max: 3 });  // v0-v3 supported
    versions.insert(ApiKey::AddOffsetsToTxn, VersionRange { min: 0, max: 3 });  // v0-v3 supported
    versions.insert(ApiKey::EndTxn, VersionRange { min: 0, max: 3 });  // v0-v3 supported
    versions.insert(ApiKey::WriteTxnMarkers, VersionRange { min: 0, max: 0 });  // Broker-internal, not client-facing
    versions.insert(ApiKey::TxnOffsetCommit, VersionRange { min: 0, max: 3 });  // v0-v3 supported
    
    // ACL APIs (v0-v3 supported, flexible encoding from v2+)
    versions.insert(ApiKey::DescribeAcls, VersionRange { min: 0, max: 3 });
    versions.insert(ApiKey::CreateAcls, VersionRange { min: 0, max: 3 });
    versions.insert(ApiKey::DeleteAcls, VersionRange { min: 0, max: 3 });
    
    // Additional APIs (NOT IMPLEMENTED - placeholder for librdkafka compatibility)
    versions.insert(ApiKey::DeleteRecords, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::OffsetForLeaderEpoch, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::AlterReplicaLogDirs, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::DescribeLogDirs, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::SaslAuthenticate, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::CreatePartitions, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::CreateDelegationToken, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::RenewDelegationToken, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::ExpireDelegationToken, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::DescribeDelegationToken, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::DeleteGroups, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::ElectLeaders, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::IncrementalAlterConfigs, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::AlterPartitionReassignments, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::ListPartitionReassignments, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::OffsetDelete, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::DescribeClientQuotas, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::AlterClientQuotas, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::DescribeUserScramCredentials, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::AlterUserScramCredentials, VersionRange { min: 0, max: 0 });
    
    // KRaft consensus APIs (NOT IMPLEMENTED - placeholder for librdkafka compatibility)
    // Note: Skipping APIs 52-55, 59, 62-64 to match CP Kafka 7.5.0
    // versions.insert(ApiKey::Vote, VersionRange { min: 0, max: 0 });  // API 52 - not in CP Kafka
    // versions.insert(ApiKey::BeginQuorumEpoch, VersionRange { min: 0, max: 0 });  // API 53 - not in CP Kafka
    // versions.insert(ApiKey::EndQuorumEpoch, VersionRange { min: 0, max: 0 });  // API 54 - not in CP Kafka
    // versions.insert(ApiKey::DescribeQuorum, VersionRange { min: 0, max: 0 });  // API 55 - not in CP Kafka
    versions.insert(ApiKey::AlterPartition, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::UpdateFeatures, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::Envelope, VersionRange { min: 0, max: 0 });
    // versions.insert(ApiKey::FetchSnapshot, VersionRange { min: 0, max: 0 });  // API 59 - not in CP Kafka
    versions.insert(ApiKey::DescribeCluster, VersionRange { min: 1, max: 1 });  // Only advertise v1 to force flexible encoding
    versions.insert(ApiKey::DescribeProducers, VersionRange { min: 0, max: 0 });
    // versions.insert(ApiKey::BrokerRegistration, VersionRange { min: 0, max: 0 });  // API 62 - not in CP Kafka
    // versions.insert(ApiKey::BrokerHeartbeat, VersionRange { min: 0, max: 0 });  // API 63 - not in CP Kafka
    // versions.insert(ApiKey::UnregisterBroker, VersionRange { min: 0, max: 0 });  // API 64 - not in CP Kafka
    
    // Transaction coordination APIs (NOT IMPLEMENTED - placeholder for librdkafka compatibility)
    versions.insert(ApiKey::DescribeTransactions, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::ListTransactions, VersionRange { min: 0, max: 0 });
    versions.insert(ApiKey::AllocateProducerIds, VersionRange { min: 0, max: 0 });
    
    // Consumer group coordination v2 (NOT IMPLEMENTED - placeholder for librdkafka compatibility)
    // versions.insert(ApiKey::ConsumerGroupHeartbeat, VersionRange { min: 0, max: 0 });  // API 68 - not in CP Kafka
    
    versions
}

/// Trait for types that can be decoded from Kafka wire protocol
pub trait KafkaDecodable: Sized {
    /// Decode from a decoder
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self>;
}

/// Trait for types that can be encoded to Kafka wire protocol
pub trait KafkaEncodable {
    /// Encode to an encoder
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_varint_encoding() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        encoder.write_unsigned_varint(0);
        encoder.write_unsigned_varint(127);
        encoder.write_unsigned_varint(128);
        encoder.write_unsigned_varint(16383);
        encoder.write_unsigned_varint(16384);
        
        let mut frozen_buf = buf.freeze();
        let mut decoder = Decoder::new(&mut frozen_buf);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 0);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 127);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 128);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 16383);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 16384);
    }
    
    #[test]
    fn test_string_encoding() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        encoder.write_string(Some("hello"));
        encoder.write_string(None);
        encoder.write_string(Some(""));
        
        let mut frozen_buf = buf.freeze();
        let mut decoder = Decoder::new(&mut frozen_buf);
        assert_eq!(decoder.read_string().unwrap(), Some("hello".to_string()));
        assert_eq!(decoder.read_string().unwrap(), None);
        assert_eq!(decoder.read_string().unwrap(), Some("".to_string()));
    }
    
    #[test]
    fn test_parse_unknown_api_key() {
        // Build a request with unknown API key 999
        let mut buf = BytesMut::new();
        buf.put_i16(999);  // Unknown API key
        buf.put_i16(0);    // API version
        buf.put_i32(67890); // Correlation ID
        buf.put_i16(-1);   // Client ID (null)
        
        let mut bytes = buf.freeze();
        
        // Test that it returns ProtocolWithCorrelation error
        match parse_request_header_with_correlation(&mut bytes) {
            Ok(_) => panic!("Expected error for unknown API key"),
            Err(Error::ProtocolWithCorrelation { correlation_id, message }) => {
                assert_eq!(correlation_id, 67890);
                assert!(message.contains("999"));
            }
            Err(e) => panic!("Expected ProtocolWithCorrelation error, got: {:?}", e),
        }
    }
}