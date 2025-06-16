//! Enhanced object storage tests for the new abstraction layer.

use chronik_storage::object_store::{
    ObjectStoreFactory, ObjectStoreConfig, StorageBackend, 
    AuthConfig, S3Credentials, ObjectStore as ObjectStoreTrait,
    PutOptions, GetOptions, ListOptions,
};
use chronik_common::Bytes;
use tempfile::TempDir;

#[tokio::test]
async fn test_local_backend_complete_operations() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    ).with_prefix("test-prefix");

    let store = ObjectStoreFactory::create(config).await.unwrap();

    // Test basic put/get
    let key = "test/document.json";
    let data = Bytes::from(r#"{"message": "Hello, World!"}"#);
    
    store.put_with_options(
        key, 
        data.clone(), 
        PutOptions::json().add_metadata("author", "test")
    ).await.unwrap();

    let retrieved = store.get_with_options(key, GetOptions::default()).await.unwrap();
    assert_eq!(retrieved, data);

    // Test range get
    let range_data = store.get_range(key, 2, 7).await.unwrap();
    assert_eq!(range_data, Bytes::from("message"));

    // Test head operation
    let metadata = store.head(key).await.unwrap();
    assert_eq!(metadata.size, data.len() as u64);
    assert_eq!(metadata.key, key);

    // Test exists
    assert!(store.exists(key).await.unwrap());
    assert!(!store.exists("nonexistent").await.unwrap());

    // Test list operations
    store.put("test/file1.txt", Bytes::from("data1")).await.unwrap();
    store.put("test/file2.txt", Bytes::from("data2")).await.unwrap();
    store.put("other/file3.txt", Bytes::from("data3")).await.unwrap();

    let objects = store.list_with_options("test/", ListOptions::default()).await.unwrap();
    assert!(objects.len() >= 3); // Should include document.json, file1.txt, file2.txt

    // Test copy operation
    let copy_key = "test/document_copy.json";
    store.copy(key, copy_key).await.unwrap();
    
    let copied_data = store.get(copy_key).await.unwrap();
    assert_eq!(copied_data, data);

    // Test delete
    store.delete(copy_key).await.unwrap();
    assert!(!store.exists(copy_key).await.unwrap());

    // Test batch delete
    let keys_to_delete = vec!["test/file1.txt".to_string(), "test/file2.txt".to_string()];
    let delete_results = store.delete_batch(&keys_to_delete).await.unwrap();
    assert_eq!(delete_results.len(), 2);
    assert!(delete_results.iter().all(|r| r.is_ok()));
}

#[tokio::test]
async fn test_multipart_upload_local() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();

    let key = "large_file.bin";
    
    // Start multipart upload
    let upload = store.start_multipart_upload(key).await.unwrap();
    assert!(!upload.upload_id.is_empty());
    assert_eq!(upload.key, key);

    // Upload parts
    let part1_data = Bytes::from(vec![1u8; 1024]);
    let part2_data = Bytes::from(vec![2u8; 1024]);
    let part3_data = Bytes::from(vec![3u8; 1024]);

    let part1 = store.upload_part(&upload, 1, part1_data.clone()).await.unwrap();
    let part2 = store.upload_part(&upload, 2, part2_data.clone()).await.unwrap();
    let part3 = store.upload_part(&upload, 3, part3_data.clone()).await.unwrap();

    assert_eq!(part1.part_number, 1);
    assert_eq!(part2.part_number, 2);
    assert_eq!(part3.part_number, 3);

    // Complete upload
    let parts = vec![part1, part2, part3];
    store.complete_multipart_upload(&upload, parts).await.unwrap();

    // Verify the assembled file
    let final_data = store.get(key).await.unwrap();
    assert_eq!(final_data.len(), 3072);
    assert!(final_data[..1024].iter().all(|&b| b == 1));
    assert!(final_data[1024..2048].iter().all(|&b| b == 2));
    assert!(final_data[2048..].iter().all(|&b| b == 3));
}

#[tokio::test]
async fn test_multipart_upload_abort() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();

    let key = "aborted_file.bin";
    
    // Start multipart upload
    let upload = store.start_multipart_upload(key).await.unwrap();
    
    // Upload a part
    let part_data = Bytes::from(vec![1u8; 1024]);
    store.upload_part(&upload, 1, part_data).await.unwrap();

    // Abort the upload
    store.abort_multipart_upload(&upload).await.unwrap();

    // File should not exist
    assert!(!store.exists(key).await.unwrap());
}

#[tokio::test]
async fn test_configuration_validation() {
    // Test valid configuration
    let valid_config = ObjectStoreConfig::local("/tmp/test".to_string());
    assert!(valid_config.validate().is_ok());

    // Test invalid configurations
    let mut invalid_config = valid_config.clone();
    invalid_config.bucket = "".to_string();
    assert!(invalid_config.validate().is_err());

    invalid_config.bucket = "test".to_string();
    invalid_config.performance.multipart_threshold = 1024; // Too small
    assert!(invalid_config.validate().is_err());

    invalid_config.performance.multipart_threshold = 16 * 1024 * 1024;
    invalid_config.retry.max_attempts = 0;
    assert!(invalid_config.validate().is_err());
}

#[tokio::test]
async fn test_s3_compatible_config_creation() {
    // Test S3-compatible configuration (MinIO style)
    let config = ObjectStoreConfig::s3_compatible(
        "http://localhost:9000".to_string(),
        "us-east-1".to_string(),
        "test-bucket".to_string(),
        "minioadmin".to_string(),
        "minioadmin".to_string(),
    );

    assert_eq!(config.bucket, "test-bucket");
    
    match config.backend {
        StorageBackend::S3 { 
            region, 
            endpoint, 
            force_path_style, 
            use_virtual_hosted_style,
            .. 
        } => {
            assert_eq!(region, "us-east-1");
            assert_eq!(endpoint, Some("http://localhost:9000".to_string()));
            assert!(force_path_style);
            assert!(!use_virtual_hosted_style);
        }
        _ => panic!("Expected S3 backend"),
    }

    match config.auth {
        AuthConfig::S3(S3Credentials::AccessKey { 
            access_key_id, 
            secret_access_key, 
            .. 
        }) => {
            assert_eq!(access_key_id, "minioadmin");
            assert_eq!(secret_access_key, "minioadmin");
        }
        _ => panic!("Expected S3 access key credentials"),
    }
}

#[tokio::test]
async fn test_put_options_variations() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();

    // Test JSON content type
    let json_data = Bytes::from(r#"{"type": "json"}"#);
    store.put_with_options(
        "test.json",
        json_data.clone(),
        PutOptions::json()
    ).await.unwrap();

    // Test binary content type
    let binary_data = Bytes::from(vec![0xFF, 0xFE, 0xFD, 0xFC]);
    store.put_with_options(
        "test.bin",
        binary_data.clone(),
        PutOptions::binary()
    ).await.unwrap();

    // Test with custom metadata
    let metadata_data = Bytes::from("data with metadata");
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("version".to_string(), "1.0".to_string());
    metadata.insert("author".to_string(), "test".to_string());
    
    store.put_with_options(
        "test_metadata.txt",
        metadata_data.clone(),
        PutOptions::default()
            .with_content_type("text/plain")
            .with_metadata(metadata)
    ).await.unwrap();

    // Verify all files exist
    assert!(store.exists("test.json").await.unwrap());
    assert!(store.exists("test.bin").await.unwrap());
    assert!(store.exists("test_metadata.txt").await.unwrap());

    // Verify content
    assert_eq!(store.get("test.json").await.unwrap(), json_data);
    assert_eq!(store.get("test.bin").await.unwrap(), binary_data);
    assert_eq!(store.get("test_metadata.txt").await.unwrap(), metadata_data);
}

#[tokio::test]
async fn test_list_options_variations() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();

    // Create test files
    let test_files = vec![
        "docs/readme.txt",
        "docs/guide.md",
        "docs/api/overview.md", 
        "docs/api/reference.md",
        "src/main.rs",
        "src/lib.rs",
    ];

    for file in &test_files {
        store.put(file, Bytes::from(format!("content of {}", file))).await.unwrap();
    }

    // Test basic listing
    let all_docs = store.list("docs/").await.unwrap();
    assert!(all_docs.len() >= 4);

    // Test with limit
    let limited = store.list_with_options(
        "docs/",
        ListOptions::default().with_limit(2)
    ).await.unwrap();
    assert!(limited.len() <= 2);

    // Test recursive listing
    let recursive = store.list_with_options(
        "docs/",
        ListOptions::recursive()
    ).await.unwrap();
    assert!(recursive.len() >= 4);

    // Test with delimiter (simulate directory listing)
    let with_delimiter = store.list_with_options(
        "",
        ListOptions::default().with_delimiter("/")
    ).await.unwrap();
    // Should include both individual files and "directories"
    assert!(!with_delimiter.is_empty());
}

#[tokio::test] 
async fn test_error_handling() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();

    // Test getting non-existent file
    let result = store.get("nonexistent.txt").await;
    assert!(result.is_err());
    
    // Test deleting non-existent file
    let result = store.delete("nonexistent.txt").await;
    assert!(result.is_err());

    // Test head on non-existent file
    let result = store.head("nonexistent.txt").await;
    assert!(result.is_err());

    // Test copy from non-existent file
    let result = store.copy("nonexistent.txt", "copy.txt").await;
    assert!(result.is_err());
}

// Note: These tests would require actual cloud storage credentials and 
// are typically run in CI/CD environments with proper setup
#[tokio::test]
#[ignore = "requires AWS credentials"]
async fn test_s3_backend_integration() {
    // This test would require actual AWS S3 credentials
    let config = ObjectStoreConfig::s3(
        "us-east-1".to_string(),
        "test-bucket".to_string(),
    );

    // In a real test environment, this would connect to actual S3
    let _store = ObjectStoreFactory::create(config).await;
    // Test operations would go here...
}

#[tokio::test]
#[ignore = "requires GCS credentials"]
async fn test_gcs_backend_integration() {
    let config = ObjectStoreConfig::gcs(
        "test-bucket".to_string(),
        Some("test-project".to_string()),
    );

    // In a real test environment, this would connect to actual GCS
    let _store = ObjectStoreFactory::create(config).await;
    // Test operations would go here...
}

#[tokio::test]
#[ignore = "requires Azure credentials"]
async fn test_azure_backend_integration() {
    let config = ObjectStoreConfig::azure(
        "testaccount".to_string(),
        "test-container".to_string(),
    );

    // In a real test environment, this would connect to actual Azure Blob Storage
    let _store = ObjectStoreFactory::create(config).await;
    // Test operations would go here...
}