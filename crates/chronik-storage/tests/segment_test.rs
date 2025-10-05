use bytes::Bytes;
use chronik_common::types::{SegmentId, SegmentMetadata, TopicPartition};
use chronik_storage::{Segment, SegmentBuilder, RecordBatch, Record};
use chronik_storage::kafka_records::{KafkaRecordBatch, CompressionType};
use std::collections::HashMap;

#[test]
fn test_segment_serialization_roundtrip() {
    // Create test metadata
    let metadata = SegmentMetadata {
        id: SegmentId::new(),
        topic_partition: TopicPartition {
            topic: "test-topic".to_string(),
            partition: 0,
        },
        base_offset: 1000,
        last_offset: 1999,
        timestamp_min: 1700000000000,
        timestamp_max: 1700001000000,
        size_bytes: 1024 * 1024,
        record_count: 1000,
        object_key: "test-key".to_string(),
        created_at: chrono::Utc::now(),
    };
    
    // Build segment
    let mut builder = SegmentBuilder::new()
        .with_metadata(metadata.clone());
    
    // Add some test data (v2 format uses raw_kafka_batches and indexed_records)
    builder.add_raw_kafka_batch(b"test kafka data");
    builder.add_indexed_record(b"test indexed data");
    builder.add_index_data(b"test index data");
    
    let segment = builder.build().expect("Failed to build segment");
    
    // Serialize
    let serialized = segment.serialize().expect("Failed to serialize segment");
    
    // Deserialize
    let deserialized = Segment::deserialize(serialized).expect("Failed to deserialize segment");
    
    // Verify
    assert_eq!(deserialized.metadata.id, metadata.id);
    assert_eq!(deserialized.metadata.topic_partition, metadata.topic_partition);
    assert_eq!(deserialized.metadata.base_offset, metadata.base_offset);
    assert_eq!(deserialized.metadata.last_offset, metadata.last_offset);
    assert_eq!(deserialized.raw_kafka_batches, Bytes::from("test kafka data"));
    assert_eq!(deserialized.indexed_records, Bytes::from("test indexed data"));
    assert_eq!(deserialized.index_data, Bytes::from("test index data"));
}

#[test]
fn test_segment_storage_key() {
    let metadata = SegmentMetadata {
        id: SegmentId::new(),
        topic_partition: TopicPartition {
            topic: "my-topic".to_string(),
            partition: 42,
        },
        base_offset: 1000,
        last_offset: 1999,
        timestamp_min: 0,
        timestamp_max: 0,
        size_bytes: 0,
        record_count: 0,
        object_key: "".to_string(),
        created_at: chrono::Utc::now(),
    };
    
    let segment = SegmentBuilder::new()
        .with_metadata(metadata)
        .build()
        .unwrap();
    
    let key = segment.storage_key();
    assert_eq!(key, "segments/my-topic/partition-00042/segment-0000000000001000-0000000000001999.chrn");
}

// ====================================================================================================
// BATCH ROUND-TRIP TESTS - Testing multi-record batch storage and retrieval
// These tests verify the bug fix for v1.3.23 where 85% of batched data was lost
// ====================================================================================================

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
    //
    // THIS TEST WILL FAIL before the fix and PASS after the fix

    let mut concatenated_bytes = Vec::new();

    // Create and encode 20 separate batches (like real production scenario)
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

    println!("Total concatenated bytes: {}", concatenated_bytes.len());

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
                    panic!("Zero bytes consumed - infinite loop detected at batch {}!", batch_count);
                }
            }
            Err(e) => {
                // Print diagnostic info before failing
                eprintln!("ERROR: Failed to decode batch {} at position {}/{}:", batch_count + 1, cursor_pos, total_len);
                eprintln!("  Error: {}", e);
                eprintln!("  Successfully decoded {} batches with {} records so far", batch_count, all_records.len());
                eprintln!("  Remaining bytes: {}", total_len - cursor_pos);

                // This is the BUG - we expect all batches to decode successfully
                panic!("Batch decoding failed - this is the v1.3.22 bug!");
            }
        }
    }

    // CRITICAL ASSERTIONS: These will fail with the bug, pass with the fix
    assert_eq!(batch_count, 20, "Should decode all 20 batches (GOT {} - THIS IS THE BUG!)", batch_count);
    assert_eq!(all_records.len(), 20, "Should have all 20 records (GOT {} - 85% DATA LOSS!)", all_records.len());

    // Verify record contents
    for i in 0..20 {
        assert_eq!(all_records[i].offset, i as i64);
        assert_eq!(all_records[i].value, format!("deck-data-{}", i).as_bytes());
    }
}

#[test]
fn test_segment_round_trip_multi_batch() {
    // Full round-trip test: Create segment with multiple batches, serialize, deserialize, read
    // This is the COMPLETE simulation of the user's reported issue

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
        id: SegmentId::new(),
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
        created_at: chrono::Utc::now(),
    };

    let segment = builder.with_metadata(metadata).build().unwrap();

    // Serialize the segment
    let serialized = segment.serialize().unwrap();

    // Deserialize it back
    let deserialized = Segment::deserialize(serialized).unwrap();

    println!("Segment has {} bytes of indexed_records", deserialized.indexed_records.len());

    // Now decode all records from indexed_records (this is what fetch_handler does)
    let mut all_records = Vec::new();
    let mut cursor_pos = 0;
    let total_len = deserialized.indexed_records.len();
    let mut batch_count = 0;

    while cursor_pos < total_len {
        match RecordBatch::decode(&deserialized.indexed_records[cursor_pos..]) {
            Ok((batch, bytes_consumed)) => {
                println!("Batch {}: {} records, {} bytes consumed at position {}",
                    batch_count + 1, batch.records.len(), bytes_consumed, cursor_pos);

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

    assert_eq!(batch_count, 20,
        "Should decode all 20 batches from segment (this is the v1.3.22 bug - only got {})",
        batch_count);
    assert_eq!(all_records.len(), 20,
        "Should have all 20 records after segment round-trip (85% data loss - only got {})",
        all_records.len());

    // Verify record contents
    for i in 0..20 {
        assert_eq!(all_records[i].offset, i as i64);
    }
}

#[test]
fn test_raw_kafka_batches_multi_batch() {
    // Test using ACTUAL Kafka RecordBatch format (what real producers send)
    // This tests the raw_kafka_batches path used in production (when enable_dual_storage=false)

    let mut builder = SegmentBuilder::new();

    // Create 20 Kafka batches (matching user's scenario: 20 produce calls)
    for i in 0..20 {
        let mut kafka_batch = KafkaRecordBatch::new(
            i,                      // base_offset
            1000 + i,               // base_timestamp
            -1,                     // producer_id
            -1,                     // producer_epoch
            -1,                     // base_sequence
            CompressionType::None,  // no compression
            false,                  // not transactional
        );

        // Add 1 record to this batch
        kafka_batch.add_record(
            Some(Bytes::from(format!("key-{}", i))),
            Some(Bytes::from(format!("value-{}", i))),
            vec![],
            1000 + i,
        );

        // Encode to Kafka wire format
        let kafka_bytes = kafka_batch.encode().unwrap();

        // Add to segment's raw_kafka_batches (this is what happens in production)
        builder.add_raw_kafka_batch(&kafka_bytes);
    }

    // Build segment with metadata
    let metadata = SegmentMetadata {
        id: SegmentId::new(),
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
        created_at: chrono::Utc::now(),
    };

    let segment = builder.with_metadata(metadata).build().unwrap();

    // Serialize and deserialize
    let serialized = segment.serialize().unwrap();
    let deserialized = Segment::deserialize(serialized).unwrap();

    println!("Segment has {} bytes of raw_kafka_batches", deserialized.raw_kafka_batches.len());
    println!("Segment has {} bytes of indexed_records", deserialized.indexed_records.len());

    // NOW DECODE using the SAME CODE PATH as fetch_handler.rs (lines 871-915)
    // This should match production behavior exactly

    let mut all_records = Vec::new();
    let mut batch_count = 0;

    use std::io::Cursor;
    let mut cursor = Cursor::new(&deserialized.raw_kafka_batches[..]);
    let total_len = deserialized.raw_kafka_batches.len();

    while (cursor.position() as usize) < total_len {
        match KafkaRecordBatch::decode(&deserialized.raw_kafka_batches[(cursor.position() as usize)..]) {
            Ok((kafka_batch, bytes_consumed)) => {
                println!("Batch {}: {} records in batch, {} bytes consumed at position {}",
                    batch_count + 1,
                    kafka_batch.records.len(),
                    bytes_consumed,
                    cursor.position());

                // Convert Kafka records to storage Records (same as fetch_handler)
                let records: Vec<Record> = kafka_batch.records.into_iter().enumerate().map(|(idx, kr)| {
                    Record {
                        offset: kafka_batch.header.base_offset + idx as i64,
                        timestamp: kafka_batch.header.base_timestamp + kr.timestamp_delta,
                        key: kr.key.map(|k| k.to_vec()),
                        value: kr.value.map(|v| v.to_vec()).unwrap_or_default(),
                        headers: kr.headers.into_iter().map(|h| {
                            (h.key, h.value.map(|v| v.to_vec()).unwrap_or_default())
                        }).collect(),
                    }
                }).collect();

                all_records.extend(records);
                batch_count += 1;

                // Advance cursor (THIS IS THE v1.3.22 FIX)
                cursor.set_position(cursor.position() + bytes_consumed as u64);

                if bytes_consumed == 0 {
                    eprintln!("ERROR: Zero bytes consumed at batch {}!", batch_count);
                    panic!("Infinite loop detected - v1.3.22 fix failed!");
                }
            }
            Err(e) => {
                eprintln!("ERROR: Failed to decode Kafka batch {} at position {}/{}: {}",
                    batch_count + 1, cursor.position(), total_len, e);
                eprintln!("Successfully decoded {} batches with {} records so far",
                    batch_count, all_records.len());
                panic!("Kafka batch decoding failed - THIS IS THE PRODUCTION BUG!");
            }
        }
    }

    // CRITICAL ASSERTIONS
    println!("Final: {} Kafka batches decoded, {} records total", batch_count, all_records.len());

    assert_eq!(batch_count, 20,
        "Should decode all 20 Kafka batches (GOT {} - PRODUCTION BUG!)", batch_count);
    assert_eq!(all_records.len(), 20,
        "Should have all 20 records (GOT {} - 85% DATA LOSS!)", all_records.len());

    // Verify record contents
    for i in 0..20 {
        assert_eq!(all_records[i].offset, i as i64);
        assert_eq!(all_records[i].value, format!("value-{}", i).as_bytes());
    }
}

#[test]
fn test_multiple_separate_batch_writes() {
    // CRITICAL TEST: This reproduces the ACTUAL bug reported by users
    //
    // The bug is NOT about multi-record batches (that works fine)
    // The bug IS about multiple separate batches written to the same segment
    //
    // User's scenario:
    // - Go producer sends 20 messages
    // - Each message becomes a separate batch (due to small size + timing)
    // - All batches written to same segment file
    // - Only first 1-2 batches are readable (85% data loss)
    //
    // This test simulates writing batches SEPARATELY (like real production)
    // instead of concatenating them in memory first

    let mut builder = SegmentBuilder::new();

    println!("\n🔬 REPRODUCING USER'S BUG: Writing 20 separate batches");
    println!("Each add_indexed_record() call simulates a separate produce request\n");

    // Write 20 batches SEPARATELY (this is what actually happens in production!)
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

        // THIS IS THE KEY: Each batch is added separately, just like in production
        // when SegmentWriter.write_dual_format() is called 20 times
        builder.add_indexed_record(&batch_bytes);

        println!("  Wrote batch {}: {} bytes", i + 1, batch_bytes.len());
    }

    // Build segment
    let metadata = SegmentMetadata {
        id: SegmentId::new(),
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
        created_at: chrono::Utc::now(),
    };

    let segment = builder.with_metadata(metadata).build().unwrap();

    // Serialize and deserialize
    let serialized = segment.serialize().unwrap();
    let deserialized = Segment::deserialize(serialized).unwrap();

    println!("\n📊 Segment stats:");
    println!("  indexed_records size: {} bytes", deserialized.indexed_records.len());
    println!("  Expected batches: 20");
    println!("\n🔍 Attempting to decode batches...\n");

    // NOW DECODE - v3 format with length prefixes
    let mut all_records = Vec::new();
    let mut cursor_pos = 0;
    let total_len = deserialized.indexed_records.len();
    let mut batch_count = 0;

    // v3 format: Each batch is prefixed with u32 length
    println!("  Segment version: {}", deserialized.header.version);

    while cursor_pos < total_len {
        // Read length prefix (4 bytes)
        if cursor_pos + 4 > total_len {
            println!("  ⚠️  Not enough bytes for length prefix at position {}", cursor_pos);
            break;
        }

        let batch_len = u32::from_be_bytes([
            deserialized.indexed_records[cursor_pos],
            deserialized.indexed_records[cursor_pos + 1],
            deserialized.indexed_records[cursor_pos + 2],
            deserialized.indexed_records[cursor_pos + 3],
        ]) as usize;

        println!("  📏 Batch {}: Length prefix = {} bytes", batch_count + 1, batch_len);

        // Move past length prefix
        let batch_data_start = cursor_pos + 4;
        let batch_data_end = batch_data_start + batch_len;

        if batch_data_end > total_len {
            eprintln!("\n  ❌ ERROR: Batch extends beyond segment (start={}, end={}, total={})",
                batch_data_start, batch_data_end, total_len);
            break;
        }

        // Decode the batch data
        match RecordBatch::decode(&deserialized.indexed_records[batch_data_start..batch_data_end]) {
            Ok((batch, _bytes_consumed)) => {
                println!("  ✅ Batch {}: {} records, {} bytes at position {}",
                    batch_count + 1, batch.records.len(), batch_len, cursor_pos);

                all_records.extend(batch.records);
                cursor_pos = batch_data_end;
                batch_count += 1;
            }
            Err(e) => {
                eprintln!("\n  ❌ ERROR: Failed to decode batch {} at position {}",
                    batch_count + 1, cursor_pos);
                eprintln!("     Error: {}", e);
                eprintln!("     Batch length: {}", batch_len);
                eprintln!("     Decoded so far: {} batches, {} records", batch_count, all_records.len());
                break;
            }
        }
    }

    println!("\n📈 RESULTS:");
    println!("  Batches decoded: {}/20", batch_count);
    println!("  Records recovered: {}/20", all_records.len());

    if batch_count < 20 {
        println!("\n🐛 BUG REPRODUCED!");
        println!("  Expected: 20 batches");
        println!("  Got:      {} batches", batch_count);
        println!("  Data loss: {}%", ((20 - all_records.len()) * 100) / 20);
        println!("\nThis is the exact bug users reported:");
        println!("  - Multiple batches written separately to segment");
        println!("  - Only first 1-2 batches readable");
        println!("  - 85-90% data loss");
    }

    // THIS ASSERTION WILL FAIL with current code (proving we reproduced the bug)
    // After fixing, this should PASS
    assert_eq!(batch_count, 20,
        "Should decode all 20 batches from segment (BUG: only decoded {})", batch_count);
    assert_eq!(all_records.len(), 20,
        "Should have all 20 records (BUG: only got {})", all_records.len());

    // Verify record contents
    for i in 0..all_records.len() {
        assert_eq!(all_records[i].offset, i as i64);
    }
}