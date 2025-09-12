use super::*;
use chronik_storage::Record;
use tempfile::TempDir;
use std::collections::HashMap;

#[tokio::test]
async fn test_buffer_high_watermark_calculation() {
    let temp_dir = TempDir::new().unwrap();
    
    let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
        temp_dir.path().join("metadata")
    ).await.unwrap());
    
    let object_store = Arc::new(chronik_storage::object_store::LocalObjectStore::new(
        temp_dir.path().join("segments")
    ).await.unwrap());
    
    let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
        object_store.clone(),
        object_store.clone()
    ));
    
    let handler = FetchHandler::new(
        segment_reader,
        metadata_store.clone(),
        object_store,
    );
    
    // Create topic
    metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
    
    // Test 1: Empty buffer and no segments - high watermark should be 0
    let response = handler.fetch_partition(
        "test-topic",
        0,
        0,    // fetch_offset
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 0, "Empty buffer should have high watermark 0");
    assert!(response.records.is_empty(), "Empty buffer should return no records");
    
    // Test 2: Add records to buffer and verify high watermark updates
    let records = vec![
        Record {
            offset: 0,
            timestamp: 1000,
            key: None,
            value: b"msg1".to_vec(),
            headers: vec![],
        },
        Record {
            offset: 1,
            timestamp: 1001,
            key: None,
            value: b"msg2".to_vec(),
            headers: vec![],
        },
        Record {
            offset: 2,
            timestamp: 1002,
            key: None,
            value: b"msg3".to_vec(),
            headers: vec![],
        },
    ];
    
    handler.update_buffer("test-topic", 0, records.clone(), 3).await.unwrap();
    
    // Test 3: Fetch with buffer containing data
    let response = handler.fetch_partition(
        "test-topic",
        0,
        0,    // fetch_offset
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 3, "Buffer high watermark should be 3");
    assert!(!response.records.is_empty(), "Should return buffered records");
}

#[tokio::test]
async fn test_fetch_from_buffer_only() {
    let temp_dir = TempDir::new().unwrap();
    
    let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
        temp_dir.path().join("metadata")
    ).await.unwrap());
    
    let object_store = Arc::new(chronik_storage::object_store::LocalObjectStore::new(
        temp_dir.path().join("segments")
    ).await.unwrap());
    
    let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
        object_store.clone(),
        object_store.clone()
    ));
    
    let handler = FetchHandler::new(
        segment_reader,
        metadata_store.clone(),
        object_store,
    );
    
    metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
    
    // Add records only to buffer (no segments)
    let records = vec![
        Record {
            offset: 0,
            timestamp: 1000,
            key: Some(b"key1".to_vec()),
            value: b"value1".to_vec(),
            headers: vec![],
        },
        Record {
            offset: 1,
            timestamp: 1001,
            key: Some(b"key2".to_vec()),
            value: b"value2".to_vec(),
            headers: vec![],
        },
    ];
    
    handler.update_buffer("test-topic", 0, records.clone(), 2).await.unwrap();
    
    // Fetch from offset 0 - should get both records from buffer
    let response = handler.fetch_partition(
        "test-topic",
        0,
        0,    // fetch_offset
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 2);
    assert!(!response.records.is_empty());
    
    // Fetch from offset 1 - should get only second record
    let response = handler.fetch_partition(
        "test-topic",
        0,
        1,    // fetch_offset
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 2);
    assert!(!response.records.is_empty());
    
    // Fetch from offset 2 - should get no records (at high watermark)
    let response = handler.fetch_partition(
        "test-topic",
        0,
        2,    // fetch_offset
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 2);
    assert!(response.records.is_empty(), "Fetching at high watermark should return no records");
}

#[tokio::test]
async fn test_buffer_with_segment_high_watermark() {
    let temp_dir = TempDir::new().unwrap();
    
    let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
        temp_dir.path().join("metadata")
    ).await.unwrap());
    
    let object_store = Arc::new(chronik_storage::object_store::LocalObjectStore::new(
        temp_dir.path().join("segments")
    ).await.unwrap());
    
    let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
        object_store.clone(),
        object_store.clone()
    ));
    
    let handler = FetchHandler::new(
        segment_reader,
        metadata_store.clone(),
        object_store.clone(),
    );
    
    metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
    
    // Create a segment with some data (simulating flushed data)
    let segment_id = uuid::Uuid::new_v4().to_string();
    metadata_store.add_segment(
        "test-topic",
        0,
        segment_id.clone(),
        0,  // start_offset
        4,  // end_offset (5 messages: 0-4)
        format!("segments/test-topic/0/{}.seg", segment_id),
        1000,
    ).await.unwrap();
    
    // Add records to buffer (simulating new unflushed data)
    let buffer_records = vec![
        Record {
            offset: 5,
            timestamp: 2000,
            key: None,
            value: b"buffered_msg1".to_vec(),
            headers: vec![],
        },
        Record {
            offset: 6,
            timestamp: 2001,
            key: None,
            value: b"buffered_msg2".to_vec(),
            headers: vec![],
        },
    ];
    
    handler.update_buffer("test-topic", 0, buffer_records, 7).await.unwrap();
    
    // Fetch should use maximum of segment high watermark (5) and buffer high watermark (7)
    let response = handler.fetch_partition(
        "test-topic",
        0,
        5,    // fetch_offset - start from after segment
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 7, "Should use maximum of segment and buffer high watermarks");
    assert!(!response.records.is_empty(), "Should return buffered records");
}

#[tokio::test]
async fn test_out_of_order_fetch() {
    let temp_dir = TempDir::new().unwrap();
    
    let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
        temp_dir.path().join("metadata")
    ).await.unwrap());
    
    let object_store = Arc::new(chronik_storage::object_store::LocalObjectStore::new(
        temp_dir.path().join("segments")
    ).await.unwrap());
    
    let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
        object_store.clone(),
        object_store.clone()
    ));
    
    let handler = FetchHandler::new(
        segment_reader,
        metadata_store.clone(),
        object_store,
    );
    
    metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
    
    // Add records to buffer
    let records = vec![
        Record {
            offset: 10,
            timestamp: 1000,
            key: None,
            value: b"msg10".to_vec(),
            headers: vec![],
        },
        Record {
            offset: 11,
            timestamp: 1001,
            key: None,
            value: b"msg11".to_vec(),
            headers: vec![],
        },
        Record {
            offset: 12,
            timestamp: 1002,
            key: None,
            value: b"msg12".to_vec(),
            headers: vec![],
        },
    ];
    
    handler.update_buffer("test-topic", 0, records, 13).await.unwrap();
    
    // Test fetching from before the buffer's base offset
    let response = handler.fetch_partition(
        "test-topic",
        0,
        0,    // fetch_offset - before buffer base
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 13);
    // Should handle gracefully even if fetch offset is before buffer
    
    // Test fetching from middle of buffer
    let response = handler.fetch_partition(
        "test-topic",
        0,
        11,   // fetch_offset - middle of buffer
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 13);
    assert!(!response.records.is_empty(), "Should return records from middle of buffer");
    
    // Test fetching beyond high watermark
    let response = handler.fetch_partition(
        "test-topic",
        0,
        20,   // fetch_offset - beyond high watermark
        1024, // max_bytes
        0,    // max_wait_ms
        0,    // min_bytes
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 13);
    assert!(response.records.is_empty(), "Fetching beyond high watermark should return no records");
}

#[tokio::test]
async fn test_buffer_overflow_trimming() {
    let temp_dir = TempDir::new().unwrap();
    
    let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
        temp_dir.path().join("metadata")
    ).await.unwrap());
    
    let object_store = Arc::new(chronik_storage::object_store::LocalObjectStore::new(
        temp_dir.path().join("segments")
    ).await.unwrap());
    
    let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
        object_store.clone(),
        object_store.clone()
    ));
    
    let handler = FetchHandler::new(
        segment_reader,
        metadata_store.clone(),
        object_store,
    );
    
    metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
    
    // Add more than 1000 records to trigger trimming
    let mut large_batch = Vec::new();
    for i in 0..1100 {
        large_batch.push(Record {
            offset: i,
            timestamp: 1000 + i,
            key: None,
            value: format!("msg{}", i).into_bytes(),
            headers: vec![],
        });
    }
    
    handler.update_buffer("test-topic", 0, large_batch, 1100).await.unwrap();
    
    // Check that buffer was trimmed to last 1000 records
    let state = handler.state.read().await;
    let key = ("test-topic".to_string(), 0);
    let buffer = state.buffers.get(&key).unwrap();
    
    assert_eq!(buffer.records.len(), 1000, "Buffer should be trimmed to 1000 records");
    assert_eq!(buffer.records[0].offset, 100, "First record should be offset 100 after trimming");
    assert_eq!(buffer.records[999].offset, 1099, "Last record should be offset 1099");
    assert_eq!(buffer.high_watermark, 1100, "High watermark should still be 1100");
}

#[tokio::test]
async fn test_clear_topic_buffers() {
    let temp_dir = TempDir::new().unwrap();
    
    let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
        temp_dir.path().join("metadata")
    ).await.unwrap());
    
    let object_store = Arc::new(chronik_storage::object_store::LocalObjectStore::new(
        temp_dir.path().join("segments")
    ).await.unwrap());
    
    let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
        object_store.clone(),
        object_store.clone()
    ));
    
    let handler = FetchHandler::new(
        segment_reader,
        metadata_store.clone(),
        object_store,
    );
    
    metadata_store.create_topic("topic1", 2, HashMap::new()).await.unwrap();
    metadata_store.create_topic("topic2", 1, HashMap::new()).await.unwrap();
    
    // Add records to multiple topics and partitions
    let records1 = vec![Record {
        offset: 0,
        timestamp: 1000,
        key: None,
        value: b"topic1-p0".to_vec(),
        headers: vec![],
    }];
    
    let records2 = vec![Record {
        offset: 0,
        timestamp: 1000,
        key: None,
        value: b"topic1-p1".to_vec(),
        headers: vec![],
    }];
    
    let records3 = vec![Record {
        offset: 0,
        timestamp: 1000,
        key: None,
        value: b"topic2-p0".to_vec(),
        headers: vec![],
    }];
    
    handler.update_buffer("topic1", 0, records1, 1).await.unwrap();
    handler.update_buffer("topic1", 1, records2, 1).await.unwrap();
    handler.update_buffer("topic2", 0, records3, 1).await.unwrap();
    
    // Verify all buffers exist
    {
        let state = handler.state.read().await;
        assert_eq!(state.buffers.len(), 3, "Should have 3 buffers");
    }
    
    // Clear topic1 buffers
    handler.clear_topic_buffers("topic1").await.unwrap();
    
    // Verify only topic2 buffer remains
    {
        let state = handler.state.read().await;
        assert_eq!(state.buffers.len(), 1, "Should have 1 buffer after clearing topic1");
        assert!(state.buffers.contains_key(&("topic2".to_string(), 0)), "topic2 buffer should remain");
    }
}

#[tokio::test]
async fn test_concurrent_buffer_access() {
    use tokio::sync::Barrier;
    use std::sync::Arc;
    
    let temp_dir = TempDir::new().unwrap();
    
    let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
        temp_dir.path().join("metadata")
    ).await.unwrap());
    
    let object_store = Arc::new(chronik_storage::object_store::LocalObjectStore::new(
        temp_dir.path().join("segments")
    ).await.unwrap());
    
    let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
        object_store.clone(),
        object_store.clone()
    ));
    
    let handler = Arc::new(FetchHandler::new(
        segment_reader,
        metadata_store.clone(),
        object_store,
    ));
    
    metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
    
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = vec![];
    
    // Producer task
    let h1 = handler.clone();
    let b1 = barrier.clone();
    handles.push(tokio::spawn(async move {
        b1.wait().await;
        
        for i in 0..10 {
            let records = vec![Record {
                offset: i,
                timestamp: 1000 + i,
                key: None,
                value: format!("msg{}", i).into_bytes(),
                headers: vec![],
            }];
            
            h1.update_buffer("test-topic", 0, records, i + 1).await.unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }));
    
    // Consumer task 1
    let h2 = handler.clone();
    let b2 = barrier.clone();
    handles.push(tokio::spawn(async move {
        b2.wait().await;
        
        let mut last_offset = 0;
        for _ in 0..5 {
            let response = h2.fetch_partition(
                "test-topic",
                0,
                last_offset,
                1024,
                100,  // max_wait_ms
                1,    // min_bytes
            ).await.unwrap();
            
            if response.high_watermark > last_offset {
                last_offset = response.high_watermark;
            }
            
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
        
        last_offset
    }));
    
    // Consumer task 2
    let h3 = handler.clone();
    let b3 = barrier.clone();
    handles.push(tokio::spawn(async move {
        b3.wait().await;
        
        let mut count = 0;
        for _ in 0..5 {
            let response = h3.fetch_partition(
                "test-topic",
                0,
                0,
                1024,
                100,
                1,
            ).await.unwrap();
            
            if !response.records.is_empty() {
                count += 1;
            }
            
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
        
        count
    }));
    
    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }
    
    // Final verification
    let response = handler.fetch_partition(
        "test-topic",
        0,
        0,
        1024,
        0,
        0,
    ).await.unwrap();
    
    assert_eq!(response.high_watermark, 10, "Final high watermark should be 10");
}