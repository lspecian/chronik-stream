//! Integration tests for storage layer.

use chronik_storage::{
    segment::{SegmentManager, SegmentWriter},
    object_store::{LocalObjectStore, ObjectStore},
};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn test_segment_lifecycle() {
    let temp_dir = TempDir::new().unwrap();
    let manager = SegmentManager::new(temp_dir.path()).await.unwrap();
    
    // Create a segment
    let topic = "test-topic";
    let partition = 0;
    let segment_id = Uuid::new_v4();
    
    let mut writer = SegmentWriter::new(
        segment_id,
        topic.to_string(),
        partition,
        0, // base_offset
    );
    
    // Write some messages
    for i in 0..100 {
        let message = format!("Message {}", i);
        writer.append(i as i64, message.as_bytes()).unwrap();
    }
    
    // Seal the segment
    let segment = writer.seal();
    assert_eq!(segment.message_count(), 100);
    
    // Store the segment
    let store = LocalObjectStore::new(temp_dir.path()).unwrap();
    let key = format!("segments/{}/{}/{}.seg", topic, partition, segment_id);
    
    let data = segment.serialize().unwrap();
    store.put(&key, data).await.unwrap();
    
    // Read back the segment
    let loaded_data = store.get(&key).await.unwrap();
    let loaded_segment = chronik_storage::segment::Segment::deserialize(&loaded_data).unwrap();
    
    assert_eq!(loaded_segment.message_count(), 100);
    assert_eq!(loaded_segment.id(), segment_id);
}

#[tokio::test]
async fn test_concurrent_writers() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(LocalObjectStore::new(temp_dir.path()).unwrap());
    
    let mut handles = vec![];
    
    // Spawn multiple writers
    for partition in 0..5 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            let mut writer = SegmentWriter::new(
                Uuid::new_v4(),
                "concurrent-topic".to_string(),
                partition,
                0,
            );
            
            // Write messages
            for i in 0..50 {
                let message = format!("Partition {} Message {}", partition, i);
                writer.append(i as i64, message.as_bytes()).unwrap();
            }
            
            // Store segment
            let segment = writer.seal();
            let key = format!("segments/concurrent-topic/{}/{}.seg", partition, segment.id());
            let data = segment.serialize().unwrap();
            store_clone.put(&key, data).await.unwrap();
            
            partition
        });
        
        handles.push(handle);
    }
    
    // Wait for all writers
    for handle in handles {
        handle.await.unwrap();
    }
    
    // Verify all segments were written
    let prefix = "segments/concurrent-topic/";
    let objects = store.list(prefix).await.unwrap();
    assert_eq!(objects.len(), 5);
}

#[tokio::test]
async fn test_segment_rotation() {
    let temp_dir = TempDir::new().unwrap();
    let manager = SegmentManager::new(temp_dir.path()).await.unwrap();
    
    let topic = "rotation-test";
    let partition = 0;
    let max_segment_size = 1024; // 1KB
    
    let mut current_offset = 0i64;
    let mut segments = vec![];
    
    // Write messages until we need to rotate
    let mut writer = SegmentWriter::new(
        Uuid::new_v4(),
        topic.to_string(),
        partition,
        current_offset,
    );
    
    for i in 0..100 {
        let message = vec![b'x'; 50]; // 50 bytes per message
        writer.append(current_offset, &message).unwrap();
        current_offset += 1;
        
        // Check if we should rotate
        if writer.size() > max_segment_size {
            segments.push(writer.seal());
            writer = SegmentWriter::new(
                Uuid::new_v4(),
                topic.to_string(),
                partition,
                current_offset,
            );
        }
    }
    
    // Seal final segment
    if writer.message_count() > 0 {
        segments.push(writer.seal());
    }
    
    // Should have multiple segments
    assert!(segments.len() > 1);
    
    // Verify offset continuity
    let mut expected_offset = 0;
    for segment in &segments {
        assert_eq!(segment.base_offset(), expected_offset);
        expected_offset += segment.message_count() as i64;
    }
    assert_eq!(expected_offset, 100);
}

use std::sync::Arc;