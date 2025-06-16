//! Example of running the Chronik Ingest server with TLS

use chronik_ingest::{IngestServer, ServerConfig, TlsConfig, ConnectionPoolConfig};
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;

/// Generate self-signed certificates for testing
/// In production, use proper certificates from a CA
async fn generate_test_certificates() -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    
    let subject_alt_names = vec!["localhost".to_string()];
    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(subject_alt_names)?;
    
    // Write certificate
    let cert_path = PathBuf::from("/tmp/chronik-cert.pem");
    std::fs::write(&cert_path, cert.serialize_pem()?)?;
    
    // Write private key
    let key_path = PathBuf::from("/tmp/chronik-key.pem");
    std::fs::write(&key_path, key_pair.serialize_pem())?;
    
    Ok((cert_path, key_path))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("chronik_ingest=debug,chronik_protocol=debug")
        .init();
    
    // Generate test certificates
    let (cert_path, key_path) = generate_test_certificates().await?;
    info!("Generated test certificates at {:?} and {:?}", cert_path, key_path);
    
    // Create TLS configuration
    let tls_config = TlsConfig {
        cert_path: cert_path.clone(),
        key_path: key_path.clone(),
        ca_path: None,
        require_client_cert: false,
    };
    
    // Create server configuration
    let config = ServerConfig {
        listen_addr: "0.0.0.0:9093".parse()?,
        max_connections: 1000,
        request_timeout: Duration::from_secs(30),
        buffer_size: 104857600, // 100MB
        idle_timeout: Duration::from_secs(600),
        tls_config: Some(tls_config),
        pool_config: ConnectionPoolConfig {
            max_per_ip: 50,
            rate_limit: 10,
            ban_duration: Duration::from_secs(300),
        },
        backpressure_threshold: 500,
        shutdown_timeout: Duration::from_secs(30),
        metrics_interval: Duration::from_secs(60),
    };
    
    // Create and start server
    let data_dir = PathBuf::from("/tmp/chronik-tls-data");
    std::fs::create_dir_all(&data_dir)?;
    
    info!("Starting TLS-enabled Chronik Ingest server on {}", config.listen_addr);
    info!("Data directory: {:?}", data_dir);
    
    let server = IngestServer::new(config, data_dir).await?;
    let handle = server.start().await?;
    
    info!("Server started successfully with TLS enabled");
    info!("To test with kafka-console-producer:");
    info!("  kafka-console-producer.sh \\");
    info!("    --broker-list localhost:9093 \\");
    info!("    --topic test-topic \\");
    info!("    --producer-property security.protocol=SSL \\");
    info!("    --producer-property ssl.truststore.location=/path/to/truststore.jks \\");
    info!("    --producer-property ssl.truststore.password=password");
    
    // Wait for shutdown signal
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
        }
        result = handle => {
            match result {
                Ok(Ok(())) => info!("Server shut down gracefully"),
                Ok(Err(e)) => eprintln!("Server error: {}", e),
                Err(e) => eprintln!("Server task error: {}", e),
            }
        }
    }
    
    // Cleanup
    server.stop().await?;
    
    // Remove test certificates
    let _ = std::fs::remove_file(cert_path);
    let _ = std::fs::remove_file(key_path);
    
    Ok(())
}