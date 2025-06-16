//! Tests for TCP server.

use chronik_ingest::{IngestServer, ServerConfig};
use std::net::SocketAddr;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_server_start_stop() {
    let config = ServerConfig {
        listen_addr: "127.0.0.1:19092".parse().unwrap(),
        max_connections: 100,
        request_timeout: Duration::from_secs(5),
        buffer_size: 8192,
    };
    
    let temp_dir = tempfile::TempDir::new().unwrap();
    let server = IngestServer::new(config, temp_dir.path().to_path_buf()).await.unwrap();
    
    // Start server
    let handle = server.start().await.unwrap();
    
    // Give it time to start
    sleep(Duration::from_millis(100)).await;
    
    // Stop server
    server.stop().await.unwrap();
    
    // Wait for task to complete
    let _ = handle.await;
}