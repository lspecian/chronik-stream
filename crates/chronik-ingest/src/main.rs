//! Chronik Ingest node entry point

use chronik_ingest::{IngestServer, ServerConfig, TlsConfig, ConnectionPoolConfig};
use chronik_common::Result;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, error};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with environment filter
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "chronik_ingest=info,chronik_protocol=debug".to_string());
    
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();
    
    // Server configuration from environment
    let bind_address = std::env::var("BIND_ADDRESS")
        .unwrap_or_else(|_| "0.0.0.0:9092".to_string());
    
    let storage_path = std::env::var("STORAGE_PATH")
        .unwrap_or_else(|_| "/tmp/chronik-data".to_string());
    
    let max_connections = std::env::var("MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10000);
    
    let request_timeout_secs = std::env::var("REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    
    let idle_timeout_secs = std::env::var("IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    
    info!("Starting Chronik Ingest server on {}", bind_address);
    info!("Storage path: {}", storage_path);
    info!("Max connections: {}", max_connections);
    
    // Parse socket address
    let listen_addr = bind_address.parse()
        .expect("Invalid bind address");
    
    // TLS configuration (optional)
    let tls_config = if let Ok(cert_path) = std::env::var("TLS_CERT_PATH") {
        let key_path = std::env::var("TLS_KEY_PATH")
            .expect("TLS_KEY_PATH required when TLS_CERT_PATH is set");
        
        let ca_path = std::env::var("TLS_CA_PATH").ok();
        let require_client_cert = std::env::var("TLS_REQUIRE_CLIENT_CERT")
            .ok()
            .map(|s| s.parse().unwrap_or(false))
            .unwrap_or(false);
        
        info!("TLS enabled with cert: {}", cert_path);
        if require_client_cert {
            info!("Client certificate authentication enabled");
        }
        
        Some(TlsConfig {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            ca_path: ca_path.map(PathBuf::from),
            require_client_cert,
        })
    } else {
        None
    };
    
    // Connection pool configuration
    let pool_config = ConnectionPoolConfig {
        max_per_ip: std::env::var("MAX_CONNECTIONS_PER_IP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100),
        rate_limit: std::env::var("CONNECTION_RATE_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10),
        ban_duration: Duration::from_secs(
            std::env::var("RATE_LIMIT_BAN_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300)
        ),
    };
    
    // Create server config  
    let config = ServerConfig {
        listen_addr,
        max_connections,
        request_timeout: Duration::from_secs(request_timeout_secs),
        buffer_size: 104857600, // 100MB to handle large Kafka batches
        idle_timeout: Duration::from_secs(idle_timeout_secs),
        tls_config,
        pool_config,
        backpressure_threshold: std::env::var("BACKPRESSURE_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000),
        shutdown_timeout: Duration::from_secs(30),
        metrics_interval: Duration::from_secs(60),
    };
    
    // Create and start server
    let data_dir = PathBuf::from(storage_path);
    let server = IngestServer::new(config, data_dir).await?;
    let handle = server.start().await?;
    
    info!("Server started successfully");
    
    // Initialize metrics
    chronik_ingest::metrics::init_metrics()
        .expect("Failed to initialize metrics");
    
    // Start metrics HTTP server
    let metrics_port = std::env::var("METRICS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9093);
    
    let metrics_exporter = chronik_ingest::metrics::MetricsExporter::new();
    let metrics_handle = tokio::spawn(async move {
        if let Err(e) = metrics_exporter.start_http_server(metrics_port).await {
            error!("Metrics server error: {}", e);
        }
    });
    
    info!("Metrics server started on port {}", metrics_port);
    
    // Start health check server
    let health_port = std::env::var("HEALTH_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9094);
    
    // TODO: Add health check server setup once we have access to RequestHandler
    info!("Health check endpoints would be available on port {}", health_port);
    
    // Setup shutdown handler
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
        }
        result = handle => {
            match result {
                Ok(Ok(())) => info!("Server shut down gracefully"),
                Ok(Err(e)) => error!("Server error: {}", e),
                Err(e) => error!("Server task error: {}", e),
            }
        }
    }
    
    // Initiate graceful shutdown
    info!("Shutting down ingest server");
    server.stop().await?;
    
    Ok(())
}