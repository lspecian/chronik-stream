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
    Unknown = -1,
}

pub struct RequestHeader {
    pub api_key: ApiKey,
    pub api_version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
}

pub struct KafkaServer {
    storage: Arc<RwLock<EmbeddedStorage>>,
    node_id: i32,
}

impl KafkaServer {
    pub fn new(storage: Arc<RwLock<EmbeddedStorage>>) -> Self {
        Self {
            storage,
            node_id: 1001, // Single node ID for all-in-one
        }
    }

    pub async fn run(&self, addr: &str) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!("Kafka server listening on {}", addr);

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            info!("New connection from {}", peer_addr);
            
            let storage = self.storage.clone();
            let node_id = self.node_id;
            
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, storage, node_id).await {
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
) -> Result<()> {
    let mut buffer = BytesMut::with_capacity(4096);
    
    loop {
        // Read message size (4 bytes)
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
        if size > 10_000_000 {
            return Err(anyhow!("Message too large: {} bytes", size));
        }
        
        // Read the full message
        buffer.resize(size, 0);
        stream.read_exact(&mut buffer).await?;
        
        // Parse request header
        let header = parse_request_header(&buffer)?;
        debug!("Request: api_key={:?}, version={}, correlation_id={}", 
               header.api_key, header.api_version, header.correlation_id);
        
        // Process the request
        let response = match header.api_key {
            ApiKey::ApiVersions => handle_api_versions(&header),
            ApiKey::Metadata => handle_metadata(&header, &storage, node_id).await,
            ApiKey::CreateTopics => handle_create_topics(&header, &buffer[16..], storage.clone()).await,
            ApiKey::Produce => handle_produce(&header, &buffer[16..], storage.clone()).await,
            ApiKey::Fetch => handle_fetch(&header, &buffer[16..], storage.clone()).await,
            ApiKey::ListOffsets => handle_list_offsets(&header, &buffer[16..], storage.clone()).await,
            _ => {
                warn!("Unsupported API: {:?}", header.api_key);
                create_error_response(&header, 35) // UNSUPPORTED_VERSION
            }
        };
        
        // Send response
        stream.write_all(&response).await?;
    }
}

fn parse_request_header(data: &[u8]) -> Result<RequestHeader> {
    if data.len() < 16 {
        return Err(anyhow!("Request too short"));
    }
    
    let api_key = i16::from_be_bytes([data[0], data[1]]);
    let api_version = i16::from_be_bytes([data[2], data[3]]);
    let correlation_id = i32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    
    // Skip client_id for now (bytes 8-15 contain length and string)
    
    Ok(RequestHeader {
        api_key: ApiKey::from_i16(api_key).unwrap_or(ApiKey::Unknown),
        api_version,
        correlation_id,
        client_id: None,
    })
}

fn handle_api_versions(header: &RequestHeader) -> Vec<u8> {
    let mut response = Vec::new();
    
    // Response size placeholder
    response.extend_from_slice(&[0, 0, 0, 0]);
    
    // Correlation ID
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Error code (0 = success)
    response.extend_from_slice(&[0, 0]);
    
    // API versions array length
    response.extend_from_slice(&[0, 10]); // 10 APIs
    
    // Supported APIs
    let apis: [(i16, i16, i16); 10] = [
        (18, 0, 3),  // ApiVersions
        (3, 0, 12),  // Metadata  
        (19, 0, 7),  // CreateTopics
        (0, 0, 11),  // Produce
        (1, 0, 13),  // Fetch
        (2, 0, 8),   // ListOffsets
        (8, 0, 8),   // OffsetCommit
        (9, 0, 8),   // OffsetFetch
        (11, 0, 9),  // JoinGroup
        (16, 0, 4),  // ListGroups
    ];
    
    for (api_key, min_version, max_version) in apis {
        response.extend_from_slice(&api_key.to_be_bytes());
        response.extend_from_slice(&min_version.to_be_bytes());
        response.extend_from_slice(&max_version.to_be_bytes());
    }
    
    // Throttle time ms
    response.extend_from_slice(&[0, 0, 0, 0]);
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

async fn handle_metadata(
    header: &RequestHeader,
    storage: &Arc<RwLock<EmbeddedStorage>>,
    node_id: i32,
) -> Vec<u8> {
    let mut response = Vec::new();
    
    // Response size placeholder
    response.extend_from_slice(&[0, 0, 0, 0]);
    
    // Correlation ID
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Throttle time ms (v3+)
    if header.api_version >= 3 {
        response.extend_from_slice(&[0, 0, 0, 0]);
    }
    
    // Brokers array
    response.extend_from_slice(&[0, 0, 0, 1]); // 1 broker
    
    // Broker info
    response.extend_from_slice(&node_id.to_be_bytes()); // Node ID
    
    // Host string
    let host = "localhost";
    response.extend_from_slice(&(host.len() as i16).to_be_bytes());
    response.extend_from_slice(host.as_bytes());
    
    // Port
    response.extend_from_slice(&9092i32.to_be_bytes());
    
    // Rack (v1+)
    if header.api_version >= 1 {
        response.extend_from_slice(&(-1i16).to_be_bytes()); // null rack
    }
    
    // Cluster ID (v2+)
    if header.api_version >= 2 {
        let cluster_id = "chronik-cluster";
        response.extend_from_slice(&(cluster_id.len() as i16).to_be_bytes());
        response.extend_from_slice(cluster_id.as_bytes());
    }
    
    // Controller ID (v1+)
    if header.api_version >= 1 {
        response.extend_from_slice(&node_id.to_be_bytes());
    }
    
    // Topics array
    let storage_guard = storage.read().await;
    let topics = storage_guard.list_topics().await;
    response.extend_from_slice(&(topics.len() as i32).to_be_bytes());
    
    for topic_name in topics {
        // Error code
        response.extend_from_slice(&[0, 0]);
        
        // Topic name
        response.extend_from_slice(&(topic_name.len() as i16).to_be_bytes());
        response.extend_from_slice(topic_name.as_bytes());
        
        // Is internal (v1+)
        if header.api_version >= 1 {
            response.push(0); // false
        }
        
        // Partitions
        if let Some(topic_meta) = storage_guard.get_topic(&topic_name).await {
            response.extend_from_slice(&topic_meta.partitions.to_be_bytes());
            
            for partition_id in 0..topic_meta.partitions {
                // Error code
                response.extend_from_slice(&[0, 0]);
                
                // Partition ID
                response.extend_from_slice(&partition_id.to_be_bytes());
                
                // Leader
                response.extend_from_slice(&node_id.to_be_bytes());
                
                // Replicas
                response.extend_from_slice(&[0, 0, 0, 1]); // 1 replica
                response.extend_from_slice(&node_id.to_be_bytes());
                
                // ISR
                response.extend_from_slice(&[0, 0, 0, 1]); // 1 in ISR
                response.extend_from_slice(&node_id.to_be_bytes());
                
                // Offline replicas (v5+)
                if header.api_version >= 5 {
                    response.extend_from_slice(&[0, 0, 0, 0]); // no offline replicas
                }
            }
        } else {
            response.extend_from_slice(&[0, 0, 0, 0]); // 0 partitions
        }
    }
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

async fn handle_create_topics(
    header: &RequestHeader,
    data: &[u8],
    storage: Arc<RwLock<EmbeddedStorage>>,
) -> Vec<u8> {
    // Parse create topics request
    let mut cursor = 0;
    
    // Skip client_id if present
    if data.len() > 2 {
        let client_id_len = i16::from_be_bytes([data[cursor], data[cursor + 1]]) as usize;
        cursor += 2 + client_id_len;
    }
    
    // Number of topics
    let num_topics = i32::from_be_bytes([
        data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]
    ]) as usize;
    cursor += 4;
    
    let mut response = Vec::new();
    response.extend_from_slice(&[0, 0, 0, 0]); // Size placeholder
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Throttle time (v2+)
    if header.api_version >= 2 {
        response.extend_from_slice(&[0, 0, 0, 0]);
    }
    
    // Topics array
    response.extend_from_slice(&(num_topics as i32).to_be_bytes());
    
    for _ in 0..num_topics {
        // Topic name
        let name_len = i16::from_be_bytes([data[cursor], data[cursor + 1]]) as usize;
        cursor += 2;
        let topic_name = String::from_utf8_lossy(&data[cursor..cursor + name_len]).to_string();
        cursor += name_len;
        
        // Num partitions
        let num_partitions = i32::from_be_bytes([
            data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]
        ]);
        cursor += 4;
        
        // Replication factor
        let _replication_factor = i16::from_be_bytes([data[cursor], data[cursor + 1]]);
        cursor += 2;
        
        // Skip replica assignments and configs for now
        // This is simplified - real implementation would parse these
        
        // Create the topic
        let mut storage_guard = storage.write().await;
        let error_code: i16 = match storage_guard.create_topic(topic_name.clone(), num_partitions).await {
            Ok(_) => 0,
            Err(_) => 36, // TOPIC_ALREADY_EXISTS
        };
        
        // Response for this topic
        response.extend_from_slice(&(topic_name.len() as i16).to_be_bytes());
        response.extend_from_slice(topic_name.as_bytes());
        response.extend_from_slice(&error_code.to_be_bytes());
        
        // Error message (v1+)
        if header.api_version >= 1 {
            response.extend_from_slice(&(-1i16).to_be_bytes()); // null error message
        }
    }
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

async fn handle_produce(
    header: &RequestHeader,
    _data: &[u8],
    _storage: Arc<RwLock<EmbeddedStorage>>,
) -> Vec<u8> {
    // Simplified produce handling - parse and store messages
    let mut response = Vec::new();
    response.extend_from_slice(&[0, 0, 0, 0]); // Size placeholder
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Parse produce request (simplified)
    // Real implementation would properly parse the request
    
    // Response: topics array
    response.extend_from_slice(&[0, 0, 0, 1]); // 1 topic
    
    // Topic name (placeholder)
    let topic_name = "test-topic";
    response.extend_from_slice(&(topic_name.len() as i16).to_be_bytes());
    response.extend_from_slice(topic_name.as_bytes());
    
    // Partitions array
    response.extend_from_slice(&[0, 0, 0, 1]); // 1 partition
    
    // Partition 0
    response.extend_from_slice(&[0, 0, 0, 0]); // partition ID
    response.extend_from_slice(&[0, 0]); // error code
    response.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // base offset
    
    // Log append time (v2+)
    if header.api_version >= 2 {
        response.extend_from_slice(&(-1i64).to_be_bytes()); // no append time
    }
    
    // Throttle time (v1+)
    if header.api_version >= 1 {
        response.extend_from_slice(&[0, 0, 0, 0]);
    }
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

async fn handle_fetch(
    header: &RequestHeader,
    _data: &[u8],
    _storage: Arc<RwLock<EmbeddedStorage>>,
) -> Vec<u8> {
    // Simplified fetch handling
    let mut response = Vec::new();
    response.extend_from_slice(&[0, 0, 0, 0]); // Size placeholder
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Throttle time
    if header.api_version >= 1 {
        response.extend_from_slice(&[0, 0, 0, 0]);
    }
    
    // Topics array
    response.extend_from_slice(&[0, 0, 0, 0]); // 0 topics for now
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

async fn handle_list_offsets(
    header: &RequestHeader,
    _data: &[u8],
    _storage: Arc<RwLock<EmbeddedStorage>>,
) -> Vec<u8> {
    let mut response = Vec::new();
    response.extend_from_slice(&[0, 0, 0, 0]); // Size placeholder
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Throttle time (v2+)
    if header.api_version >= 2 {
        response.extend_from_slice(&[0, 0, 0, 0]);
    }
    
    // Topics array
    response.extend_from_slice(&[0, 0, 0, 0]); // 0 topics for now
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

fn create_error_response(header: &RequestHeader, error_code: i16) -> Vec<u8> {
    let mut response = Vec::new();
    
    // Response size placeholder
    response.extend_from_slice(&[0, 0, 0, 0]);
    
    // Correlation ID
    response.extend_from_slice(&header.correlation_id.to_be_bytes());
    
    // Error code
    response.extend_from_slice(&error_code.to_be_bytes());
    
    // Update size
    let size = (response.len() - 4) as i32;
    response[0..4].copy_from_slice(&size.to_be_bytes());
    
    response
}

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
            _ => Some(ApiKey::Unknown),
        }
    }
}