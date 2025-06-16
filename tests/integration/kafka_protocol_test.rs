//! Integration tests for Kafka protocol compatibility.

use chronik_ingest::{IngestServer, ServerConfig};
use chronik_protocol::{
    ApiKey, MetadataRequest, MetadataResponse, ProduceRequest, FetchRequest,
    RequestHeader, ResponseHeader,
};
use chronik_storage::segment::SegmentManager;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Test helper to start an ingest server
async fn start_test_server() -> (String, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let config = ServerConfig {
        listen_addr: "127.0.0.1:0".to_string(),
        segment_size: 1024 * 1024, // 1MB
        flush_interval_ms: 100,
    };
    
    let storage = SegmentManager::new(temp_dir.path()).await.unwrap();
    let server = IngestServer::new(config, storage);
    
    let addr = server.local_addr().await.unwrap();
    
    tokio::spawn(async move {
        server.run().await.unwrap();
    });
    
    // Give server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    
    (addr, temp_dir)
}

#[tokio::test]
async fn test_metadata_request() {
    let (addr, _temp_dir) = start_test_server().await;
    
    // Connect to server
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    
    // Create metadata request
    let header = RequestHeader {
        api_key: ApiKey::Metadata as i16,
        api_version: 0,
        correlation_id: 1,
        client_id: Some("test-client".to_string()),
    };
    
    let request = MetadataRequest {
        topics: None, // Get all topics
        allow_auto_topic_creation: false,
    };
    
    // Send request
    let mut buffer = Vec::new();
    header.encode(&mut buffer).unwrap();
    request.encode(&mut buffer).unwrap();
    
    let size = buffer.len() as i32;
    stream.write_all(&size.to_be_bytes()).await.unwrap();
    stream.write_all(&buffer).await.unwrap();
    
    // Read response
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await.unwrap();
    let response_size = i32::from_be_bytes(size_buf);
    
    let mut response_buf = vec![0u8; response_size as usize];
    stream.read_exact(&mut response_buf).await.unwrap();
    
    // Decode response
    let mut cursor = std::io::Cursor::new(&response_buf);
    let response_header = ResponseHeader::decode(&mut cursor).unwrap();
    let _response = MetadataResponse::decode(&mut cursor).unwrap();
    
    assert_eq!(response_header.correlation_id, 1);
}

#[tokio::test]
async fn test_produce_fetch_flow() {
    let (addr, _temp_dir) = start_test_server().await;
    
    // Connect to server
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    
    // 1. Send produce request
    let produce_header = RequestHeader {
        api_key: ApiKey::Produce as i16,
        api_version: 0,
        correlation_id: 2,
        client_id: Some("test-producer".to_string()),
    };
    
    let produce_request = ProduceRequest {
        acks: 1,
        timeout_ms: 1000,
        topic_data: vec![
            chronik_protocol::TopicProduceData {
                name: "test-topic".to_string(),
                partition_data: vec![
                    chronik_protocol::PartitionProduceData {
                        index: 0,
                        records: Some(vec![
                            chronik_protocol::Record {
                                offset: 0,
                                timestamp: chrono::Utc::now().timestamp_millis(),
                                key: None,
                                value: Some(b"Hello, Chronik!".to_vec()),
                                headers: vec![],
                            },
                        ]),
                    },
                ],
            },
        ],
    };
    
    // Send produce request
    let mut buffer = Vec::new();
    produce_header.encode(&mut buffer).unwrap();
    produce_request.encode(&mut buffer).unwrap();
    
    let size = buffer.len() as i32;
    stream.write_all(&size.to_be_bytes()).await.unwrap();
    stream.write_all(&buffer).await.unwrap();
    
    // Read produce response
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await.unwrap();
    let response_size = i32::from_be_bytes(size_buf);
    
    let mut response_buf = vec![0u8; response_size as usize];
    stream.read_exact(&mut response_buf).await.unwrap();
    
    // Give time for message to be indexed
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    
    // 2. Send fetch request
    let fetch_header = RequestHeader {
        api_key: ApiKey::Fetch as i16,
        api_version: 0,
        correlation_id: 3,
        client_id: Some("test-consumer".to_string()),
    };
    
    let fetch_request = FetchRequest {
        replica_id: -1,
        max_wait_ms: 100,
        min_bytes: 1,
        max_bytes: Some(1024 * 1024),
        isolation_level: Some(0),
        topics: vec![
            chronik_protocol::FetchTopic {
                name: "test-topic".to_string(),
                partitions: vec![
                    chronik_protocol::FetchPartition {
                        partition: 0,
                        fetch_offset: 0,
                        partition_max_bytes: 1024 * 1024,
                    },
                ],
            },
        ],
    };
    
    // Send fetch request
    buffer.clear();
    fetch_header.encode(&mut buffer).unwrap();
    fetch_request.encode(&mut buffer).unwrap();
    
    let size = buffer.len() as i32;
    stream.write_all(&size.to_be_bytes()).await.unwrap();
    stream.write_all(&buffer).await.unwrap();
    
    // Read fetch response
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await.unwrap();
    let response_size = i32::from_be_bytes(size_buf);
    
    let mut response_buf = vec![0u8; response_size as usize];
    stream.read_exact(&mut response_buf).await.unwrap();
    
    // Verify we got the message back
    // (In a real test, we would decode and verify the response)
    assert!(response_size > 0);
}

#[tokio::test]
async fn test_consumer_group_flow() {
    let (addr, _temp_dir) = start_test_server().await;
    
    // Connect to server
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    
    // 1. Join group
    let join_header = RequestHeader {
        api_key: ApiKey::JoinGroup as i16,
        api_version: 0,
        correlation_id: 4,
        client_id: Some("test-consumer".to_string()),
    };
    
    let join_request = chronik_protocol::JoinGroupRequest {
        group_id: "test-group".to_string(),
        session_timeout_ms: 30000,
        rebalance_timeout_ms: Some(60000),
        member_id: "".to_string(), // Empty for new member
        protocol_type: "consumer".to_string(),
        protocols: vec![
            chronik_protocol::JoinGroupProtocol {
                name: "range".to_string(),
                metadata: vec![], // Simplified for test
            },
        ],
    };
    
    // Send join request
    let mut buffer = Vec::new();
    join_header.encode(&mut buffer).unwrap();
    join_request.encode(&mut buffer).unwrap();
    
    let size = buffer.len() as i32;
    stream.write_all(&size.to_be_bytes()).await.unwrap();
    stream.write_all(&buffer).await.unwrap();
    
    // Read join response
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await.unwrap();
    let response_size = i32::from_be_bytes(size_buf);
    
    let mut response_buf = vec![0u8; response_size as usize];
    stream.read_exact(&mut response_buf).await.unwrap();
    
    // Verify we got a response
    assert!(response_size > 0);
}