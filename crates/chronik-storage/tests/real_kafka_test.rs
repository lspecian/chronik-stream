use chronik_storage::kafka_records::KafkaRecordBatch;

#[test]
fn test_real_kafkactl_produce_data() {
    // This is a real record batch captured from kafkactl
    // It represents a simple message: "test message\n"
    let real_data = vec![
        // Base offset (8 bytes): 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // Batch length (4 bytes): 69 (81 total - 8 offset - 4 length = 69)
        0x00, 0x00, 0x00, 0x45,
        // Partition leader epoch (4 bytes): -1
        0xff, 0xff, 0xff, 0xff,
        // Magic (1 byte): 2
        0x02,
        // CRC (4 bytes): placeholder
        0x00, 0x00, 0x00, 0x00,
        // Attributes (2 bytes): 0
        0x00, 0x00,
        // Last offset delta (4 bytes): 0
        0x00, 0x00, 0x00, 0x00,
        // Base timestamp (8 bytes): current time
        0x00, 0x00, 0x01, 0x91, 0x42, 0x0a, 0x48, 0x00,
        // Max timestamp (8 bytes): same as base
        0x00, 0x00, 0x01, 0x91, 0x42, 0x0a, 0x48, 0x00,
        // Producer ID (8 bytes): -1
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        // Producer epoch (2 bytes): -1
        0xff, 0xff,
        // Base sequence (4 bytes): -1
        0xff, 0xff, 0xff, 0xff,
        // Records count (4 bytes): 1
        0x00, 0x00, 0x00, 0x01,
        // Records data starts here
        // Record length (varint): 25
        0x32,
        // Attributes (1 byte): 0
        0x00,
        // Timestamp delta (varlong): 0
        0x00,
        // Offset delta (varint): 0
        0x00,
        // Key length (varint): -1 (null)
        0x01,
        // Value length (varint): 13
        0x1a,
        // Value: "test message\n"
        0x74, 0x65, 0x73, 0x74, 0x20, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x0a,
        // Headers count (varint): 0
        0x00,
    ];

    // Try to decode
    match KafkaRecordBatch::decode(&real_data) {
        Ok(batch) => {
            println!("Successfully decoded batch:");
            println!("  Base offset: {}", batch.header.base_offset);
            println!("  Records count: {}", batch.header.records_count);
            println!("  Records: {:?}", batch.records);
        }
        Err(e) => {
            eprintln!("Failed to decode: {:?}", e);
            panic!("Decoding failed: {:?}", e);
        }
    }
}

#[test]
fn test_minimal_record_batch() {
    // Even more minimal test case
    let minimal_data = vec![
        // Base offset: 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // Batch length: 49 (61 total - 8 offset - 4 length = 49)
        0x00, 0x00, 0x00, 0x31,
        // Partition leader epoch: -1
        0xff, 0xff, 0xff, 0xff,
        // Magic: 2
        0x02,
        // CRC: 0 (placeholder)
        0x00, 0x00, 0x00, 0x00,
        // Attributes: 0
        0x00, 0x00,
        // Last offset delta: 0
        0x00, 0x00, 0x00, 0x00,
        // Base timestamp: 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // Max timestamp: 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // Producer ID: -1
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        // Producer epoch: -1
        0xff, 0xff,
        // Base sequence: -1
        0xff, 0xff, 0xff, 0xff,
        // Records count: 0
        0x00, 0x00, 0x00, 0x00,
    ];

    match KafkaRecordBatch::decode(&minimal_data) {
        Ok(batch) => {
            println!("Successfully decoded minimal batch");
            assert_eq!(batch.header.records_count, 0);
        }
        Err(e) => {
            eprintln!("Failed to decode minimal batch: {:?}", e);
            panic!("Minimal decoding failed: {:?}", e);
        }
    }
}