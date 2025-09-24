use bytes::{Buf, BufMut, Bytes, BytesMut};
use chronik_common::Result;

pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const GROUP_COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
}

// DescribeGroups Request (API key 15)
#[derive(Debug)]
pub struct DescribeGroupsRequest {
    pub group_ids: Vec<String>,
    pub include_authorized_operations: bool,  // v3+
}

impl DescribeGroupsRequest {
    pub fn parse(buf: &mut Bytes, version: i16) -> Result<Self> {
        use crate::parser::Decoder;
        
        let mut decoder = Decoder::new(buf);
        
        let group_ids = if version >= 5 {
            // Flexible version uses compact arrays
            let count = decoder.read_unsigned_varint()? as i32 - 1;
            if count < 0 {
                vec![]
            } else {
                let mut groups = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    if let Some(group_id) = decoder.read_compact_string()? {
                        groups.push(group_id);
                    }
                }
                groups
            }
        } else {
            let count = decoder.read_i32()?;
            let mut groups = Vec::with_capacity(count as usize);
            for _ in 0..count {
                if let Some(group_id) = decoder.read_string()? {
                    groups.push(group_id);
                }
            }
            groups
        };
        
        let include_authorized_operations = if version >= 3 {
            decoder.read_i8()? != 0
        } else {
            false
        };
        
        // Skip tagged fields for flexible versions
        if version >= 5 {
            let tag_section_size = decoder.read_unsigned_varint()? as usize;
            decoder.advance(tag_section_size)?;
        }
        
        Ok(Self {
            group_ids,
            include_authorized_operations,
        })
    }
}

// DescribeGroups Response
#[derive(Debug)]
pub struct DescribeGroupsResponse {
    pub throttle_time_ms: i32,  // v1+
    pub groups: Vec<DescribedGroup>,
}

#[derive(Debug)]
pub struct DescribedGroup {
    pub error_code: i16,
    pub group_id: String,
    pub group_state: String,
    pub protocol_type: String,
    pub protocol_data: String,
    pub members: Vec<GroupMember>,
    pub authorized_operations: i32,  // v3+
}

#[derive(Debug)]
pub struct GroupMember {
    pub member_id: String,
    pub group_instance_id: Option<String>,  // v4+
    pub client_id: String,
    pub client_host: String,
    pub member_metadata: Vec<u8>,
    pub member_assignment: Vec<u8>,
}

impl DescribeGroupsResponse {
    pub fn encode(&self, version: i16) -> Bytes {
        use crate::parser::Encoder;
        
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        // Throttle time (v1+)
        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }
        
        // Groups array
        if version >= 5 {
            // Flexible version uses compact arrays
            encoder.write_compact_array_len(self.groups.len());
            for group in &self.groups {
                encoder.write_i16(group.error_code);
                encoder.write_compact_string(Some(&group.group_id));
                encoder.write_compact_string(Some(&group.group_state));
                encoder.write_compact_string(Some(&group.protocol_type));
                encoder.write_compact_string(Some(&group.protocol_data));
                
                // Members array
                encoder.write_compact_array_len(group.members.len());
                for member in &group.members {
                    encoder.write_compact_string(Some(&member.member_id));
                    
                    // Group instance ID (v4+)
                    if version >= 4 {
                        encoder.write_compact_string(member.group_instance_id.as_deref());
                    }
                    
                    encoder.write_compact_string(Some(&member.client_id));
                    encoder.write_compact_string(Some(&member.client_host));
                    
                    // Compact bytes for metadata
                    encoder.write_unsigned_varint((member.member_metadata.len() + 1) as u32);
                    encoder.write_raw_bytes(&member.member_metadata);
                    
                    // Compact bytes for assignment
                    encoder.write_unsigned_varint((member.member_assignment.len() + 1) as u32);
                    encoder.write_raw_bytes(&member.member_assignment);
                    
                    // Tagged fields
                    encoder.write_tagged_fields();
                }
                
                // Authorized operations (v3+)
                if version >= 3 {
                    encoder.write_i32(group.authorized_operations);
                }
                
                // Tagged fields
                encoder.write_tagged_fields();
            }
        } else {
            // Non-flexible version
            encoder.write_i32(self.groups.len() as i32);
            for group in &self.groups {
                encoder.write_i16(group.error_code);
                encoder.write_string(Some(&group.group_id));
                encoder.write_string(Some(&group.group_state));
                encoder.write_string(Some(&group.protocol_type));
                encoder.write_string(Some(&group.protocol_data));
                
                // Members array
                encoder.write_i32(group.members.len() as i32);
                for member in &group.members {
                    encoder.write_string(Some(&member.member_id));
                    
                    // Group instance ID (v4+)
                    if version >= 4 {
                        encoder.write_string(member.group_instance_id.as_deref());
                    }
                    
                    encoder.write_string(Some(&member.client_id));
                    encoder.write_string(Some(&member.client_host));
                    encoder.write_bytes(Some(&member.member_metadata));
                    encoder.write_bytes(Some(&member.member_assignment));
                }
                
                // Authorized operations (v3+)
                if version >= 3 {
                    encoder.write_i32(group.authorized_operations);
                }
            }
        }
        
        // Tagged fields for flexible versions
        if version >= 5 {
            encoder.write_tagged_fields();
        }
        
        buf.freeze()
    }
}

// ListConsumerGroupOffsets Request (API key 9 - OffsetFetch)
#[derive(Debug)]
pub struct ListConsumerGroupOffsetsRequest {
    pub group_id: String,
    pub topics: Option<Vec<TopicPartitions>>,  // None means all topics
    pub require_stable: bool,  // v7+
}

#[derive(Debug)]
pub struct TopicPartitions {
    pub name: String,
    pub partition_indexes: Vec<i32>,
}

impl ListConsumerGroupOffsetsRequest {
    pub fn parse(buf: &mut Bytes, version: i16) -> Result<Self> {
        use crate::parser::Decoder;
        
        let mut decoder = Decoder::new(buf);
        
        let group_id = if version >= 8 {
            decoder.read_compact_string()?.unwrap_or_default()
        } else {
            decoder.read_string()?.unwrap_or_default()
        };
        
        let topics = if version >= 2 {
            if version >= 8 {
                // Flexible version - compact nullable array
                let count = decoder.read_unsigned_varint()? as i32 - 1;
                if count < 0 {
                    None
                } else {
                    let mut topics = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        let name = decoder.read_compact_string()?.unwrap_or_default();
                        
                        let partition_count = decoder.read_unsigned_varint()? as i32 - 1;
                        let mut partition_indexes = Vec::with_capacity(partition_count.max(0) as usize);
                        for _ in 0..partition_count {
                            partition_indexes.push(decoder.read_i32()?);
                        }
                        
                        topics.push(TopicPartitions { name, partition_indexes });
                    }
                    Some(topics)
                }
            } else {
                // Non-flexible version
                let count = decoder.read_i32()?;
                if count < 0 {
                    None
                } else {
                    let mut topics = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        let name = decoder.read_string()?.unwrap_or_default();
                        
                        let partition_count = decoder.read_i32()?;
                        let mut partition_indexes = Vec::with_capacity(partition_count as usize);
                        for _ in 0..partition_count {
                            partition_indexes.push(decoder.read_i32()?);
                        }
                        
                        topics.push(TopicPartitions { name, partition_indexes });
                    }
                    Some(topics)
                }
            }
        } else {
            None
        };
        
        let require_stable = if version >= 7 {
            decoder.read_i8()? != 0
        } else {
            false
        };
        
        // Skip tagged fields for flexible versions
        if version >= 8 {
            let tag_section_size = decoder.read_unsigned_varint()? as usize;
            decoder.advance(tag_section_size)?;
        }
        
        Ok(Self {
            group_id,
            topics,
            require_stable,
        })
    }
}

// ListConsumerGroupOffsets Response
#[derive(Debug)]
pub struct ListConsumerGroupOffsetsResponse {
    pub throttle_time_ms: i32,  // v2+
    pub error_code: i16,  // v0-v1
    pub topics: Vec<OffsetFetchResponseTopic>,
}

#[derive(Debug)]
pub struct OffsetFetchResponseTopic {
    pub name: String,
    pub partitions: Vec<OffsetFetchResponsePartition>,
}

#[derive(Debug)]
pub struct OffsetFetchResponsePartition {
    pub partition_index: i32,
    pub committed_offset: i64,
    pub committed_leader_epoch: i32,  // v5+
    pub metadata: Option<String>,
    pub error_code: i16,
}

impl ListConsumerGroupOffsetsResponse {
    pub fn encode(&self, version: i16) -> Bytes {
        use crate::parser::Encoder;
        
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        // Throttle time (v2+)
        if version >= 2 {
            encoder.write_i32(self.throttle_time_ms);
        }
        
        // Error code (v0-v1 only)
        if version <= 1 {
            encoder.write_i16(self.error_code);
        }
        
        // Topics array
        if version >= 8 {
            // Flexible version uses compact arrays
            encoder.write_compact_array_len(self.topics.len());
            for topic in &self.topics {
                encoder.write_compact_string(Some(&topic.name));
                
                encoder.write_compact_array_len(topic.partitions.len());
                for partition in &topic.partitions {
                    encoder.write_i32(partition.partition_index);
                    encoder.write_i64(partition.committed_offset);
                    
                    if version >= 5 {
                        encoder.write_i32(partition.committed_leader_epoch);
                    }
                    
                    encoder.write_compact_string(partition.metadata.as_deref());
                    encoder.write_i16(partition.error_code);
                    
                    // Tagged fields
                    encoder.write_tagged_fields();
                }
                
                // Tagged fields
                encoder.write_tagged_fields();
            }
        } else {
            // Non-flexible version
            encoder.write_i32(self.topics.len() as i32);
            for topic in &self.topics {
                encoder.write_string(Some(&topic.name));
                
                encoder.write_i32(topic.partitions.len() as i32);
                for partition in &topic.partitions {
                    encoder.write_i32(partition.partition_index);
                    encoder.write_i64(partition.committed_offset);
                    
                    if version >= 5 {
                        encoder.write_i32(partition.committed_leader_epoch);
                    }
                    
                    encoder.write_string(partition.metadata.as_deref());
                    encoder.write_i16(partition.error_code);
                }
            }
        }
        
        // Tagged fields for flexible versions
        if version >= 8 {
            encoder.write_tagged_fields();
        }
        
        buf.freeze()
    }
}