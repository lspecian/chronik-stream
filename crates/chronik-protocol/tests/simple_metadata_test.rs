//! Simple metadata test to debug issues

use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::Encoder;
use chronik_protocol::ApiKey;
use chronik_common::metadata::InMemoryMetadataStore;
use std::sync::Arc;
use bytes::{Bytes, BytesMut};

/// Helper to encode Metadata request for version 0 (simplest)
fn encode_metadata_request_v0() -> Bytes {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Write request header
    encoder.write_i16(ApiKey::Metadata as i16);
    encoder.write_i16(0); // version 0
    encoder.write_i32(123); // correlation_id
    encoder.write_string(Some("test-client")); // client_id
    
    // For version 0, no request body (gets all topics)
    
    buf.freeze()
}

#[tokio::test]
async fn test_metadata_simple() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_store(metadata_store.clone());
    
    // Request metadata when no topics exist (version 0)
    let request = encode_metadata_request_v0();
    let response = handler.handle_request(&request).await.unwrap();
    
    println!("Response body length: {}", response.body.len());
    println!("Response body: {:?}", response.body);
    
    // Try to parse response manually
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // Version 0 doesn't have throttle_time_ms
    
    // brokers array
    match decoder.read_i32() {
        Ok(broker_count) => {
            println!("Broker count: {}", broker_count);
            assert!(broker_count >= 0, "Broker count should be non-negative");
        }
        Err(e) => {
            panic!("Failed to read broker count: {:?}", e);
        }
    }
}