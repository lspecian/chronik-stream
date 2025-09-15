//! Metadata API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Metadata request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataRequest {
    /// Topics to fetch metadata for (null for all topics)
    pub topics: Option<Vec<String>>,
    /// Whether to allow auto topic creation
    pub allow_auto_topic_creation: bool,
    /// Include cluster authorized operations (v8+)
    pub include_cluster_authorized_operations: bool,
    /// Include topic authorized operations (v8+)
    pub include_topic_authorized_operations: bool,
}

impl KafkaDecodable for MetadataRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let topic_count = decoder.read_i32()?;
        let topics = if topic_count < 0 {
            None
        } else {
            let mut topic_list = Vec::with_capacity(topic_count as usize);
            for _ in 0..topic_count {
                let topic = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
                topic_list.push(topic);
            }
            Some(topic_list)
        };

        let allow_auto_topic_creation = if version >= 4 {
            decoder.read_bool()?
        } else {
            true // Default for older versions
        };

        let include_cluster_authorized_operations = if version >= 8 {
            decoder.read_bool()?
        } else {
            false
        };

        let include_topic_authorized_operations = if version >= 8 {
            decoder.read_bool()?
        } else {
            false
        };

        Ok(MetadataRequest {
            topics,
            allow_auto_topic_creation,
            include_cluster_authorized_operations,
            include_topic_authorized_operations,
        })
    }
}

impl KafkaEncodable for MetadataRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        if let Some(ref topics) = self.topics {
            encoder.write_i32(topics.len() as i32);
            for topic in topics {
                encoder.write_string(Some(topic));
            }
        } else {
            encoder.write_i32(-1);
        }

        if version >= 4 {
            encoder.write_bool(self.allow_auto_topic_creation);
        }

        if version >= 8 {
            encoder.write_bool(self.include_cluster_authorized_operations);
            encoder.write_bool(self.include_topic_authorized_operations);
        }

        Ok(())
    }
}

/// Metadata broker info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataBroker {
    /// Node ID
    pub node_id: i32,
    /// Host name or IP
    pub host: String,
    /// Port number
    pub port: i32,
    /// Rack identifier
    pub rack: Option<String>,
}

impl KafkaEncodable for MetadataBroker {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.node_id);
        encoder.write_string(Some(&self.host));
        encoder.write_i32(self.port);

        if version >= 1 {
            encoder.write_string(self.rack.as_deref());
        }

        Ok(())
    }
}

/// Metadata partition info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataPartition {
    /// Error code
    pub error_code: i16,
    /// Partition index
    pub partition_index: i32,
    /// Leader node ID
    pub leader_id: i32,
    /// Leader epoch (v7+)
    pub leader_epoch: Option<i32>,
    /// Replica nodes
    pub replica_nodes: Vec<i32>,
    /// In-sync replica nodes
    pub isr_nodes: Vec<i32>,
    /// Offline replica nodes (v5+)
    pub offline_replicas: Option<Vec<i32>>,
}

impl KafkaEncodable for MetadataPartition {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i16(self.error_code);
        encoder.write_i32(self.partition_index);
        encoder.write_i32(self.leader_id);

        if version >= 7 {
            encoder.write_i32(self.leader_epoch.unwrap_or(-1));
        }

        encoder.write_i32(self.replica_nodes.len() as i32);
        for replica in &self.replica_nodes {
            encoder.write_i32(*replica);
        }

        encoder.write_i32(self.isr_nodes.len() as i32);
        for isr in &self.isr_nodes {
            encoder.write_i32(*isr);
        }

        if version >= 5 {
            if let Some(ref offline) = self.offline_replicas {
                encoder.write_i32(offline.len() as i32);
                for replica in offline {
                    encoder.write_i32(*replica);
                }
            } else {
                encoder.write_i32(0);
            }
        }

        Ok(())
    }
}

/// Metadata topic info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataTopic {
    /// Error code
    pub error_code: i16,
    /// Topic name
    pub name: String,
    /// Whether topic is internal
    pub is_internal: bool,
    /// Partition metadata
    pub partitions: Vec<MetadataPartition>,
    /// Topic authorized operations (v8+)
    pub topic_authorized_operations: Option<i32>,
}

impl KafkaEncodable for MetadataTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i16(self.error_code);
        encoder.write_string(Some(&self.name));

        if version >= 1 {
            encoder.write_bool(self.is_internal);
        }

        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }

        if version >= 8 {
            encoder.write_i32(self.topic_authorized_operations.unwrap_or(-2147483648));
        }

        Ok(())
    }
}

/// Metadata response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Broker metadata
    pub brokers: Vec<MetadataBroker>,
    /// Cluster ID (v2+)
    pub cluster_id: Option<String>,
    /// Controller node ID (v1+)
    pub controller_id: i32,
    /// Topic metadata
    pub topics: Vec<MetadataTopic>,
    /// Cluster authorized operations (v8+)
    pub cluster_authorized_operations: Option<i32>,
}

impl MetadataResponse {
    pub fn make_response(
        throttle_time_ms: i32,
        brokers: Vec<MetadataBroker>,
        cluster_id: Option<String>,
        controller_id: i32,
        topics: Vec<MetadataTopic>,
        cluster_authorized_operations: Option<i32>,
    ) -> Self {
        Self {
            throttle_time_ms,
            brokers,
            cluster_id,
            controller_id,
            topics,
            cluster_authorized_operations,
        }
    }
}

impl KafkaEncodable for MetadataResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        if version >= 3 {
            encoder.write_i32(self.throttle_time_ms);
        }

        encoder.write_i32(self.brokers.len() as i32);
        for broker in &self.brokers {
            broker.encode(encoder, version)?;
        }

        if version >= 2 {
            encoder.write_string(self.cluster_id.as_deref());
        }

        if version >= 1 {
            encoder.write_i32(self.controller_id);
        }

        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }

        if version >= 8 {
            encoder.write_i32(self.cluster_authorized_operations.unwrap_or(-2147483648));
        }

        Ok(())
    }
}

/// Error codes for Metadata API
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const LEADER_NOT_AVAILABLE: i16 = 5;
    pub const NOT_LEADER_FOR_PARTITION: i16 = 6;
    pub const REPLICA_NOT_AVAILABLE: i16 = 9;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const INVALID_TOPIC_EXCEPTION: i16 = 17;
    pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
}