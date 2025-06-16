//! Kafka protocol request handler.

use bytes::{Bytes, BytesMut};
use chronik_common::{Result, Error};
use std::collections::HashMap;
use crate::parser::{
    ApiKey, RequestHeader, ResponseHeader, VersionRange, 
    parse_request_header, supported_api_versions,
    Encoder
};

/// Response for an API request
pub struct Response {
    pub header: ResponseHeader,
    pub body: Bytes,
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
}

impl ProtocolHandler {
    /// Create a new protocol handler.
    pub fn new() -> Self {
        Self {
            supported_versions: supported_api_versions(),
        }
    }
    
    /// Handle a raw request and return a response
    pub async fn handle_request(&self, request_bytes: &[u8]) -> Result<Response> {
        let mut buf = Bytes::copy_from_slice(request_bytes);
        let header = parse_request_header(&mut buf)?;
        
        // Check if we support this API and version
        if let Some(version_range) = self.supported_versions.get(&header.api_key) {
            if header.api_version < version_range.min || header.api_version > version_range.max {
                return self.error_response(
                    header.correlation_id,
                    35, // UNSUPPORTED_VERSION
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
            ApiKey::ApiVersions => self.handle_api_versions(header, &mut buf).await,
            ApiKey::Metadata => self.handle_metadata(header, &mut buf).await,
            ApiKey::Produce => self.handle_produce(header, &mut buf).await,
            ApiKey::Fetch => self.handle_fetch(header, &mut buf).await,
            _ => self.error_response(
                header.correlation_id,
                35, // UNSUPPORTED_VERSION
            ),
        }
    }
    
    /// Handle ApiVersions request
    async fn handle_api_versions(
        &self,
        header: RequestHeader,
        _body: &mut Bytes,
    ) -> Result<Response> {
        let mut api_versions = Vec::new();
        
        for (api_key, version_range) in &self.supported_versions {
            api_versions.push(ApiVersionInfo {
                api_key: *api_key as i16,
                min_version: version_range.min,
                max_version: version_range.max,
            });
        }
        
        let response = ApiVersionsResponse {
            error_code: 0,
            api_versions,
            throttle_time_ms: 0,
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_api_versions_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Response {
            header: ResponseHeader {
                correlation_id: header.correlation_id,
            },
            body: body_buf.freeze(),
        })
    }
    
    /// Encode ApiVersions response
    fn encode_api_versions_response(
        &self,
        buf: &mut BytesMut,
        response: &ApiVersionsResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        encoder.write_i16(response.error_code);
        
        if version >= 0 {
            // Write array of API versions
            encoder.write_i32(response.api_versions.len() as i32);
            for api in &response.api_versions {
                encoder.write_i16(api.api_key);
                encoder.write_i16(api.min_version);
                encoder.write_i16(api.max_version);
                if version >= 3 {
                    // Write tagged fields (empty for now)
                    encoder.write_unsigned_varint(0);
                }
            }
        }
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        if version >= 3 {
            // Write tagged fields (empty for now)
            encoder.write_unsigned_varint(0);
        }
        
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
        
        let mut decoder = Decoder::new(body);
        
        // Parse metadata request
        let topics = if header.api_version >= 1 {
            let topic_count = decoder.read_i32()?;
            if topic_count < 0 {
                None // All topics
            } else {
                let mut topic_names = Vec::with_capacity(topic_count as usize);
                for _ in 0..topic_count {
                    if let Some(name) = decoder.read_string()? {
                        topic_names.push(name);
                    }
                }
                Some(topic_names)
            }
        } else {
            // v0 gets all topics
            None
        };
        
        let allow_auto_topic_creation = if header.api_version >= 4 {
            decoder.read_bool()?
        } else {
            true
        };
        
        let include_cluster_authorized_operations = if header.api_version >= 8 {
            decoder.read_bool()?
        } else {
            false
        };
        
        let include_topic_authorized_operations = if header.api_version >= 8 {
            decoder.read_bool()?
        } else {
            false
        };
        
        let _request = MetadataRequest {
            topics,
            allow_auto_topic_creation,
            include_cluster_authorized_operations,
            include_topic_authorized_operations,
        };
        
        // Create simple response
        let response = MetadataResponse {
            correlation_id: header.correlation_id,
            throttle_time_ms: 0,
            brokers: vec![MetadataBroker {
                node_id: 1,
                host: "localhost".to_string(),
                port: 9092,
                rack: None,
            }],
            cluster_id: Some("chronik-stream".to_string()),
            controller_id: 1,
            topics: vec![],
        };
        
        let mut body_buf = BytesMut::new();
        self.encode_metadata_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body: body_buf.freeze(),
        })
    }
    
    /// Handle Produce request
    async fn handle_produce(
        &self,
        header: RequestHeader,
        body: &mut Bytes,
    ) -> Result<Response> {
        use crate::types::{ProduceRequest, ProduceResponse, ProduceResponseTopic, ProduceResponsePartition};
        use crate::parser::Decoder;
        
        let mut decoder = Decoder::new(body);
        
        // Parse produce request based on version
        let transactional_id = if header.api_version >= 3 {
            decoder.read_string()?
        } else {
            None
        };
        
        let acks = decoder.read_i16()?;
        let timeout_ms = decoder.read_i32()?;
        
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
                let partition_index = decoder.read_i32()?;
                let records = decoder.read_bytes()?
                    .ok_or_else(|| Error::Protocol("Records cannot be null".into()))?;
                
                partitions.push(crate::types::ProduceRequestPartition {
                    index: partition_index,
                    records: records.to_vec(),
                });
            }
            
            topics.push(crate::types::ProduceRequestTopic {
                name: topic_name,
                partitions,
            });
        }
        
        let request = ProduceRequest {
            transactional_id,
            acks,
            timeout_ms,
            topics,
        };
        
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
        
        Ok(Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body: body_buf.freeze(),
        })
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
        
        let mut body_buf = BytesMut::new();
        self.encode_fetch_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body: body_buf.freeze(),
        })
    }
    
    /// Create an error response
    fn error_response(&self, correlation_id: i32, error_code: i16) -> Result<Response> {
        let mut body_buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut body_buf);
        encoder.write_i16(error_code);
        
        Ok(Response {
            header: ResponseHeader { correlation_id },
            body: body_buf.freeze(),
        })
    }
    
    /// Encode Produce response
    fn encode_produce_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::types::ProduceResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        // Topics array
        encoder.write_i32(response.topics.len() as i32);
        for topic in &response.topics {
            encoder.write_string(Some(&topic.name));
            
            // Partitions array
            encoder.write_i32(topic.partitions.len() as i32);
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
            }
        }
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        Ok(())
    }
    
    /// Encode Fetch response
    fn encode_fetch_response(
        &self,
        buf: &mut BytesMut,
        response: &crate::types::FetchResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        
        if version >= 1 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        // Topics array
        encoder.write_i32(response.topics.len() as i32);
        for topic in &response.topics {
            encoder.write_string(Some(&topic.name));
            
            // Partitions array
            encoder.write_i32(topic.partitions.len() as i32);
            for partition in &topic.partitions {
                encoder.write_i32(partition.partition);
                encoder.write_i16(partition.error_code);
                encoder.write_i64(partition.high_watermark);
                
                if version >= 4 {
                    encoder.write_i64(partition.last_stable_offset);
                    
                    if version >= 5 {
                        encoder.write_i64(partition.log_start_offset);
                    }
                    
                    // Aborted transactions (null for now)
                    encoder.write_i32(-1);
                }
                
                // Records
                encoder.write_bytes(if partition.records.is_empty() {
                    None
                } else {
                    Some(&partition.records)
                });
            }
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
        
        if version >= 3 {
            encoder.write_i32(response.throttle_time_ms);
        }
        
        // Brokers array
        encoder.write_i32(response.brokers.len() as i32);
        for broker in &response.brokers {
            encoder.write_i32(broker.node_id);
            encoder.write_string(Some(&broker.host));
            encoder.write_i32(broker.port);
            
            if version >= 1 {
                encoder.write_string(broker.rack.as_deref());
            }
        }
        
        if version >= 2 {
            encoder.write_string(response.cluster_id.as_deref());
        }
        
        if version >= 1 {
            encoder.write_i32(response.controller_id);
        }
        
        // Topics array
        encoder.write_i32(response.topics.len() as i32);
        for topic in &response.topics {
            encoder.write_i16(topic.error_code);
            encoder.write_string(Some(&topic.name));
            
            if version >= 1 {
                encoder.write_bool(topic.is_internal);
            }
            
            // Partitions array
            encoder.write_i32(topic.partitions.len() as i32);
            for partition in &topic.partitions {
                encoder.write_i16(partition.error_code);
                encoder.write_i32(partition.partition_index);
                encoder.write_i32(partition.leader_id);
                
                if version >= 7 {
                    encoder.write_i32(partition.leader_epoch);
                }
                
                // Replica nodes
                encoder.write_i32(partition.replica_nodes.len() as i32);
                for replica in &partition.replica_nodes {
                    encoder.write_i32(*replica);
                }
                
                // ISR nodes
                encoder.write_i32(partition.isr_nodes.len() as i32);
                for isr in &partition.isr_nodes {
                    encoder.write_i32(*isr);
                }
                
                if version >= 5 {
                    // Offline replicas
                    encoder.write_i32(partition.offline_replicas.len() as i32);
                    for offline in &partition.offline_replicas {
                        encoder.write_i32(*offline);
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