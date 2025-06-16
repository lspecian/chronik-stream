//! Integration tests for the TCP server with real Kafka clients

use bytes::{Bytes, BytesMut};
use chronik_ingest::{IngestServer, ServerConfig, TlsConfig, ConnectionPoolConfig};
use chronik_protocol::parser::{Encoder, Decoder};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

/// Helper to create a test server configuration
fn test_server_config(addr: SocketAddr) -> ServerConfig {
    ServerConfig {
        listen_addr: addr,
        max_connections: 100,
        request_timeout: Duration::from_secs(5),
        buffer_size: 1024 * 1024, // 1MB
        idle_timeout: Duration::from_secs(30),
        tls_config: None,
        pool_config: ConnectionPoolConfig {
            max_per_ip: 10,
            rate_limit: 100,
            ban_duration: Duration::from_secs(1),
        },
        backpressure_threshold: 50,
        shutdown_timeout: Duration::from_secs(5),
        metrics_interval: Duration::from_secs(60),
    }
}

/// Send a Kafka request and receive response
async fn send_kafka_request(
    stream: &mut TcpStream,
    request: Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    // Send size header
    let size = request.len() as u32;
    stream.write_all(&size.to_be_bytes()).await?;
    
    // Send request
    stream.write_all(&request).await?;
    stream.flush().await?;
    
    // Read response size
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await?;
    let response_size = u32::from_be_bytes(size_buf) as usize;
    
    // Read response
    let mut response = vec![0u8; response_size];
    stream.read_exact(&mut response).await?;
    
    Ok(response)
}

/// Create a metadata request
fn create_metadata_request(correlation_id: i32) -> Vec<u8> {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Request header
    encoder.write_i16(3); // API key (Metadata)
    encoder.write_i16(9); // API version
    encoder.write_i32(correlation_id);
    encoder.write_string(Some("test-client")); // Client ID
    
    // Request body
    encoder.write_i32(-1); // All topics
    encoder.write_bool(false); // Don't auto-create topics
    encoder.write_bool(false); // Include cluster authorized operations
    encoder.write_bool(false); // Include topic authorized operations
    
    buf.to_vec()
}

/// Create a produce request
fn create_produce_request(
    correlation_id: i32,
    topic: &str,
    partition: i32,
    key: Option<&[u8]>,
    value: &[u8],
) -> Vec<u8> {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Request header
    encoder.write_i16(0); // API key (Produce)
    encoder.write_i16(8); // API version
    encoder.write_i32(correlation_id);
    encoder.write_string(Some("test-client")); // Client ID
    
    // Request body
    encoder.write_string(None); // Transactional ID
    encoder.write_i16(1); // Acks (1 = leader acknowledgment)
    encoder.write_i32(30000); // Timeout ms
    
    // Topics array
    encoder.write_i32(1); // One topic
    encoder.write_string(Some(topic));
    
    // Partitions array
    encoder.write_i32(1); // One partition
    encoder.write_i32(partition);
    
    // Create record batch
    let mut records_buf = BytesMut::new();
    let mut records_encoder = Encoder::new(&mut records_buf);
    
    // Record batch header
    records_encoder.write_i64(0); // Base offset
    records_encoder.write_i32(0); // Batch length (placeholder)
    records_encoder.write_i32(0); // Partition leader epoch
    records_encoder.write_i8(2); // Magic byte (v2)
    records_encoder.write_i32(0); // CRC (placeholder)
    records_encoder.write_i16(0); // Attributes
    records_encoder.write_i32(0); // Last offset delta
    records_encoder.write_i64(0); // First timestamp
    records_encoder.write_i64(0); // Max timestamp
    records_encoder.write_i64(-1); // Producer ID
    records_encoder.write_i16(-1); // Producer epoch
    records_encoder.write_i32(-1); // Base sequence
    records_encoder.write_i32(1); // Record count
    
    // Record
    records_encoder.write_i8(0); // Record attributes
    records_encoder.write_varlong(0); // Timestamp delta
    records_encoder.write_varlong(0); // Offset delta
    
    // Key
    if let Some(key_data) = key {
        records_encoder.write_varlong(key_data.len() as i64);
        records_encoder.write_bytes(Some(key_data));
    } else {
        records_encoder.write_varlong(-1);
    }
    
    // Value
    records_encoder.write_varlong(value.len() as i64);
    records_encoder.write_bytes(Some(value));
    
    // Headers
    records_encoder.write_varlong(0); // No headers
    
    let records_data = records_buf.to_vec();
    encoder.write_bytes(Some(&records_data));
    
    buf.to_vec()
}

#[tokio::test]
async fn test_server_lifecycle() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let config = test_server_config(addr);
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config, data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(100)).await;
    
    // Stop server
    server.stop().await.unwrap();
    
    // Wait for graceful shutdown
    timeout(Duration::from_secs(5), handle).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn test_metadata_request() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = test_server_config(addr);
    
    // Use a specific port for this test
    config.listen_addr = "127.0.0.1:19092".parse().unwrap();
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config.clone(), data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let _handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(500)).await;
    
    // Connect to server
    let mut stream = TcpStream::connect(config.listen_addr).await.unwrap();
    
    // Send metadata request
    let request = create_metadata_request(123);
    let response = send_kafka_request(&mut stream, request).await.unwrap();
    
    // Parse response header
    let mut response_bytes = Bytes::from(response);
    let mut decoder = Decoder::new(&mut response_bytes);
    
    let correlation_id = decoder.read_i32().unwrap();
    assert_eq!(correlation_id, 123);
    
    // Verify we got a response
    assert!(!response_bytes.is_empty());
}

#[tokio::test]
async fn test_produce_request() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = test_server_config(addr);
    
    // Use a specific port for this test
    config.listen_addr = "127.0.0.1:19093".parse().unwrap();
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config.clone(), data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let _handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(500)).await;
    
    // Connect to server
    let mut stream = TcpStream::connect(config.listen_addr).await.unwrap();
    
    // Create topic first via metadata request
    let metadata_request = create_metadata_request(1);
    let _ = send_kafka_request(&mut stream, metadata_request).await.unwrap();
    
    // Send produce request
    let produce_request = create_produce_request(
        2,
        "test-topic",
        0,
        Some(b"key1"),
        b"Hello, Chronik!",
    );
    let response = send_kafka_request(&mut stream, produce_request).await.unwrap();
    
    // Verify we got a response
    assert!(!response.is_empty());
}

#[tokio::test]
async fn test_connection_limit() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = test_server_config(addr);
    
    // Set low connection limit
    config.max_connections = 2;
    config.listen_addr = "127.0.0.1:19094".parse().unwrap();
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config.clone(), data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let _handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(500)).await;
    
    // Connect two clients
    let stream1 = TcpStream::connect(config.listen_addr).await.unwrap();
    let stream2 = TcpStream::connect(config.listen_addr).await.unwrap();
    
    // Third connection should fail or be rejected
    let result = timeout(
        Duration::from_secs(1),
        TcpStream::connect(config.listen_addr),
    )
    .await;
    
    // Connection might succeed but should be closed immediately
    if let Ok(Ok(mut stream3)) = result {
        // Try to send data - should fail
        let request = create_metadata_request(1);
        let send_result = timeout(
            Duration::from_secs(1),
            send_kafka_request(&mut stream3, request),
        )
        .await;
        
        // Should either timeout or error
        assert!(send_result.is_err() || send_result.unwrap().is_err());
    }
    
    // Keep connections alive
    drop(stream1);
    drop(stream2);
}

#[tokio::test]
async fn test_idle_timeout() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = test_server_config(addr);
    
    // Set short idle timeout
    config.idle_timeout = Duration::from_secs(2);
    config.listen_addr = "127.0.0.1:19095".parse().unwrap();
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config.clone(), data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let _handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(500)).await;
    
    // Connect to server
    let mut stream = TcpStream::connect(config.listen_addr).await.unwrap();
    
    // Send initial request
    let request = create_metadata_request(1);
    let _ = send_kafka_request(&mut stream, request.clone()).await.unwrap();
    
    // Wait for idle timeout
    sleep(Duration::from_secs(3)).await;
    
    // Try to send another request - should fail
    let result = send_kafka_request(&mut stream, request).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_concurrent_requests() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = test_server_config(addr);
    
    config.listen_addr = "127.0.0.1:19096".parse().unwrap();
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config.clone(), data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let _handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(500)).await;
    
    // Spawn multiple concurrent clients
    let mut handles = vec![];
    
    for i in 0..10 {
        let addr = config.listen_addr;
        let handle = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            
            // Send multiple requests
            for j in 0..5 {
                let request = create_metadata_request(i * 100 + j);
                let response = send_kafka_request(&mut stream, request).await.unwrap();
                assert!(!response.is_empty());
            }
        });
        
        handles.push(handle);
    }
    
    // Wait for all clients to complete
    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_graceful_shutdown() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = test_server_config(addr);
    
    config.listen_addr = "127.0.0.1:19097".parse().unwrap();
    config.shutdown_timeout = Duration::from_secs(10);
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config.clone(), data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(500)).await;
    
    // Connect clients
    let mut stream1 = TcpStream::connect(config.listen_addr).await.unwrap();
    let mut stream2 = TcpStream::connect(config.listen_addr).await.unwrap();
    
    // Start a long-running request on stream1
    let request = create_metadata_request(1);
    let _ = send_kafka_request(&mut stream1, request).await.unwrap();
    
    // Initiate shutdown
    server.stop().await.unwrap();
    
    // Server should wait for existing connections
    // Try to send on stream2 - might still work briefly
    let request2 = create_metadata_request(2);
    let _ = send_kafka_request(&mut stream2, request2).await;
    
    // Wait for server to shut down
    let shutdown_result = timeout(Duration::from_secs(15), handle).await;
    assert!(shutdown_result.is_ok());
}

#[tokio::test]
async fn test_rate_limiting() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = test_server_config(addr);
    
    // Set strict rate limit
    config.pool_config.rate_limit = 2;
    config.pool_config.ban_duration = Duration::from_secs(2);
    config.listen_addr = "127.0.0.1:19098".parse().unwrap();
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = IngestServer::new(config.clone(), data_dir.path().to_path_buf())
        .await
        .unwrap();
    
    let _handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(500)).await;
    
    // Connect multiple times quickly
    let stream1 = TcpStream::connect(config.listen_addr).await;
    let stream2 = TcpStream::connect(config.listen_addr).await;
    let stream3 = TcpStream::connect(config.listen_addr).await;
    
    // First two should succeed
    assert!(stream1.is_ok());
    assert!(stream2.is_ok());
    
    // Third might fail due to rate limit
    if stream3.is_err() {
        // Rate limit kicked in
        
        // Wait for ban to expire
        sleep(Duration::from_secs(3)).await;
        
        // Should be able to connect again
        let stream4 = TcpStream::connect(config.listen_addr).await;
        assert!(stream4.is_ok());
    }
}