//! Integration tests for produce and fetch.

use chronik_ingest::{IngestServer, ServerConfig};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

#[tokio::test]
async fn test_produce_and_fetch() {
    let temp_dir = TempDir::new().unwrap();
    
    // Start server
    let config = ServerConfig {
        listen_addr: "127.0.0.1:19093".parse().unwrap(),
        max_connections: 100,
        request_timeout: Duration::from_secs(5),
        buffer_size: 8192,
    };
    
    let server = IngestServer::new(config, temp_dir.path().to_path_buf()).await.unwrap();
    let _handle = server.start().await.unwrap();
    
    // Give server time to start
    sleep(Duration::from_millis(100)).await;
    
    // Connect to server
    let mut stream = TcpStream::connect("127.0.0.1:19093").await.unwrap();
    
    // Send metadata request
    let metadata_request = build_metadata_request();
    send_request(&mut stream, &metadata_request).await.unwrap();
    
    // Read response
    let response = read_response(&mut stream).await.unwrap();
    assert!(!response.is_empty());
    
    // TODO: Add actual produce and fetch tests once protocol encoding is complete
    
    // Stop server
    server.stop().await.unwrap();
}

fn build_metadata_request() -> Vec<u8> {
    let mut request = Vec::new();
    
    // API key (Metadata = 3)
    request.extend_from_slice(&3i16.to_be_bytes());
    // API version
    request.extend_from_slice(&0i16.to_be_bytes());
    // Correlation ID
    request.extend_from_slice(&1i32.to_be_bytes());
    // Client ID (null)
    request.extend_from_slice(&(-1i16).to_be_bytes());
    
    // Topics array (null = all topics)
    request.extend_from_slice(&(-1i32).to_be_bytes());
    
    request
}

async fn send_request(stream: &mut TcpStream, request: &[u8]) -> Result<(), std::io::Error> {
    // Write size
    stream.write_all(&(request.len() as u32).to_be_bytes()).await?;
    // Write request
    stream.write_all(request).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_response(stream: &mut TcpStream) -> Result<Vec<u8>, std::io::Error> {
    // Read size
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await?;
    let size = u32::from_be_bytes(size_buf) as usize;
    
    // Read response
    let mut response = vec![0u8; size];
    stream.read_exact(&mut response).await?;
    
    Ok(response)
}