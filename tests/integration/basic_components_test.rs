//! Basic component integration tests that don't require full cluster setup

use chronik_common::{Result, Error};
use chronik_storage::{
    SegmentWriter, SegmentReader, SegmentWriterConfig, SegmentReaderConfig,
    RecordBatch, Record,
    object_store::{storage::ObjectStore, backends::local::LocalObjectStore},
};
use std::sync::Arc;
use tempfile::TempDir;
use tokio;

#[tokio::test]
async fn test_segment_writer_reader_roundtrip() -> Result<()> {
    super::test_setup::init();
    
    // Create temporary directory
    let temp_dir = TempDir::new()?;
    let segments_path = temp_dir.path().join("segments");
    std::fs::create_dir_all(&segments_path)?;
    
    // Create object store
    let object_store: Arc<dyn ObjectStore> = Arc::new(
        LocalObjectStore::new(segments_path.to_str().unwrap()).await?
    );
    
    // Create segment writer
    let writer_config = SegmentWriterConfig {
        data_dir: temp_dir.path().to_path_buf(),
        compression_codec: "snappy".to_string(),
        max_segment_size: 10 * 1024 * 1024, // 10MB
    };
    
    let mut writer = SegmentWriter::new(writer_config).await?;
    
    // Write some test records
    let test_topic = "test-topic";
    let test_partition = 0;
    
    let records = vec![
        Record {
            offset: 0,
            timestamp: 1234567890,
            key: Some(b"key1".to_vec()),
            value: b"value1".to_vec(),
            headers: Default::default(),
        },
        Record {
            offset: 1,
            timestamp: 1234567891,
            key: Some(b"key2".to_vec()),
            value: b"value2".to_vec(),
            headers: Default::default(),
        },
        Record {
            offset: 2,
            timestamp: 1234567892,
            key: None,
            value: b"value3".to_vec(),
            headers: Default::default(),
        },
    ];
    
    let batch = RecordBatch { records: records.clone() };
    writer.write_batch(test_topic, test_partition, batch).await?;
    
    // Flush and get segment path
    let segment_path = writer.flush().await?;
    
    // Upload to object store
    let segment_key = format!("{}/partition-{}/segment-0000000000", test_topic, test_partition);
    let segment_data = tokio::fs::read(&segment_path).await?;
    object_store.put(&segment_key, segment_data.into()).await?;
    
    // Create segment reader
    let reader_config = SegmentReaderConfig::default();
    let reader = SegmentReader::new(reader_config, object_store.clone());
    
    // Read back the records
    let fetch_result = reader.fetch(
        test_topic,
        test_partition,
        0, // start offset
        1024, // max bytes
    ).await?;
    
    // Verify records
    assert_eq!(fetch_result.records.len(), 3);
    assert_eq!(fetch_result.high_watermark, 3);
    assert_eq!(fetch_result.log_start_offset, 0);
    
    for (i, record) in fetch_result.records.iter().enumerate() {
        assert_eq!(record.offset, i as i64);
        assert_eq!(record.value, records[i].value);
        assert_eq!(record.key, records[i].key);
    }
    
    Ok(())
}

#[tokio::test]
async fn test_metadata_store_operations() -> Result<()> {
    use chronik_common::metadata::{
        traits::MetadataStore,
        sled_store::SledMetadataStore,
        TopicMetadata, TopicConfig,
    };
    
    super::test_setup::init();
    
    // Create temporary directory
    let temp_dir = TempDir::new()?;
    let metadata_store = SledMetadataStore::new(temp_dir.path())?;
    
    // Test topic operations
    let topic_name = "test-metadata-topic";
    let topic_config = TopicConfig {
        name: topic_name.to_string(),
        partition_count: 3,
        replication_factor: 1,
        retention_ms: Some(86400000), // 1 day
        segment_ms: Some(3600000), // 1 hour
        compression_type: Some("snappy".to_string()),
        max_message_bytes: Some(1048576), // 1MB
    };
    
    // Create topic
    metadata_store.create_topic(topic_name, topic_config.clone()).await?;
    
    // Get topic
    let retrieved = metadata_store.get_topic(topic_name).await?;
    assert!(retrieved.is_some());
    let retrieved_meta = retrieved.unwrap();
    assert_eq!(retrieved_meta.config.name, topic_name);
    assert_eq!(retrieved_meta.config.partition_count, 3);
    
    // List topics
    let topics = metadata_store.list_topics().await?;
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0], topic_name);
    
    // Update topic config
    let mut updated_config = topic_config.clone();
    updated_config.retention_ms = Some(172800000); // 2 days
    metadata_store.update_topic_config(topic_name, updated_config).await?;
    
    // Verify update
    let updated = metadata_store.get_topic(topic_name).await?.unwrap();
    assert_eq!(updated.config.retention_ms, Some(172800000));
    
    // Delete topic
    metadata_store.delete_topic(topic_name).await?;
    
    // Verify deletion
    let deleted = metadata_store.get_topic(topic_name).await?;
    assert!(deleted.is_none());
    
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_management() -> Result<()> {
    use chronik_common::metadata::{
        traits::MetadataStore,
        sled_store::SledMetadataStore,
        ConsumerGroupMetadata, ConsumerOffset,
    };
    
    super::test_setup::init();
    
    // Create temporary directory
    let temp_dir = TempDir::new()?;
    let metadata_store = SledMetadataStore::new(temp_dir.path())?;
    
    let group_id = "test-consumer-group";
    let topic = "test-topic";
    
    // Create consumer group
    let group_metadata = ConsumerGroupMetadata {
        state: "Stable".to_string(),
        protocol_type: "consumer".to_string(),
        protocol: Some("range".to_string()),
        members: vec![],
    };
    
    metadata_store.update_consumer_group(group_id, group_metadata).await?;
    
    // Store offsets
    let offsets = vec![
        ConsumerOffset {
            topic: topic.to_string(),
            partition: 0,
            offset: 100,
            metadata: None,
        },
        ConsumerOffset {
            topic: topic.to_string(),
            partition: 1,
            offset: 150,
            metadata: Some("checkpoint".to_string()),
        },
    ];
    
    metadata_store.store_consumer_offsets(group_id, &offsets).await?;
    
    // Fetch offsets
    let fetched_offsets = metadata_store.fetch_consumer_offsets(group_id, Some(topic)).await?;
    assert_eq!(fetched_offsets.len(), 2);
    
    // Verify offset values
    for offset in &fetched_offsets {
        match offset.partition {
            0 => {
                assert_eq!(offset.offset, 100);
                assert!(offset.metadata.is_none());
            }
            1 => {
                assert_eq!(offset.offset, 150);
                assert_eq!(offset.metadata.as_deref(), Some("checkpoint"));
            }
            _ => panic!("Unexpected partition"),
        }
    }
    
    // List consumer groups
    let groups = metadata_store.list_consumer_groups().await?;
    assert!(groups.contains(&group_id.to_string()));
    
    Ok(())
}

#[tokio::test]
async fn test_search_index_operations() -> Result<()> {
    use chronik_search::{SearchApi, SearchRequest, MatchQuery, QueryWrapper};
    use serde_json::json;
    
    super::test_setup::init();
    
    // Create search API
    let api = SearchApi::new()?;
    
    // Create an index
    let index_name = "test-index";
    api.create_index(index_name, json!({
        "mappings": {
            "properties": {
                "title": { "type": "text" },
                "content": { "type": "text" },
                "timestamp": { "type": "date" }
            }
        }
    })).await?;
    
    // Index some documents
    let docs = vec![
        json!({
            "title": "Introduction to Chronik",
            "content": "Chronik is a distributed log storage system",
            "timestamp": "2024-01-01T00:00:00Z"
        }),
        json!({
            "title": "Kafka Protocol Support",
            "content": "Chronik supports the Kafka wire protocol",
            "timestamp": "2024-01-02T00:00:00Z"
        }),
        json!({
            "title": "Search Capabilities",
            "content": "Full-text search powered by Tantivy",
            "timestamp": "2024-01-03T00:00:00Z"
        }),
    ];
    
    for (i, doc) in docs.iter().enumerate() {
        api.index_document(index_name, &format!("doc{}", i), doc.clone()).await?;
    }
    
    // Refresh to ensure documents are searchable
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    
    // Search for documents
    let search_request = SearchRequest {
        query: Some(QueryWrapper::Match(MatchQuery {
            field: "content".to_string(),
            query: "Kafka".to_string(),
            boost: None,
        })),
        from: None,
        size: Some(10),
        sort: None,
        aggs: None,
    };
    
    let results = api.search(Some(index_name), search_request).await?;
    
    // Verify results
    assert!(results.hits.total.value > 0);
    assert!(!results.hits.hits.is_empty());
    
    // The document about Kafka should be in the results
    let kafka_doc = results.hits.hits.iter()
        .find(|hit| hit.source.get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("Kafka"))
            .unwrap_or(false));
    
    assert!(kafka_doc.is_some());
    
    // Clean up
    api.delete_index(index_name).await?;
    
    Ok(())
}

#[tokio::test]
async fn test_auth_middleware() -> Result<()> {
    use chronik_auth::{AuthMiddleware, SaslMechanism};
    
    super::test_setup::init();
    
    // Create auth middleware
    let auth = AuthMiddleware::new(false); // Allow anonymous = false
    auth.init_defaults().await?;
    
    // Create a session
    let session_id = "test-session-123";
    let context = auth.get_or_create_context(session_id).await;
    
    // Initially not authenticated
    assert!(!context.is_authenticated());
    
    // Authenticate with PLAIN mechanism
    let auth_bytes = b"\0user\0password"; // PLAIN format: \0username\0password
    let principal = auth.authenticate_sasl(
        session_id,
        &SaslMechanism::Plain,
        auth_bytes,
    ).await?;
    
    assert_eq!(principal, "user");
    
    // Now should be authenticated
    let context = auth.get_or_create_context(session_id).await;
    assert!(context.is_authenticated());
    
    // Check topic permissions
    auth.check_topic_read(session_id, "test-topic").await?;
    auth.check_topic_write(session_id, "test-topic").await?;
    
    Ok(())
}