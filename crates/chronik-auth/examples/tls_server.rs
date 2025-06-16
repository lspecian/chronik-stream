//! Example TLS server with authentication.

use anyhow::Result;
use chronik_auth::{
    TlsConfig, TlsAcceptor, SaslAuthenticator, SaslMechanism,
    Acl, Permission, Resource, Operation,
};
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, error};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    
    // Create TLS configuration
    let tls_config = TlsConfig {
        cert_file: "server.crt".to_string(),
        key_file: "server.key".to_string(),
        ca_file: None,
        verify_client: false,
        min_version: "1.2".to_string(),
    };
    
    // Create TLS acceptor
    let tls_acceptor = match TlsAcceptor::new(&tls_config) {
        Ok(acceptor) => acceptor,
        Err(e) => {
            error!("Failed to create TLS acceptor: {:?}", e);
            info!("To generate self-signed certificates for testing:");
            info!("openssl req -x509 -newkey rsa:4096 -keyout server.key -out server.crt -days 365 -nodes -subj '/CN=localhost'");
            return Ok(());
        }
    };
    
    // Create SASL authenticator
    let mut sasl_auth = SaslAuthenticator::new();
    sasl_auth.add_user("alice".to_string(), "password123".to_string())?;
    sasl_auth.add_user("bob".to_string(), "secret456".to_string())?;
    
    // Create ACL
    let acl = Acl::new();
    acl.init_kafka_defaults()?;
    
    // Add specific permissions
    acl.add_user_permission("alice", Permission {
        resource: Resource::Topic("sensitive-topic".to_string()),
        operation: Operation::Write,
        allow: true,
    })?;
    
    // Start TLS server
    let listener = TcpListener::bind("127.0.0.1:9093").await?;
    info!("TLS server listening on 127.0.0.1:9093");
    
    loop {
        let (stream, addr) = listener.accept().await?;
        info!("New connection from {}", addr);
        
        let tls_stream = match tls_acceptor.inner().accept(stream).await {
            Ok(stream) => stream,
            Err(e) => {
                error!("TLS handshake failed: {}", e);
                continue;
            }
        };
        
        // Handle the connection
        tokio::spawn(async move {
            if let Err(e) = handle_connection(tls_stream).await {
                error!("Connection error: {}", e);
            }
        });
    }
}

async fn handle_connection<S>(mut stream: S) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Simple echo server
    let mut buf = [0u8; 1024];
    
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        
        stream.write_all(&buf[..n]).await?;
    }
    
    Ok(())
}