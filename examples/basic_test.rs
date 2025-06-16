//! Basic test to verify Chronik Stream functionality

use chronik_protocol::{ProtocolHandler, Encoder, parser::ApiKey};
use chronik_storage::{ObjectStoreConfig, LocalObjectStore};
use bytes::BytesMut;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing Chronik Stream basic functionality...\n");

    // Test 1: Protocol Handler
    println!("1. Testing Protocol Handler:");
    let handler = ProtocolHandler::new();
    
    // Build an ApiVersions request
    let mut request_buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut request_buf);
    
    encoder.write_i16(ApiKey::ApiVersions as i16);
    encoder.write_i16(3);
    encoder.write_i32(42);
    encoder.write_string(Some("test-client"));
    
    match handler.handle_request(&request_buf).await {
        Ok(response) => {
            println!("   ✓ Protocol handler working - correlation_id: {}", response.header.correlation_id);
        }
        Err(e) => {
            println!("   ✗ Protocol handler failed: {}", e);
        }
    }

    // Test 2: Storage
    println!("\n2. Testing Storage:");
    let storage_config = ObjectStoreConfig::Local {
        path: "/tmp/chronik-test".into(),
    };
    
    match LocalObjectStore::new(storage_config).await {
        Ok(storage) => {
            // Test write
            let test_data = b"Hello, Chronik!";
            if let Err(e) = storage.put("test/file.txt", test_data.to_vec().into()).await {
                println!("   ✗ Storage write failed: {}", e);
            } else {
                println!("   ✓ Storage write successful");
                
                // Test read
                match storage.get("test/file.txt").await {
                    Ok(data) => {
                        let content = String::from_utf8_lossy(&data);
                        if content == "Hello, Chronik!" {
                            println!("   ✓ Storage read successful");
                        } else {
                            println!("   ✗ Storage read returned wrong data");
                        }
                    }
                    Err(e) => {
                        println!("   ✗ Storage read failed: {}", e);
                    }
                }
            }
        }
        Err(e) => {
            println!("   ✗ Storage creation failed: {}", e);
        }
    }

    // Test 3: Database Connection
    println!("\n3. Testing Database Connection:");
    match sqlx::PgPool::connect("postgres://chronik:chronik@localhost/chronik").await {
        Ok(pool) => {
            println!("   ✓ Database connection successful");
            
            // Test query
            match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM topics")
                .fetch_one(&pool)
                .await {
                Ok(count) => {
                    println!("   ✓ Database query successful - topics count: {}", count);
                }
                Err(e) => {
                    println!("   ✗ Database query failed: {}", e);
                }
            }
        }
        Err(e) => {
            println!("   ✗ Database connection failed: {}", e);
        }
    }

    println!("\nBasic functionality test complete!");
    
    Ok(())
}