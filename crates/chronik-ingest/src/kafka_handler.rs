//! Kafka protocol handler that integrates with storage and indexing.

use crate::produce_handler::ProduceHandler;
use crate::fetch_handler::FetchHandler;
use crate::storage::StorageService;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{
    ProtocolHandler as BaseProtocolHandler,
    ProduceRequest, ProduceResponse,
    FetchRequest, FetchResponse,
    MetadataRequest, MetadataResponse, MetadataBroker, MetadataTopic, MetadataPartition,
    parser::ResponseHeader,
};
use chronik_storage::SegmentReader;
use bytes::{Bytes, BytesMut};
use std::sync::Arc;

/// Kafka protocol handler with storage integration
pub struct KafkaProtocolHandler {
    base_handler: BaseProtocolHandler,
    produce_handler: Arc<ProduceHandler>,
    fetch_handler: Arc<FetchHandler>,
    metadata_store: Arc<dyn MetadataStore>,
    node_id: i32,
    host: String,
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
        let fetch_handler = Arc::new(FetchHandler::new(
            segment_reader,
            metadata_store.clone(),
        ));
        
        Ok(Self {
            base_handler: BaseProtocolHandler::new(),
            produce_handler,
            fetch_handler,
            metadata_store,
            node_id,
            host,
            port,
        })
    }
    
    /// Handle a raw request
    pub async fn handle_request(&self, request_bytes: &[u8]) -> Result<chronik_protocol::handler::Response> {
        use chronik_protocol::parser::{parse_request_header, ApiKey};
        use bytes::Bytes;
        
        let mut buf = Bytes::copy_from_slice(request_bytes);
        let header = parse_request_header(&mut buf)?;
        
        // Route based on API key
        match header.api_key {
            ApiKey::Produce => self.handle_produce(header, buf).await,
            ApiKey::Fetch => self.handle_fetch(header, buf).await,
            ApiKey::Metadata => self.handle_metadata(header, buf).await,
            _ => {
                // Delegate to base handler for other requests
                self.base_handler.handle_request(request_bytes).await
            }
        }
    }
    
    /// Handle produce request with actual storage
    async fn handle_produce(
        &self,
        header: chronik_protocol::parser::RequestHeader,
        mut body: Bytes,
    ) -> Result<chronik_protocol::handler::Response> {
        use chronik_protocol::parser::Decoder;
        use bytes::BytesMut;
        
        // Parse the request completely before any await
        let request = {
            let mut decoder = Decoder::new(&mut body);
            
            // Parse produce request
            let transactional_id = if header.api_version >= 3 {
                decoder.read_string()?
            } else {
                None
            };
            
            let acks = decoder.read_i16()?;
            let timeout_ms = decoder.read_i32()?;
            
            // Read topics
            let topic_count = decoder.read_i32()? as usize;
            let mut topics = Vec::with_capacity(topic_count);
            
            for _ in 0..topic_count {
                let topic_name = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
                
                let partition_count = decoder.read_i32()? as usize;
                let mut partitions = Vec::with_capacity(partition_count);
                
                for _ in 0..partition_count {
                    let partition_index = decoder.read_i32()?;
                    let records = decoder.read_bytes()?
                        .ok_or_else(|| Error::Protocol("Records cannot be null".into()))?;
                    
                    partitions.push(chronik_protocol::types::ProduceRequestPartition {
                        index: partition_index,
                        records: records.to_vec(),
                    });
                }
                
                topics.push(chronik_protocol::types::ProduceRequestTopic {
                    name: topic_name,
                    partitions,
                });
            }
            
            ProduceRequest {
                transactional_id,
                acks,
                timeout_ms,
                topics,
            }
        }; // decoder is dropped here
        
        // Handle through produce handler
        let response = self.produce_handler.handle_produce(request, header.correlation_id).await?;
        
        // Encode response
        let mut body_buf = BytesMut::new();
        encode_produce_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(chronik_protocol::handler::Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body: body_buf.freeze(),
        })
    }
    
    /// Handle fetch request with actual storage
    async fn handle_fetch(
        &self,
        header: chronik_protocol::parser::RequestHeader,
        mut body: Bytes,
    ) -> Result<chronik_protocol::handler::Response> {
        use chronik_protocol::parser::Decoder;
        use bytes::BytesMut;
        
        // Parse the request completely before any await
        let request = {
            let mut decoder = Decoder::new(&mut body);
            
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
            
            // Read topics
            let topic_count = decoder.read_i32()? as usize;
            let mut topics = Vec::with_capacity(topic_count);
            
            for _ in 0..topic_count {
                let topic_name = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
                
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
                    
                    partitions.push(chronik_protocol::types::FetchRequestPartition {
                        partition,
                        current_leader_epoch,
                        fetch_offset,
                        log_start_offset,
                        partition_max_bytes,
                    });
                }
                
                topics.push(chronik_protocol::types::FetchRequestTopic {
                    name: topic_name,
                    partitions,
                });
            }
            
            FetchRequest {
                replica_id,
                max_wait_ms,
                min_bytes,
                max_bytes,
                isolation_level,
                session_id,
                session_epoch,
                topics,
            }
        }; // decoder is dropped here
        
        // Handle through fetch handler
        let response = self.fetch_handler.handle_fetch(request, header.correlation_id).await?;
        
        // Encode response
        let mut body_buf = BytesMut::new();
        encode_fetch_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(chronik_protocol::handler::Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body: body_buf.freeze(),
        })
    }
    
    /// Handle metadata request with actual metadata
    async fn handle_metadata(
        &self,
        header: chronik_protocol::parser::RequestHeader,
        mut body: Bytes,
    ) -> Result<chronik_protocol::handler::Response> {
        use chronik_protocol::parser::Decoder;
        use bytes::BytesMut;
        
        // Parse the request completely before any await
        let (topics, _allow_auto_topic_creation) = {
            let mut decoder = Decoder::new(&mut body);
            
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
                None
            };
            
            let allow_auto_topic_creation = if header.api_version >= 4 {
                decoder.read_bool()?
            } else {
                true
            };
            
            (topics, allow_auto_topic_creation)
        }; // decoder is dropped here
        
        // Build metadata response
        let brokers = vec![MetadataBroker {
            node_id: self.node_id,
            host: self.host.clone(),
            port: self.port,
            rack: None,
        }];
        
        let mut response_topics = Vec::new();
        
        // Get topics from metadata store
        if let Some(topic_names) = topics {
            for topic_name in topic_names {
                if let Ok(Some(topic_meta)) = self.metadata_store.get_topic(&topic_name).await {
                    let mut partitions = Vec::new();
                    
                    for i in 0..topic_meta.config.partition_count {
                        partitions.push(MetadataPartition {
                            error_code: 0,
                            partition_index: i as i32,
                            leader_id: self.node_id, // Simplified: this node is leader
                            leader_epoch: 0,
                            replica_nodes: vec![self.node_id],
                            isr_nodes: vec![self.node_id],
                            offline_replicas: vec![],
                        });
                    }
                    
                    response_topics.push(MetadataTopic {
                        error_code: 0,
                        name: topic_name,
                        is_internal: false,
                        partitions,
                    });
                } else {
                    // Topic not found
                    response_topics.push(MetadataTopic {
                        error_code: 3, // UNKNOWN_TOPIC_OR_PARTITION
                        name: topic_name,
                        is_internal: false,
                        partitions: vec![],
                    });
                }
            }
        } else {
            // Return all topics
            if let Ok(topics) = self.metadata_store.list_topics().await {
                for topic_meta in topics {
                    let mut partitions = Vec::new();
                    
                    for i in 0..topic_meta.config.partition_count {
                        partitions.push(MetadataPartition {
                            error_code: 0,
                            partition_index: i as i32,
                            leader_id: self.node_id,
                            leader_epoch: 0,
                            replica_nodes: vec![self.node_id],
                            isr_nodes: vec![self.node_id],
                            offline_replicas: vec![],
                        });
                    }
                    
                    response_topics.push(MetadataTopic {
                        error_code: 0,
                        name: topic_meta.name,
                        is_internal: false,
                        partitions,
                    });
                }
            }
        }
        
        let response = MetadataResponse {
            correlation_id: header.correlation_id,
            throttle_time_ms: 0,
            brokers,
            cluster_id: Some("chronik-stream".to_string()),
            controller_id: 1,
            topics: response_topics,
        };
        
        // Encode response
        let mut body_buf = BytesMut::new();
        encode_metadata_response(&mut body_buf, &response, header.api_version)?;
        
        Ok(chronik_protocol::handler::Response {
            header: ResponseHeader { correlation_id: header.correlation_id },
            body: body_buf.freeze(),
        })
    }
}

// Helper functions for encoding responses
fn encode_produce_response(buf: &mut BytesMut, response: &ProduceResponse, version: i16) -> Result<()> {
    use chronik_protocol::parser::Encoder;
    
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

fn encode_fetch_response(buf: &mut BytesMut, response: &FetchResponse, version: i16) -> Result<()> {
    use chronik_protocol::parser::Encoder;
    
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

fn encode_metadata_response(buf: &mut BytesMut, response: &MetadataResponse, version: i16) -> Result<()> {
    use chronik_protocol::parser::Encoder;
    
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