//! Enhanced protocol metadata exchange and versioning support
//!
//! This module provides improved protocol metadata handling with support for
//! multiple versions, capability negotiation, and compatibility validation.

use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Cursor, Write, Read};

/// Supported protocol versions
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProtocolVersion {
    /// Basic subscription protocol
    V0 = 0,
    /// Added user data support
    V1 = 1,
    /// Added owned partitions for cooperative rebalancing (KIP-429)
    V2 = 2,
    /// Added rack awareness
    V3 = 3,
    /// Added generation ID for better coordination
    V4 = 4,
}

impl TryFrom<i16> for ProtocolVersion {
    type Error = Error;
    
    fn try_from(value: i16) -> Result<Self> {
        match value {
            0 => Ok(ProtocolVersion::V0),
            1 => Ok(ProtocolVersion::V1),
            2 => Ok(ProtocolVersion::V2),
            3 => Ok(ProtocolVersion::V3),
            4 => Ok(ProtocolVersion::V4),
            _ => Err(Error::Protocol(format!("Unsupported protocol version: {}", value))),
        }
    }
}

/// Consumer subscription metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionMetadata {
    /// Protocol version
    pub version: ProtocolVersion,
    /// Subscribed topics
    pub topics: Vec<String>,
    /// User data
    pub user_data: Option<Vec<u8>>,
    /// Owned partitions for cooperative rebalancing (v2+)
    pub owned_partitions: Option<HashMap<String, Vec<i32>>>,
    /// Rack ID for rack-aware assignment (v3+)
    pub rack_id: Option<String>,
    /// Generation ID (v4+)
    pub generation_id: Option<i32>,
}

impl SubscriptionMetadata {
    /// Create new subscription metadata
    pub fn new(version: ProtocolVersion, topics: Vec<String>) -> Self {
        Self {
            version,
            topics,
            user_data: None,
            owned_partitions: None,
            rack_id: None,
            generation_id: None,
        }
    }
    
    /// Encode to bytes
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        
        // Version
        cursor.write_i16::<BigEndian>(self.version as i16)?;
        
        // Topics
        cursor.write_i32::<BigEndian>(self.topics.len() as i32)?;
        for topic in &self.topics {
            write_string(&mut cursor, topic)?;
        }
        
        // User data
        write_optional_bytes(&mut cursor, &self.user_data)?;
        
        // Version 2+: Owned partitions
        if self.version >= ProtocolVersion::V2 {
            if let Some(owned) = &self.owned_partitions {
                cursor.write_i32::<BigEndian>(owned.len() as i32)?;
                for (topic, partitions) in owned {
                    write_string(&mut cursor, topic)?;
                    cursor.write_i32::<BigEndian>(partitions.len() as i32)?;
                    for &partition in partitions {
                        cursor.write_i32::<BigEndian>(partition)?;
                    }
                }
            } else {
                cursor.write_i32::<BigEndian>(0)?;
            }
        }
        
        // Version 3+: Rack ID
        if self.version >= ProtocolVersion::V3 {
            write_optional_string(&mut cursor, &self.rack_id)?;
        }
        
        // Version 4+: Generation ID
        if self.version >= ProtocolVersion::V4 {
            cursor.write_i32::<BigEndian>(self.generation_id.unwrap_or(-1))?;
        }
        
        Ok(buf)
    }
    
    /// Decode from bytes
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        
        // Version
        let version_num = cursor.read_i16::<BigEndian>()?;
        let version = ProtocolVersion::try_from(version_num)?;
        
        // Topics
        let topic_count = cursor.read_i32::<BigEndian>()?;
        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(read_string(&mut cursor)?);
        }
        
        // User data
        let user_data = read_optional_bytes(&mut cursor)?;
        
        // Version 2+: Owned partitions
        let owned_partitions = if version >= ProtocolVersion::V2 {
            let owned_count = cursor.read_i32::<BigEndian>()?;
            if owned_count > 0 {
                let mut owned = HashMap::new();
                for _ in 0..owned_count {
                    let topic = read_string(&mut cursor)?;
                    let partition_count = cursor.read_i32::<BigEndian>()?;
                    let mut partitions = Vec::with_capacity(partition_count as usize);
                    for _ in 0..partition_count {
                        partitions.push(cursor.read_i32::<BigEndian>()?);
                    }
                    owned.insert(topic, partitions);
                }
                Some(owned)
            } else {
                None
            }
        } else {
            None
        };
        
        // Version 3+: Rack ID
        let rack_id = if version >= ProtocolVersion::V3 {
            read_optional_string(&mut cursor)?
        } else {
            None
        };
        
        // Version 4+: Generation ID
        let generation_id = if version >= ProtocolVersion::V4 {
            let gen = cursor.read_i32::<BigEndian>()?;
            if gen >= 0 { Some(gen) } else { None }
        } else {
            None
        };
        
        Ok(Self {
            version,
            topics,
            user_data,
            owned_partitions,
            rack_id,
            generation_id,
        })
    }
}

/// Assignment metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentMetadata {
    /// Protocol version
    pub version: ProtocolVersion,
    /// Assigned partitions by topic
    pub assignments: HashMap<String, Vec<i32>>,
    /// User data
    pub user_data: Option<Vec<u8>>,
    /// Error code for assignment (v2+)
    pub error_code: Option<i16>,
}

impl AssignmentMetadata {
    /// Create new assignment metadata
    pub fn new(version: ProtocolVersion, assignments: HashMap<String, Vec<i32>>) -> Self {
        Self {
            version,
            assignments,
            user_data: None,
            error_code: None,
        }
    }
    
    /// Encode to bytes
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        
        // Version
        cursor.write_i16::<BigEndian>(self.version as i16)?;
        
        // Assignments
        cursor.write_i32::<BigEndian>(self.assignments.len() as i32)?;
        for (topic, partitions) in &self.assignments {
            write_string(&mut cursor, topic)?;
            cursor.write_i32::<BigEndian>(partitions.len() as i32)?;
            for &partition in partitions {
                cursor.write_i32::<BigEndian>(partition)?;
            }
        }
        
        // User data
        write_optional_bytes(&mut cursor, &self.user_data)?;
        
        // Version 2+: Error code
        if self.version >= ProtocolVersion::V2 {
            cursor.write_i16::<BigEndian>(self.error_code.unwrap_or(0))?;
        }
        
        Ok(buf)
    }
    
    /// Decode from bytes
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        
        // Version
        let version_num = cursor.read_i16::<BigEndian>()?;
        let version = ProtocolVersion::try_from(version_num)?;
        
        // Assignments
        let topic_count = cursor.read_i32::<BigEndian>()?;
        let mut assignments = HashMap::new();
        for _ in 0..topic_count {
            let topic = read_string(&mut cursor)?;
            let partition_count = cursor.read_i32::<BigEndian>()?;
            let mut partitions = Vec::with_capacity(partition_count as usize);
            for _ in 0..partition_count {
                partitions.push(cursor.read_i32::<BigEndian>()?);
            }
            assignments.insert(topic, partitions);
        }
        
        // User data
        let user_data = read_optional_bytes(&mut cursor)?;
        
        // Version 2+: Error code
        let error_code = if version >= ProtocolVersion::V2 {
            Some(cursor.read_i16::<BigEndian>()?)
        } else {
            None
        };
        
        Ok(Self {
            version,
            assignments,
            user_data,
            error_code,
        })
    }
}

/// Protocol compatibility checker
pub struct ProtocolCompatibility;

impl ProtocolCompatibility {
    /// Check if two protocol versions are compatible
    pub fn are_compatible(v1: ProtocolVersion, v2: ProtocolVersion) -> bool {
        // Generally, newer versions can understand older ones
        v1 <= v2 || v2 <= v1
    }
    
    /// Get the highest common version between members
    pub fn get_common_version(versions: &[ProtocolVersion]) -> Option<ProtocolVersion> {
        versions.iter().min().copied()
    }
    
    /// Validate protocol metadata for a group
    pub fn validate_group_protocols(
        _protocol_type: &str,
        member_protocols: &[(String, Vec<(String, Vec<u8>)>)],
    ) -> Result<String> {
        if member_protocols.is_empty() {
            return Err(Error::Protocol("No members in group".to_string()));
        }
        
        // Find common protocols
        let first_member_protocols: HashSet<String> = member_protocols[0].1
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        
        let common_protocols = member_protocols.iter()
            .skip(1)
            .fold(first_member_protocols, |acc, (_, protocols)| {
                let member_set: HashSet<String> = protocols
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect();
                acc.intersection(&member_set).cloned().collect()
            });
        
        if common_protocols.is_empty() {
            return Err(Error::Protocol("No common protocol among members".to_string()));
        }
        
        // For now, just pick the first common protocol
        // In practice, this should use a more sophisticated selection
        Ok(common_protocols.into_iter().next().unwrap())
    }
}

// Helper functions for encoding/decoding

fn write_string<W: Write>(writer: &mut W, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    writer.write_i16::<BigEndian>(bytes.len() as i16)?;
    writer.write_all(bytes)?;
    Ok(())
}

fn read_string<R: Read>(reader: &mut R) -> Result<String> {
    let len = reader.read_i16::<BigEndian>()? as usize;
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|e| Error::Serialization(format!("Invalid UTF-8 string: {}", e)))
}

fn write_optional_string<W: Write>(writer: &mut W, s: &Option<String>) -> Result<()> {
    match s {
        Some(s) => write_string(writer, s),
        None => {
            writer.write_i16::<BigEndian>(-1)?;
            Ok(())
        }
    }
}

fn read_optional_string<R: Read>(reader: &mut R) -> Result<Option<String>> {
    let len = reader.read_i16::<BigEndian>()?;
    if len < 0 {
        Ok(None)
    } else {
        let mut bytes = vec![0u8; len as usize];
        reader.read_exact(&mut bytes)?;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| Error::Serialization(format!("Invalid UTF-8 string: {}", e)))
    }
}

fn write_optional_bytes<W: Write>(writer: &mut W, bytes: &Option<Vec<u8>>) -> Result<()> {
    match bytes {
        Some(b) => {
            writer.write_i32::<BigEndian>(b.len() as i32)?;
            writer.write_all(b)?;
        }
        None => writer.write_i32::<BigEndian>(-1)?,
    }
    Ok(())
}

fn read_optional_bytes<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let len = reader.read_i32::<BigEndian>()?;
    if len < 0 {
        Ok(None)
    } else {
        let mut bytes = vec![0u8; len as usize];
        reader.read_exact(&mut bytes)?;
        Ok(Some(bytes))
    }
}

use std::collections::HashSet;

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_subscription_metadata_v0() {
        let metadata = SubscriptionMetadata::new(
            ProtocolVersion::V0,
            vec!["topic1".to_string(), "topic2".to_string()],
        );
        
        let encoded = metadata.encode().unwrap();
        let decoded = SubscriptionMetadata::decode(&encoded).unwrap();
        
        assert_eq!(decoded.version, ProtocolVersion::V0);
        assert_eq!(decoded.topics, vec!["topic1", "topic2"]);
        assert_eq!(decoded.user_data, None);
    }
    
    #[test]
    fn test_subscription_metadata_v2() {
        let mut metadata = SubscriptionMetadata::new(
            ProtocolVersion::V2,
            vec!["topic1".to_string()],
        );
        
        let mut owned = HashMap::new();
        owned.insert("topic1".to_string(), vec![0, 1, 2]);
        metadata.owned_partitions = Some(owned);
        
        let encoded = metadata.encode().unwrap();
        let decoded = SubscriptionMetadata::decode(&encoded).unwrap();
        
        assert_eq!(decoded.version, ProtocolVersion::V2);
        assert_eq!(decoded.owned_partitions.unwrap()["topic1"], vec![0, 1, 2]);
    }
    
    #[test]
    fn test_protocol_compatibility() {
        assert!(ProtocolCompatibility::are_compatible(
            ProtocolVersion::V0,
            ProtocolVersion::V1
        ));
        
        let versions = vec![
            ProtocolVersion::V2,
            ProtocolVersion::V1,
            ProtocolVersion::V3,
        ];
        
        assert_eq!(
            ProtocolCompatibility::get_common_version(&versions),
            Some(ProtocolVersion::V1)
        );
    }
}