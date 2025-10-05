//! Integration test for multi-record batch round-trip through segment storage.
//!
//! This test verifies that batched messages can be written to and read from segments
//! without data loss. It tests the critical bug reported in v1.3.22 where 85% of data
//! was lost when reading back batched records from segments.

use chronik_storage::{RecordBatch, Record, SegmentBuilder, Segment};
use chronik_common::types::{SegmentMetadata, SegmentId, TopicPartition};
use chrono::Utc;
use std::collections::HashMap;
use uuid::Uuid;

#[test]
fn test_single_batch_multiple_records() {
    // Scenario: Producer sends 1 batch with 20 records (common for high-throughput)
    // Expected: All 20 records should be readable after round-trip

    let mut batch = RecordBatch { records: Vec::new() };

    // Create 20 records in a single batch
    for i in 0..20 {
        batch.records.push(Record {
            offset: i,
            timestamp: 1000 + i,
            key: Some(format!("key-{}", i).into_bytes()),
            value: format!("value-{}", i).into_bytes(),
            headers: HashMap::new(),
        });
    }

    // Encode the batch
    let batch_bytes = batch.encode().unwrap();

    // Decode it back
    let (decoded_batch, bytes_consumed) = RecordBatch::decode(&batch_bytes).unwrap();

    // Verify all 20 records were decoded
    assert_eq!(decoded_batch.records.len(), 20, "Should decode all 20 records from single batch");
    assert_eq!(bytes_consumed, batch_bytes.len(), "Should consume all bytes");

    // Verify record contents
    for i in 0..20 {
        assert_eq!(decoded_batch.records[i].offset, i as i64);
        assert_eq!(decoded_batch.records[i].value, format!("value-{}", i).as_bytes());
    }
}

#[test]
fn test_multiple_batches_concatenated() {
    // Scenario: Producer sends 20 batches, each with 1 record (user's reported case)
    // This mimics how SegmentWriter concatenates batches via add_indexed_record()
    // Expected: All 20 records should be readable

    let mut concatenated_bytes = Vec::new();

    // Create and encode 20 separate batches
    for i in 0..20 {
        let batch = RecordBatch {
            records: vec![Record {
                offset: i,
                timestamp: 1000 + i,
                key: Some(format!("deck-{}", i).into_bytes()),
                value: format!("deck-data-{}", i).into_bytes(),
                headers: HashMap::new(),
            }],
        };

        let batch_bytes = batch.encode().unwrap();
        concatenated_bytes.extend_from_slice(&batch_bytes);
    }

    // Now decode all batches from the concatenated data
    let mut all_records = Vec::new();
    let mut cursor_pos = 0;
    let total_len = concatenated_bytes.len();
    let mut batch_count = 0;

    while cursor_pos < total_len {
        match RecordBatch::decode(&concatenated_bytes[cursor_pos..]) {
            Ok((batch, bytes_consumed)) => {
                println!("Batch {}: decoded {} records, consumed {} bytes at position {}",
                    batch_count + 1, batch.records.len(), bytes_consumed, cursor_pos);

                all_records.extend(batch.records);
                cursor_pos += bytes_consumed;
                batch_count += 1;

                if bytes_consumed == 0 {
                    panic!("Zero bytes consumed - infinite loop detected!");
                }
            }
            Err(e) => {
                panic!("Failed to decode batch {} at position {}/{}: {}",
                    batch_count + 1, cursor_pos, total_len, e);
            }
        }
    }

    // CRITICAL: This is the bug - we should get all 20 records
    assert_eq!(batch_count, 20, "Should decode all 20 batches");
    assert_eq!(all_records.len(), 20, "Should have all 20 records");

    // Verify record contents
    for i in 0..20 {
        assert_eq!(all_records[i].offset, i as i64);
        assert_eq!(all_records[i].value, format!("deck-data-{}", i).as_bytes());
    }
}

#[test]
fn test_mixed_batch_sizes() {
    // Scenario: Producer sends batches with varying record counts (realistic scenario)
    // Batch sizes: 5, 3, 7, 2, 3 (total: 20 records)

    let batch_sizes = vec![5, 3, 7, 2, 3];
    let mut concatenated_bytes = Vec::new();
    let mut expected_offset = 0i64;

    // Create batches with varying sizes
    for batch_size in &batch_sizes {
        let mut records = Vec::new();
        for _ in 0..*batch_size {
            records.push(Record {
                offset: expected_offset,
                timestamp: 1000 + expected_offset,
                key: None,
                value: format!("record-{}", expected_offset).into_bytes(),
                headers: HashMap::new(),
            });
            expected_offset += 1;
        }

        let batch = RecordBatch { records };
        let batch_bytes = batch.encode().unwrap();
        concatenated_bytes.extend_from_slice(&batch_bytes);
    }

    // Decode all batches
    let mut all_records = Vec::new();
    let mut cursor_pos = 0;
    let total_len = concatenated_bytes.len();
    let mut batch_count = 0;

    while cursor_pos < total_len {
        match RecordBatch::decode(&concatenated_bytes[cursor_pos..]) {
            Ok((batch, bytes_consumed)) => {
                all_records.extend(batch.records);
                cursor_pos += bytes_consumed;
                batch_count += 1;

                if bytes_consumed == 0 {
                    panic!("Zero bytes consumed!");
                }
            }
            Err(e) => {
                panic!("Failed to decode at position {}: {}", cursor_pos, e);
            }
        }
    }

    assert_eq!(batch_count, 5, "Should decode all 5 batches");
    assert_eq!(all_records.len(), 20, "Should have all 20 records");
}

#[test]
fn test_segment_round_trip_multi_batch() {
    // Full round-trip test: Create segment with multiple batches, serialize, deserialize, read

    let mut builder = SegmentBuilder::new();

    // Add 20 batches to the segment (simulating 20 produce calls)
    for i in 0..20 {
        let batch = RecordBatch {
            records: vec![Record {
                offset: i,
                timestamp: 1000 + i,
                key: Some(format!("key-{}", i).into_bytes()),
                value: format!("value-{}", i).into_bytes(),
                headers: HashMap::new(),
            }],
        };

        let batch_bytes = batch.encode().unwrap();
        builder.add_indexed_record(&batch_bytes);
    }

    // Set metadata and build segment
    let metadata = SegmentMetadata {
        id: SegmentId(Uuid::new_v4()),
        topic_partition: TopicPartition {
            topic: "test-topic".to_string(),
            partition: 0,
        },
        base_offset: 0,
        last_offset: 19,
        timestamp_min: 1000,
        timestamp_max: 1019,
        size_bytes: 0,
        record_count: 20,
        object_key: "test/0/segment.chrn".to_string(),
        created_at: Utc::now(),
    };

    let segment = builder.with_metadata(metadata).build().unwrap();

    // Serialize the segment
    let serialized = segment.serialize().unwrap();

    // Deserialize it back
    let deserialized = Segment::deserialize(serialized).unwrap();

    // Now decode all records from indexed_records
    let mut all_records = Vec::new();
    let mut cursor_pos = 0;
    let total_len = deserialized.indexed_records.len();
    let mut batch_count = 0;

    println!("Decoding {} bytes of indexed_records", total_len);

    while cursor_pos < total_len {
        match RecordBatch::decode(&deserialized.indexed_records[cursor_pos..]) {
            Ok((batch, bytes_consumed)) => {
                println!("Batch {}: {} records, {} bytes consumed",
                    batch_count + 1, batch.records.len(), bytes_consumed);

                all_records.extend(batch.records);
                cursor_pos += bytes_consumed;
                batch_count += 1;

                if bytes_consumed == 0 {
                    panic!("Zero bytes consumed at position {}", cursor_pos);
                }
            }
            Err(e) => {
                eprintln!("ERROR: Failed to decode batch {} at position {}/{}: {}",
                    batch_count + 1, cursor_pos, total_len, e);
                eprintln!("Successfully decoded {} batches with {} records so far",
                    batch_count, all_records.len());
                break;
            }
        }
    }

    // THIS IS THE CRITICAL ASSERTION - should get all 20 records
    println!("Final: {} batches decoded, {} records total", batch_count, all_records.len());
    assert_eq!(batch_count, 20, "Should decode all 20 batches from segment");
    assert_eq!(all_records.len(), 20, "Should have all 20 records after segment round-trip");
}