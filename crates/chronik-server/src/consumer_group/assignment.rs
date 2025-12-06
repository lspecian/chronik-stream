//! Assignment encoding and decoding
//!
//! Handles Kafka ConsumerGroup assignment protocol encoding/decoding.
//! Supports versions 0-3 (Kafka 0.9-3.x).

use chronik_common::{Result, Error};
use std::collections::HashMap;
use std::io::Read;

/// Encode assignment to bytes (Kafka protocol format)
///
/// Complexity: < 15 (byteorder encoding with sorted partitions)
pub fn encode_assignment(assignment: &HashMap<String, Vec<i32>>) -> Vec<u8> {
    use byteorder::{BigEndian, WriteBytesExt};

    let mut bytes = Vec::new();

    // Version (0 for legacy, 1 for incremental)
    // CRITICAL: Use version 0 for kafka-python/rdkafka compatibility
    bytes.write_i16::<BigEndian>(0).unwrap();

    // Topic count
    bytes.write_i32::<BigEndian>(assignment.len() as i32).unwrap();

    for (topic, partitions) in assignment {
        // Topic name
        bytes.write_i16::<BigEndian>(topic.len() as i16).unwrap();
        bytes.extend_from_slice(topic.as_bytes());

        // Partition count
        bytes.write_i32::<BigEndian>(partitions.len() as i32).unwrap();

        // Partitions (sorted for consistency)
        let mut sorted_partitions = partitions.clone();
        sorted_partitions.sort();
        for partition in sorted_partitions {
            bytes.write_i32::<BigEndian>(partition).unwrap();
        }
    }

    // User data (empty)
    bytes.write_i32::<BigEndian>(0).unwrap();

    // Log hex dump for debugging (trace level)
    let hex_str = bytes.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ");
    tracing::trace!(
        assignment_hex = %hex_str,
        assignment_len = bytes.len(),
        "Encoded assignment bytes (hex dump)"
    );

    bytes
}

/// Decode assignment from bytes
///
/// Complexity: < 20 (byteorder decoding with version support)
pub fn decode_assignment(bytes: &[u8]) -> Result<HashMap<String, Vec<i32>>> {
    use byteorder::{BigEndian, ReadBytesExt};
    use std::io::Cursor;

    let mut cursor = Cursor::new(bytes);
    let mut assignment = HashMap::new();

    // Read version
    let version = cursor.read_i16::<BigEndian>()
        .map_err(|e| Error::Serialization(format!("Failed to read version: {}", e)))?;

    // Support versions 0-3 (Kafka 0.9-3.x)
    // Version 0: topic-partitions only
    // Version 1: topic-partitions + user_data
    // Version 2: topic-partitions + user_data (same as v1)
    // Version 3: topic-partitions + user_data (same as v1, v2)
    if version < 0 || version > 3 {
        return Err(Error::Protocol(format!("Unsupported assignment version: {}", version)));
    }

    // Read topic count
    let topic_count = cursor.read_i32::<BigEndian>()
        .map_err(|e| Error::Serialization(format!("Failed to read topic count: {}", e)))?;

    for _ in 0..topic_count {
        // Read topic name
        let topic_len = cursor.read_i16::<BigEndian>()
            .map_err(|e| Error::Serialization(format!("Failed to read topic length: {}", e)))? as usize;

        let mut topic_bytes = vec![0u8; topic_len];
        cursor.read_exact(&mut topic_bytes)
            .map_err(|e| Error::Serialization(format!("Failed to read topic name: {}", e)))?;

        let topic = String::from_utf8(topic_bytes)
            .map_err(|e| Error::Serialization(format!("Invalid topic name: {}", e)))?;

        // Read partition count
        let partition_count = cursor.read_i32::<BigEndian>()
            .map_err(|e| Error::Serialization(format!("Failed to read partition count: {}", e)))?;

        let mut partitions = Vec::new();
        for _ in 0..partition_count {
            let partition = cursor.read_i32::<BigEndian>()
                .map_err(|e| Error::Serialization(format!("Failed to read partition: {}", e)))?;
            partitions.push(partition);
        }

        assignment.insert(topic, partitions);
    }

    // Skip user data
    let user_data_len = cursor.read_i32::<BigEndian>()
        .map_err(|e| Error::Serialization(format!("Failed to read user data length: {}", e)))?;

    if user_data_len > 0 {
        let mut user_data = vec![0u8; user_data_len as usize];
        cursor.read_exact(&mut user_data)
            .map_err(|e| Error::Serialization(format!("Failed to read user data: {}", e)))?;
    }

    Ok(assignment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut assignment = HashMap::new();
        assignment.insert("topic1".to_string(), vec![0, 1, 2]);
        assignment.insert("topic2".to_string(), vec![3, 4]);

        let encoded = encode_assignment(&assignment);
        let decoded = decode_assignment(&encoded).unwrap();

        // Note: HashMap ordering might differ, so check contents
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.get("topic1"), Some(&vec![0, 1, 2]));
        assert_eq!(decoded.get("topic2"), Some(&vec![3, 4]));
    }

    #[test]
    fn test_decode_empty_assignment() {
        let mut assignment = HashMap::new();
        let encoded = encode_assignment(&assignment);
        let decoded = decode_assignment(&encoded).unwrap();

        assert_eq!(decoded.len(), 0);
    }
}
