//! Metadata event definitions for event-sourced metadata storage.

use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{
    TopicConfig, SegmentMetadata, BrokerMetadata, BrokerStatus,
    PartitionAssignment, ConsumerGroupMetadata, ConsumerOffset,
};

/// Version for metadata event schema
pub const METADATA_EVENT_VERSION: u32 = 1;

/// A transactional offset for atomic commits
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionalOffset {
    pub topic: String,
    pub partition: u32,
    pub offset: i64,
    pub metadata: Option<String>,
}

/// A partition involved in a transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionPartition {
    pub topic: String,
    pub partition: u32,
}

/// Metadata event that can be stored in the WAL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataEvent {
    /// Unique event ID
    pub event_id: Uuid,
    /// Event timestamp
    pub timestamp: DateTime<Utc>,
    /// Event version for compatibility
    pub version: u32,
    /// Node ID that created this event (for conflict resolution)
    pub created_by_node: u64,
    /// The actual event payload
    pub payload: MetadataEventPayload,
}

impl MetadataEvent {
    /// Create a new metadata event with the given payload
    pub fn new(payload: MetadataEventPayload) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            version: METADATA_EVENT_VERSION,
            created_by_node: 0, // Default to 0 for single-node mode
            payload,
        }
    }

    /// Create a new metadata event with node ID (for cluster mode)
    pub fn new_with_node(payload: MetadataEventPayload, node_id: u64) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            version: METADATA_EVENT_VERSION,
            created_by_node: node_id,
            payload,
        }
    }

    /// Create an event from a specific timestamp (for replay)
    pub fn with_timestamp(payload: MetadataEventPayload, timestamp: DateTime<Utc>) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            timestamp,
            version: METADATA_EVENT_VERSION,
            created_by_node: 0,
            payload,
        }
    }

    /// Serialize event to bytes for WAL storage (JSON format)
    ///
    /// v2.2.9 CRITICAL FIX: Switched from bincode to JSON because bincode doesn't
    /// support internally tagged enums (#[serde(tag = "type")]), which causes
    /// deserialization to fail with "Bincode does not support deserialize_any".
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self)
            .map_err(|e| format!("Failed to serialize MetadataEvent: {}", e))
    }

    /// Deserialize event from bytes (JSON format)
    ///
    /// v2.2.9: Supports JSON format (current) and falls back to bincode for
    /// backward compatibility with existing WAL files.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        // Try JSON first (current format)
        match serde_json::from_slice(data) {
            Ok(event) => Ok(event),
            Err(json_err) => {
                // Fall back to bincode for backward compatibility
                // This will fail for internally tagged enums, but we try anyway
                bincode::deserialize(data)
                    .map_err(|bincode_err| {
                        format!(
                            "Failed to deserialize MetadataEvent: JSON error: {}, Bincode error: {}",
                            json_err, bincode_err
                        )
                    })
            }
        }
    }
}

/// Metadata event payload variants
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MetadataEventPayload {
    // Topic events
    TopicCreated {
        name: String,
        config: TopicConfig,
    },
    TopicUpdated {
        name: String,
        config: TopicConfig,
    },
    TopicDeleted {
        name: String,
    },

    // Segment events
    SegmentCreated {
        metadata: SegmentMetadata,
    },
    SegmentDeleted {
        topic: String,
        partition: u32,
        segment_id: String,
    },

    // Broker events
    BrokerRegistered {
        metadata: BrokerMetadata,
    },
    BrokerStatusChanged {
        broker_id: i32,
        status: BrokerStatus,
    },
    BrokerRemoved {
        broker_id: i32,
    },

    // Partition assignment events
    PartitionAssigned {
        assignment: PartitionAssignment,
    },
    PartitionReassigned {
        topic: String,
        partition: u32,
        from_broker: i32,
        to_broker: i32,
    },

    // Consumer group events
    ConsumerGroupCreated {
        metadata: ConsumerGroupMetadata,
    },
    ConsumerGroupUpdated {
        metadata: ConsumerGroupMetadata,
    },
    ConsumerGroupDeleted {
        group_id: String,
    },

    // Offset events
    OffsetCommitted {
        offset: ConsumerOffset,
    },
    OffsetsReset {
        group_id: String,
        topic: String,
        partition: u32,
        new_offset: i64,
    },

    // High watermark events (for partition replication)
    HighWatermarkUpdated {
        topic: String,
        partition: i32,
        new_watermark: i64,
    },

    // Transactional offset events
    TransactionalOffsetCommit {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
        offsets: Vec<TransactionalOffset>,
    },

    // Transaction lifecycle events
    BeginTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        transaction_timeout_ms: i32,
    },
    AddPartitionsToTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        partitions: Vec<TransactionPartition>,
    },
    AddOffsetsToTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
    },
    PrepareCommit {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    },
    PrepareAbort {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    },
    CommitTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        committed_partitions: Vec<TransactionPartition>,
        committed_offsets: Vec<TransactionalOffset>,
    },
    AbortTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        aborted_partitions: Vec<TransactionPartition>,
    },
    ProducerFenced {
        transactional_id: String,
        old_producer_id: i64,
        old_producer_epoch: i16,
        new_producer_id: i64,
        new_producer_epoch: i16,
    },

    // Configuration events
    ConfigUpdated {
        scope: ConfigScope,
        key: String,
        value: String,
    },
    ConfigDeleted {
        scope: ConfigScope,
        key: String,
    },

    // Snapshot events
    SnapshotCreated {
        snapshot_id: String,
        offset: u64,
        metadata: SnapshotMetadata,
    },
    SnapshotDeleted {
        snapshot_id: String,
    },

    // Migration events
    MigrationStarted {
        from_version: String,
        to_version: String,
    },
    MigrationCompleted {
        from_version: String,
        to_version: String,
        success: bool,
    },
}

/// Configuration scope for config events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfigScope {
    Cluster,
    Topic(String),
    Broker(i32),
}

/// Metadata about a snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Path to the snapshot file
    pub path: String,
    /// Size of the snapshot in bytes
    pub size: u64,
    /// Number of events included in the snapshot
    pub event_count: u64,
    /// Compression used (e.g., "zstd", "none")
    pub compression: String,
    /// Checksum of the snapshot data
    pub checksum: String,
}

/// Trait for applying events to build state
pub trait EventApplicator {
    /// Apply an event to update the state
    fn apply_event(&mut self, event: &MetadataEvent) -> Result<(), String>;

    /// Get the current version of the state
    fn version(&self) -> u64;
}

/// Event log for storing and replaying metadata events
#[derive(Debug, Clone)]
pub struct EventLog {
    events: Vec<MetadataEvent>,
}

impl EventLog {
    /// Create a new empty event log
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
        }
    }

    /// Add an event to the log
    pub fn append(&mut self, event: MetadataEvent) {
        self.events.push(event);
    }

    /// Get all events in the log
    pub fn events(&self) -> &[MetadataEvent] {
        &self.events
    }

    /// Get events after a specific timestamp
    pub fn events_after(&self, timestamp: DateTime<Utc>) -> Vec<&MetadataEvent> {
        self.events
            .iter()
            .filter(|e| e.timestamp > timestamp)
            .collect()
    }

    /// Get events for a specific topic
    pub fn events_for_topic(&self, topic: &str) -> Vec<&MetadataEvent> {
        self.events
            .iter()
            .filter(|e| match &e.payload {
                MetadataEventPayload::TopicCreated { name, .. } => name == topic,
                MetadataEventPayload::TopicUpdated { name, .. } => name == topic,
                MetadataEventPayload::TopicDeleted { name } => name == topic,
                MetadataEventPayload::SegmentCreated { metadata } => metadata.topic == topic,
                MetadataEventPayload::SegmentDeleted { topic: t, .. } => t == topic,
                MetadataEventPayload::PartitionAssigned { assignment } => assignment.topic == topic,
                MetadataEventPayload::PartitionReassigned { topic: t, .. } => t == topic,
                MetadataEventPayload::OffsetCommitted { offset } => offset.topic == topic,
                MetadataEventPayload::OffsetsReset { topic: t, .. } => t == topic,
                MetadataEventPayload::ConfigUpdated { scope, .. } => {
                    matches!(scope, ConfigScope::Topic(t) if t == topic)
                }
                MetadataEventPayload::ConfigDeleted { scope, .. } => {
                    matches!(scope, ConfigScope::Topic(t) if t == topic)
                }
                _ => false,
            })
            .collect()
    }

    /// Clear the event log
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Get the number of events in the log
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Check if the log is empty
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_creation() {
        let payload = MetadataEventPayload::TopicCreated {
            name: "test-topic".to_string(),
            config: TopicConfig::default(),
        };

        let event = MetadataEvent::new(payload.clone());

        assert_eq!(event.version, METADATA_EVENT_VERSION);
        assert_eq!(event.created_by_node, 0); // Default single-node mode
        assert!(matches!(event.payload, MetadataEventPayload::TopicCreated { .. }));
    }

    #[test]
    fn test_event_creation_with_node() {
        let payload = MetadataEventPayload::TopicCreated {
            name: "test-topic".to_string(),
            config: TopicConfig::default(),
        };

        let event = MetadataEvent::new_with_node(payload.clone(), 42);

        assert_eq!(event.version, METADATA_EVENT_VERSION);
        assert_eq!(event.created_by_node, 42);
        assert!(matches!(event.payload, MetadataEventPayload::TopicCreated { .. }));
    }

    #[test]
    fn test_event_log() {
        let mut log = EventLog::new();

        let event1 = MetadataEvent::new(MetadataEventPayload::TopicCreated {
            name: "topic1".to_string(),
            config: TopicConfig::default(),
        });

        let event2 = MetadataEvent::new(MetadataEventPayload::TopicCreated {
            name: "topic2".to_string(),
            config: TopicConfig::default(),
        });

        log.append(event1);
        log.append(event2);

        assert_eq!(log.len(), 2);
        assert!(!log.is_empty());

        let topic1_events = log.events_for_topic("topic1");
        assert_eq!(topic1_events.len(), 1);
    }

    #[test]
    fn test_event_serialization() {
        let payload = MetadataEventPayload::OffsetCommitted {
            offset: ConsumerOffset {
                group_id: "test-group".to_string(),
                topic: "test-topic".to_string(),
                partition: 0,
                offset: 42,
                metadata: None,
                commit_timestamp: Utc::now(),
            },
        };

        let event = MetadataEvent::new(payload);

        // Serialize to JSON
        let json = serde_json::to_string(&event).unwrap();

        // Deserialize back
        let deserialized: MetadataEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(event.event_id, deserialized.event_id);
        assert_eq!(event.version, deserialized.version);
        assert_eq!(event.created_by_node, deserialized.created_by_node);
    }

    #[test]
    fn test_event_binary_serialization() {
        let payload = MetadataEventPayload::TopicCreated {
            name: "test-topic".to_string(),
            config: TopicConfig::default(),
        };

        let event = MetadataEvent::new_with_node(payload, 1);

        // Serialize to bytes
        let bytes = event.to_bytes().unwrap();

        // Deserialize back
        let deserialized = MetadataEvent::from_bytes(&bytes).unwrap();

        assert_eq!(event.event_id, deserialized.event_id);
        assert_eq!(event.version, deserialized.version);
        assert_eq!(event.created_by_node, deserialized.created_by_node);
        assert!(matches!(deserialized.payload, MetadataEventPayload::TopicCreated { .. }));
    }

    #[test]
    fn test_high_watermark_event() {
        let payload = MetadataEventPayload::HighWatermarkUpdated {
            topic: "test-topic".to_string(),
            partition: 0,
            new_watermark: 42,
        };

        let event = MetadataEvent::new_with_node(payload, 1);
        assert!(matches!(event.payload, MetadataEventPayload::HighWatermarkUpdated { .. }));
    }
}