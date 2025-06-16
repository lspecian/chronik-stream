//! Tests for request handler.

use chronik_ingest::{RequestHandler, HandlerConfig};
use chronik_protocol::{
    Request, RequestBody, MetadataRequest,
    parser::{ApiKey, RequestHeader},
};
use chronik_storage::{SegmentReader, SegmentReaderConfig, OpenDalObjectStore, ObjectStoreConfig, ObjectStoreBackend};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_metadata_request() {
    let temp_dir = TempDir::new().unwrap();
    let (tx, _rx) = mpsc::channel(100);
    
    // Create object store
    let object_store_config = ObjectStoreConfig {
        backend: ObjectStoreBackend::Local { 
            path: temp_dir.path().to_string_lossy().to_string() 
        },
        bucket: "test".to_string(),
        prefix: None,
        retry_attempts: 3,
        connection_timeout: std::time::Duration::from_secs(10),
    };
    let object_store = OpenDalObjectStore::new(object_store_config).await.unwrap();
    
    // Create segment reader
    let reader_config = SegmentReaderConfig::default();
    let segment_reader = Arc::new(SegmentReader::new(reader_config, object_store));
    
    // Create handler
    let config = HandlerConfig {
        node_id: 1,
        host: "localhost".to_string(),
        port: 9092,
        controller_addrs: vec![],
    };
    
    let handler = RequestHandler::new(config, tx, segment_reader).unwrap();
    
    // Create metadata request
    let request = Request {
        header: RequestHeader {
            api_key: ApiKey::Metadata,
            api_version: 0,
            correlation_id: 123,
            client_id: Some("test-client".to_string()),
        },
        body: RequestBody::Metadata(MetadataRequest {
            topics: Some(vec!["test-topic".to_string()]),
            allow_auto_topic_creation: false,
            include_cluster_authorized_operations: false,
            include_topic_authorized_operations: false,
        }),
    };
    
    // Handle request
    let response = handler.handle_request(request).await.unwrap();
    
    // Verify response
    match response {
        chronik_protocol::Response::Metadata(metadata) => {
            assert_eq!(metadata.correlation_id, 123);
            assert_eq!(metadata.brokers.len(), 1);
            assert_eq!(metadata.brokers[0].node_id, 1);
            assert_eq!(metadata.topics.len(), 1);
            assert_eq!(metadata.topics[0].name, "test-topic");
        }
        _ => panic!("Expected metadata response"),
    }
}