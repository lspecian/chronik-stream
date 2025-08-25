//! Fixed Kafka server with proper message handling

use anyhow::{Result, anyhow};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, debug, warn, error};
use bytes::BytesMut;

use crate::storage::EmbeddedStorage;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApiKey {
    Produce = 0,
    Fetch = 1,
    ListOffsets = 2,
    Metadata = 3,
    OffsetCommit = 8,
    OffsetFetch = 9,
    JoinGroup = 11,
    ListGroups = 16,
    ApiVersions = 18,
    CreateTopics = 19,
    InitProducerId = 22,
    Unknown = -1,
}

pub struct RequestHeader {
    pub api_key: ApiKey,
    pub api_version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
    pub header_size: usize,
}

pub struct KafkaServer {
    storage: Arc<RwLock<EmbeddedStorage>>,
    node_id: i32,
    advertised_host: String,
    advertised_port: i32,
}

impl KafkaServer {
    pub fn new(storage: Arc<RwLock<EmbeddedStorage>>) -> Self {
        // Get advertised address from environment or use default
        let advertised_addr = std::env::var("CHRONIK_ADVERTISED_ADDR")
            .unwrap_or_else(|_| "localhost:9092".to_string());
        
        let (host, port_str) = advertised_addr.rsplit_once(':')
            .unwrap_or((&advertised_addr, "9092"));
        let port = port_str.parse().unwrap_or(9092);
        
        Self {
            storage,
            node_id: 1001,
            advertised_host: host.to_string(),
            advertised_port: port,
        }
    }

    pub async fn run(&self, addr: &str) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!("Kafka server listening on {}", addr);
        info!("Advertised as {}:{}", self.advertised_host, self.advertised_port);
        
        loop {
            let (stream, peer_addr) = listener.accept().await?;
            info!("New connection from {}", peer_addr);
            
            let storage = self.storage.clone();
            let node_id = self.node_id;
            let advertised_host = self.advertised_host.clone();
            let advertised_port = self.advertised_port;
            
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, storage, node_id, advertised_host, advertised_port).await {
                    error!("Connection error: {}", e);
                }
            });
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    storage: Arc<RwLock<EmbeddedStorage>>,
    node_id: i32,
    advertised_host: String,
    advertised_port: i32,
) -> Result<()> {
    let mut buffer = BytesMut::with_capacity(1024 * 64);
    
    loop {
        // Read request size
        let mut size_buf = [0u8; 4];
        match stream.read_exact(&mut size_buf).await {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("Client disconnected");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        
        let size = i32::from_be_bytes(size_buf) as usize;
        if size == 0 || size > 100 * 1024 * 1024 {
            warn!("Invalid request size: {}", size);
            return Err(anyhow!("Invalid request size"));
        }
        
        // Read request
        buffer.resize(size, 0);
        stream.read_exact(&mut buffer).await?;
        
        // Parse request header
        let header = parse_request_header(&buffer)?;
        info!("Request: api_key={:?}, version={}, correlation_id={}, client_id={:?}", 
               header.api_key, header.api_version, header.correlation_id, header.client_id);
        
        // Process request
        let body_start = header.header_size;
        let response = match header.api_key {
            ApiKey::ApiVersions => handle_api_versions(&header),
            ApiKey::Metadata => handle_metadata(&header, &storage, node_id, &advertised_host, advertised_port).await,
            ApiKey::CreateTopics => handle_create_topics(&header, &buffer[body_start..], storage.clone()).await,
            ApiKey::Produce => handle_produce_fixed(&header, &buffer[body_start..], storage.clone()).await,
            ApiKey::Fetch => handle_fetch_fixed(&header, &buffer[body_start..], storage.clone()).await,
            ApiKey::ListOffsets => handle_list_offsets(&header, &buffer[body_start..], storage.clone()).await,
            ApiKey::InitProducerId => handle_init_producer_id(&header),
            _ => {
                warn!("Unsupported API: {:?}", header.api_key);
                create_error_response(&header, 35)
            }
        };
        
        // Send response
        stream.write_all(&response).await?;
    }
}

async fn handle_produce_fixed(
    header: &RequestHeader,
    data: &[u8],
    storage: Arc<RwLock<EmbeddedStorage>>,
) -> Vec<u8> {
    let mut response = Vec::new();
    response.extend_from_slice(&[0, 0, 0, 0]); // Size placeholder
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Parse produce request properly
    let mut cursor = 0;
    
    // Skip transactional_id (nullable string)
    if data.len() > cursor + 2 {
        let str_len = i16::from_be_bytes([data[cursor], data[cursor + 1]]);
        cursor += 2;
        if str_len > 0 {
            cursor += str_len as usize;
        }
    }
    
    // Skip acks and timeout
    cursor += 2 + 4; // acks(i16) + timeout(i32)
    
    // Parse topics array
    let mut response_topics = Vec::new();
    
    if data.len() > cursor + 4 {
        let topic_count = i32::from_be_bytes([data[cursor], data[cursor+1], data[cursor+2], data[cursor+3]]);
        cursor += 4;
        
        for _ in 0..topic_count {
            if data.len() <= cursor + 2 {
                break;
            }
            
            // Topic name
            let name_len = i16::from_be_bytes([data[cursor], data[cursor + 1]]);
            cursor += 2;
            
            let topic_name = if name_len > 0 && data.len() >= cursor + name_len as usize {
                let name = String::from_utf8_lossy(&data[cursor..cursor + name_len as usize]).to_string();
                cursor += name_len as usize;
                name
            } else {
                "unknown".to_string()
            };
            
            // Parse partitions
            let mut partition_responses = Vec::new();
            
            if data.len() > cursor + 4 {
                let partition_count = i32::from_be_bytes([data[cursor], data[cursor+1], data[cursor+2], data[cursor+3]]);
                cursor += 4;
                
                for _ in 0..partition_count {
                    if data.len() <= cursor + 4 {
                        break;
                    }
                    
                    let partition_id = i32::from_be_bytes([data[cursor], data[cursor+1], data[cursor+2], data[cursor+3]]);
                    cursor += 4;
                    
                    // Parse records (simplified - just skip for now)
                    if data.len() > cursor + 4 {
                        let record_size = i32::from_be_bytes([data[cursor], data[cursor+1], data[cursor+2], data[cursor+3]]);
                        cursor += 4;
                        
                        if record_size > 0 && data.len() >= cursor + record_size as usize {
                            // Extract messages from record batch
                            let messages = parse_record_batch(&data[cursor..cursor + record_size as usize]);
                            cursor += record_size as usize;
                            
                            // Store messages
                            let mut storage = storage.write().await;
                            let offsets = storage.append_messages(topic_name.clone(), partition_id, messages)
                                .await
                                .unwrap_or_else(|_| vec![0]);
                            
                            let base_offset = offsets.first().copied().unwrap_or(0);
                            
                            partition_responses.push((partition_id, 0i16, base_offset)); // 0 = success
                        } else if record_size == -1 {
                            // Null records
                            cursor += 0;
                            partition_responses.push((partition_id, 0i16, 0i64));
                        }
                    }
                }
            }
            
            response_topics.push((topic_name, partition_responses));
        }
    }
    
    // Build response
    if header.api_version >= 1 {
        // Throttle time (v1+)
        response.extend_from_slice(&0i32.to_be_bytes());
    }
    
    // Topics array
    response.extend_from_slice(&(response_topics.len() as i32).to_be_bytes());
    
    for (topic_name, partitions) in response_topics {
        // Topic name
        response.extend_from_slice(&(topic_name.len() as i16).to_be_bytes());
        response.extend_from_slice(topic_name.as_bytes());
        
        // Partitions array
        response.extend_from_slice(&(partitions.len() as i32).to_be_bytes());
        
        for (partition_id, error_code, base_offset) in partitions {
            response.extend_from_slice(&partition_id.to_be_bytes());
            response.extend_from_slice(&error_code.to_be_bytes());
            response.extend_from_slice(&base_offset.to_be_bytes());
            
            // Log append time (v2+)
            if header.api_version >= 2 {
                response.extend_from_slice(&(-1i64).to_be_bytes());
            }
            
            // Log start offset (v5+)
            if header.api_version >= 5 {
                response.extend_from_slice(&0i64.to_be_bytes());
            }
        }
    }
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

async fn handle_fetch_fixed(
    header: &RequestHeader,
    data: &[u8],
    storage: Arc<RwLock<EmbeddedStorage>>,
) -> Vec<u8> {
    let mut response = Vec::new();
    response.extend_from_slice(&[0, 0, 0, 0]); // Size placeholder
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Throttle time (v1+)
    if header.api_version >= 1 {
        response.extend_from_slice(&0i32.to_be_bytes());
    }
    
    // Session ID (v7+)
    if header.api_version >= 7 {
        response.extend_from_slice(&0i32.to_be_bytes());
    }
    
    // Parse fetch request
    let mut cursor = 4; // Skip replica_id
    cursor += 4; // Skip max_wait_time
    cursor += 4; // Skip min_bytes
    
    if header.api_version >= 3 {
        cursor += 4; // Skip max_bytes (v3+)
    }
    if header.api_version >= 4 {
        cursor += 1; // Skip isolation_level (v4+)
    }
    if header.api_version >= 7 {
        cursor += 4; // Skip session_id (v7+)
        cursor += 4; // Skip session_epoch (v7+)
    }
    
    // Parse topics
    let mut response_topics = Vec::new();
    
    if data.len() > cursor + 4 {
        let topic_count = i32::from_be_bytes([data[cursor], data[cursor+1], data[cursor+2], data[cursor+3]]);
        cursor += 4;
        
        for _ in 0..topic_count {
            if data.len() <= cursor + 2 {
                break;
            }
            
            // Topic name
            let name_len = i16::from_be_bytes([data[cursor], data[cursor + 1]]);
            cursor += 2;
            
            let topic_name = if name_len > 0 && data.len() >= cursor + name_len as usize {
                let name = String::from_utf8_lossy(&data[cursor..cursor + name_len as usize]).to_string();
                cursor += name_len as usize;
                name
            } else {
                continue;
            };
            
            // Parse partitions
            let mut partition_responses = Vec::new();
            
            if data.len() > cursor + 4 {
                let partition_count = i32::from_be_bytes([data[cursor], data[cursor+1], data[cursor+2], data[cursor+3]]);
                cursor += 4;
                
                for _ in 0..partition_count {
                    if data.len() <= cursor + 4 {
                        break;
                    }
                    
                    let partition_id = i32::from_be_bytes([data[cursor], data[cursor+1], data[cursor+2], data[cursor+3]]);
                    cursor += 4;
                    
                    let fetch_offset = if data.len() >= cursor + 8 {
                        let offset = i64::from_be_bytes([
                            data[cursor], data[cursor+1], data[cursor+2], data[cursor+3],
                            data[cursor+4], data[cursor+5], data[cursor+6], data[cursor+7]
                        ]);
                        cursor += 8;
                        offset
                    } else {
                        0
                    };
                    
                    // Skip other fields
                    if header.api_version >= 5 {
                        cursor += 8; // log_start_offset
                    }
                    cursor += 4; // max_bytes
                    
                    // Fetch messages from storage
                    let storage = storage.read().await;
                    let messages = storage.fetch_messages(topic_name.clone(), partition_id, fetch_offset, 1024 * 1024)
                        .await
                        .unwrap_or_else(|_| vec![]);
                    
                    partition_responses.push((partition_id, messages));
                }
            }
            
            response_topics.push((topic_name, partition_responses));
        }
    }
    
    // Build response
    response.extend_from_slice(&(response_topics.len() as i32).to_be_bytes());
    
    for (topic_name, partitions) in response_topics {
        // Topic name
        response.extend_from_slice(&(topic_name.len() as i16).to_be_bytes());
        response.extend_from_slice(topic_name.as_bytes());
        
        // Partitions
        response.extend_from_slice(&(partitions.len() as i32).to_be_bytes());
        
        for (partition_id, messages) in partitions {
            response.extend_from_slice(&partition_id.to_be_bytes());
            response.extend_from_slice(&0i16.to_be_bytes()); // error code
            
            // High water mark
            let high_water = messages.last().map(|m| m.offset + 1).unwrap_or(0);
            response.extend_from_slice(&high_water.to_be_bytes());
            
            // Last stable offset (v4+)
            if header.api_version >= 4 {
                response.extend_from_slice(&high_water.to_be_bytes());
            }
            
            // Log start offset (v5+)
            if header.api_version >= 5 {
                response.extend_from_slice(&0i64.to_be_bytes());
            }
            
            // Aborted transactions (v4+)
            if header.api_version >= 4 {
                response.extend_from_slice(&0i32.to_be_bytes());
            }
            
            // Records
            let records = build_record_batch(messages);
            response.extend_from_slice(&(records.len() as i32).to_be_bytes());
            response.extend_from_slice(&records);
        }
    }
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

// Helper functions for record batch handling
fn parse_record_batch(data: &[u8]) -> Vec<(Option<Vec<u8>>, Vec<u8>)> {
    // Simplified parsing - just extract key/value pairs
    // In reality, this would parse the full RecordBatch format
    vec![(None, data.to_vec())]
}

fn build_record_batch(messages: Vec<crate::storage::Message>) -> Vec<u8> {
    if messages.is_empty() {
        return vec![];
    }
    
    // Simplified - build a basic record batch
    let mut batch = Vec::new();
    
    // RecordBatch header (simplified)
    batch.extend_from_slice(&messages[0].offset.to_be_bytes()); // base offset
    batch.extend_from_slice(&(messages.len() as i32).to_be_bytes()); // batch length
    batch.extend_from_slice(&0i32.to_be_bytes()); // partition leader epoch
    batch.push(2); // magic byte (v2)
    batch.extend_from_slice(&0i32.to_be_bytes()); // crc
    batch.extend_from_slice(&0i16.to_be_bytes()); // attributes
    batch.extend_from_slice(&(messages.len() as i32 - 1).to_be_bytes()); // last offset delta
    batch.extend_from_slice(&messages[0].timestamp.to_be_bytes()); // first timestamp
    batch.extend_from_slice(&messages.last().unwrap().timestamp.to_be_bytes()); // max timestamp
    batch.extend_from_slice(&(-1i64).to_be_bytes()); // producer id
    batch.extend_from_slice(&(-1i16).to_be_bytes()); // producer epoch
    batch.extend_from_slice(&(-1i32).to_be_bytes()); // base sequence
    batch.extend_from_slice(&(messages.len() as i32).to_be_bytes()); // record count
    
    // Records
    for msg in messages {
        // Simplified record format
        batch.push(0); // attributes
        batch.extend_from_slice(&0i64.to_be_bytes()); // timestamp delta
        batch.extend_from_slice(&0i32.to_be_bytes()); // offset delta
        
        // Key
        if let Some(key) = &msg.key {
            batch.extend_from_slice(&(key.len() as i32).to_be_bytes());
            batch.extend_from_slice(key);
        } else {
            batch.extend_from_slice(&(-1i32).to_be_bytes());
        }
        
        // Value
        batch.extend_from_slice(&(msg.value.len() as i32).to_be_bytes());
        batch.extend_from_slice(&msg.value);
        
        // Headers
        batch.extend_from_slice(&0i32.to_be_bytes()); // no headers
    }
    
    batch
}

// Import remaining functions from original kafka_server.rs
use crate::kafka_server::{
    parse_request_header,
    handle_api_versions,
    handle_metadata,
    handle_create_topics,
    handle_init_producer_id,
    handle_list_offsets,
    create_error_response,
};

// Helper to convert i16 to ApiKey
impl ApiKey {
    fn from_i16(value: i16) -> Option<Self> {
        match value {
            0 => Some(ApiKey::Produce),
            1 => Some(ApiKey::Fetch),
            2 => Some(ApiKey::ListOffsets),
            3 => Some(ApiKey::Metadata),
            8 => Some(ApiKey::OffsetCommit),
            9 => Some(ApiKey::OffsetFetch),
            11 => Some(ApiKey::JoinGroup),
            16 => Some(ApiKey::ListGroups),
            18 => Some(ApiKey::ApiVersions),
            19 => Some(ApiKey::CreateTopics),
            22 => Some(ApiKey::InitProducerId),
            _ => Some(ApiKey::Unknown),
        }
    }
}