//! Example demonstrating the Object Storage Abstraction Layer usage.

use chronik_storage::object_store::{
    ObjectStoreFactory, ObjectStoreConfig, StorageBackend, AuthConfig, S3Credentials,
    ObjectStoreTrait, PutOptions, GetOptions, ListOptions, ChronikStorageAdapter, SegmentLocation,
    PerformanceConfig, RetryConfig,
};
use chronik_storage::{
    chronik_segment::{ChronikSegmentBuilder, CompressionType},
    RecordBatch, Record,
};
use chronik_common::Bytes;
use std::collections::HashMap;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for observability
    tracing_subscriber::init();

    println!("=== Chronik Stream Object Storage Examples ===\n");

    // Example 1: Basic Local Storage
    local_storage_example().await?;

    // Example 2: S3-Compatible Storage (MinIO)
    if std::env::var("MINIO_ENDPOINT").is_ok() {
        s3_compatible_example().await?;
    } else {
        println!("Skipping S3-compatible example (set MINIO_ENDPOINT to enable)\n");
    }

    // Example 3: Advanced Configuration
    advanced_configuration_example().await?;

    // Example 4: ChronikSegment Storage Integration
    chronik_integration_example().await?;

    // Example 5: Multipart Upload
    multipart_upload_example().await?;

    println!("All examples completed successfully!");
    Ok(())
}

async fn local_storage_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Example 1: Basic Local Storage ---");

    // Create a temporary directory for this example
    let temp_dir = tempfile::tempdir()?;
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    ).with_prefix("chronik-data");

    let store = ObjectStoreFactory::create(config).await?;

    // Basic put/get operations
    let key = "documents/readme.txt";
    let content = Bytes::from("Welcome to Chronik Stream!\n\nThis is a high-performance streaming data platform.");

    store.put_with_options(
        key,
        content.clone(),
        PutOptions::default()
            .with_content_type("text/plain")
            .add_metadata("author", "chronik-team")
            .add_metadata("version", "1.0")
    ).await?;

    println!("✓ Stored document: {}", key);

    // Retrieve and verify
    let retrieved = store.get(key).await?;
    assert_eq!(retrieved, content);
    println!("✓ Retrieved document successfully");

    // Get metadata
    let metadata = store.head(key).await?;
    println!("✓ Document size: {} bytes, modified: {}", 
             metadata.size, metadata.last_modified.format("%Y-%m-%d %H:%M:%S"));

    // List operations
    store.put("documents/guide.md", Bytes::from("# User Guide")).await?;
    store.put("documents/api/reference.md", Bytes::from("# API Reference")).await?;
    store.put("config/settings.json", Bytes::from(r#"{"debug": true}"#)).await?;

    let documents = store.list("documents/").await?;
    println!("✓ Found {} documents", documents.len());

    println!("Local storage example completed!\n");
    Ok(())
}

async fn s3_compatible_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Example 2: S3-Compatible Storage (MinIO) ---");

    let endpoint = std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".to_string());
    let access_key = std::env::var("MINIO_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_string());
    let secret_key = std::env::var("MINIO_SECRET_KEY").unwrap_or_else(|_| "minioadmin".to_string());

    let config = ObjectStoreConfig::s3_compatible(
        endpoint,
        "us-east-1".to_string(),
        "chronik-test".to_string(),
        access_key,
        secret_key,
    ).with_prefix("examples");

    match ObjectStoreFactory::create(config).await {
        Ok(store) => {
            // Test basic operations
            let test_data = Bytes::from("Test data for S3-compatible storage");
            store.put("test/s3-example.txt", test_data.clone()).await?;
            
            let retrieved = store.get("test/s3-example.txt").await?;
            assert_eq!(retrieved, test_data);
            
            println!("✓ S3-compatible storage operations successful");

            // Test presigned URLs (if supported)
            match store.presign_get("test/s3-example.txt", Duration::from_secs(3600)).await {
                Ok(url) => println!("✓ Generated presigned URL: {}", url),
                Err(_) => println!("ℹ Presigned URLs not supported for this backend"),
            }
        }
        Err(e) => {
            println!("⚠ S3-compatible storage not available: {}", e);
            println!("  Make sure MinIO is running and credentials are correct");
        }
    }

    println!("S3-compatible example completed!\n");
    Ok(())
}

async fn advanced_configuration_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Example 3: Advanced Configuration ---");

    let temp_dir = tempfile::tempdir()?;
    
    // Create a highly customized configuration
    let config = ObjectStoreConfig {
        backend: StorageBackend::Local {
            path: temp_dir.path().to_string_lossy().to_string(),
        },
        bucket: "advanced-example".to_string(),
        prefix: Some("chronik/v1".to_string()),
        connection: chronik_storage::object_store::ConnectionConfig {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            max_connections_per_host: 20,
            ..Default::default()
        },
        performance: PerformanceConfig {
            buffer_size: 128 * 1024, // 128KB
            multipart_threshold: 32 * 1024 * 1024, // 32MB
            multipart_part_size: 16 * 1024 * 1024, // 16MB
            max_concurrent_uploads: 8,
            enable_compression: true,
            compression_level: 6,
            enable_caching: true,
            cache_size_limit: 256 * 1024 * 1024, // 256MB
            cache_ttl: Duration::from_secs(7200), // 2 hours
        },
        retry: RetryConfig {
            max_attempts: 5,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.5,
            jitter_factor: 0.2,
            timeout: Duration::from_secs(300), // 5 minutes
        },
        auth: AuthConfig::None,
        default_metadata: Some([
            ("service".to_string(), "chronik-stream".to_string()),
            ("environment".to_string(), "example".to_string()),
        ].into_iter().collect()),
        encryption: None,
    };

    // Validate configuration
    config.validate()?;
    println!("✓ Configuration validation passed");

    let store = ObjectStoreFactory::create(config).await?;

    // Test with advanced options
    let large_data = Bytes::from(vec![0u8; 1024 * 1024]); // 1MB of zeros
    
    store.put_with_options(
        "large/data.bin",
        large_data.clone(),
        PutOptions::binary()
            .add_metadata("size_mb", "1")
            .add_metadata("type", "binary")
    ).await?;

    println!("✓ Stored large binary data with metadata");

    // Test range get
    let partial = store.get_range("large/data.bin", 0, 1023).await?;
    assert_eq!(partial.len(), 1024);
    println!("✓ Retrieved partial data (1KB from 1MB file)");

    println!("Advanced configuration example completed!\n");
    Ok(())
}

async fn chronik_integration_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Example 4: ChronikSegment Storage Integration ---");

    let temp_dir = tempfile::tempdir()?;
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    ).with_prefix("segments");

    let store = ObjectStoreFactory::create(config).await?;
    let adapter = ChronikStorageAdapter::with_cache(store, 50); // Cache up to 50 segments

    // Create sample streaming data
    let topic = "user-events".to_string();
    let partition_id = 0;
    let base_offset = 10000i64;

    let events = vec![
        ("user:alice", "login", r#"{"ip": "192.168.1.100", "browser": "Chrome"}"#),
        ("user:bob", "purchase", r#"{"item_id": "book-123", "amount": 29.99}"#),
        ("user:alice", "view", r#"{"page": "/products", "duration": 45}"#),
        ("user:charlie", "signup", r#"{"email": "charlie@example.com", "source": "organic"}"#),
        ("user:bob", "logout", r#"{"session_duration": 1847}"#),
    ];

    let records: Vec<Record> = events.into_iter().enumerate().map(|(i, (user, event, data))| {
        Record {
            offset: base_offset + i as i64,
            timestamp: 1640995200000 + (i as i64 * 1000), // 1 second apart
            key: Some(user.as_bytes().to_vec()),
            value: data.as_bytes().to_vec(),
            headers: [("event_type".to_string(), event.as_bytes().to_vec())]
                .into_iter().collect(),
        }
    }).collect();

    let batch = RecordBatch { records };

    // Build and store segment
    let segment = ChronikSegmentBuilder::new(topic.clone(), partition_id)
        .add_batch(batch)
        .compression(CompressionType::Gzip)
        .with_index(true)
        .with_bloom_filter(true)
        .build()?;

    let location = SegmentLocation::new(topic.clone(), partition_id, base_offset);
    adapter.store_segment(&location, segment).await?;
    
    println!("✓ Stored ChronikSegment with {} events", 5);

    // Retrieve and analyze
    let loaded_segment = adapter.load_segment(&location).await?;
    println!("✓ Loaded segment: {} records, compression ratio: {:.1}%",
             loaded_segment.metadata().record_count,
             loaded_segment.metadata().compression_ratio * 100.0);

    // Test bloom filter
    println!("✓ Bloom filter tests:");
    println!("  - might contain 'user:alice': {}", loaded_segment.might_contain_key(b"user:alice"));
    println!("  - might contain 'user:xyz': {}", loaded_segment.might_contain_key(b"user:xyz"));

    // Get segment statistics
    let stats = adapter.segment_stats(&location).await?;
    println!("✓ Segment stats: {} bytes stored, {} records",
             stats.storage_size, stats.record_count);

    // List segments
    let segments = adapter.list_segments(&topic, Some(partition_id)).await?;
    println!("✓ Found {} segments for topic '{}'", segments.len(), topic);

    println!("ChronikSegment integration example completed!\n");
    Ok(())
}

async fn multipart_upload_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Example 5: Multipart Upload ---");

    let temp_dir = tempfile::tempdir()?;
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await?;

    let key = "large-files/dataset.bin";
    
    // Start multipart upload
    let upload = store.start_multipart_upload(key).await?;
    println!("✓ Started multipart upload: {}", upload.upload_id);

    // Simulate uploading a large file in chunks
    let chunk_size = 1024 * 1024; // 1MB chunks
    let total_chunks = 5;
    let mut parts = Vec::new();

    for i in 1..=total_chunks {
        // Create test data for this chunk
        let chunk_data = Bytes::from(vec![i as u8; chunk_size]);
        
        let part = store.upload_part(&upload, i, chunk_data).await?;
        parts.push(part);
        
        println!("✓ Uploaded part {} ({} bytes)", i, chunk_size);
    }

    // Complete the upload
    store.complete_multipart_upload(&upload, parts).await?;
    println!("✓ Completed multipart upload");

    // Verify the assembled file
    let final_data = store.get(key).await?;
    assert_eq!(final_data.len(), chunk_size * total_chunks);
    println!("✓ Verified assembled file: {} bytes", final_data.len());

    // Check that different parts have different content
    assert_eq!(final_data[0], 1);
    assert_eq!(final_data[chunk_size], 2);
    assert_eq!(final_data[chunk_size * 2], 3);
    println!("✓ Verified chunk integrity");

    println!("Multipart upload example completed!\n");
    Ok(())
}

// Helper function to demonstrate error handling patterns
async fn _error_handling_example() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = tempfile::tempdir()?;
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await?;

    // Handle different types of errors
    match store.get("nonexistent-file.txt").await {
        Ok(_) => println!("Unexpected success"),
        Err(chronik_storage::object_store::ObjectStoreError::NotFound { key }) => {
            println!("Expected error: File '{}' not found", key);
        }
        Err(e) => {
            println!("Unexpected error: {}", e);
        }
    }

    Ok(())
}