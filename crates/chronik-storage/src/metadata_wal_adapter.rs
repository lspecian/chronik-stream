//! WAL adapter for metadata storage.
//!
//! Bridges the ChronikMetaLogStore with the actual WAL system.

use std::sync::Arc;
use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::{debug, info, error};

use chronik_common::metadata::{
    MetadataEvent, MetadataEventPayload, MetaLogWalInterface,
    MetadataError, Result, METADATA_TOPIC,
};

use chronik_wal::{
    manager::{WalManager, TopicPartition},
    record::WalRecord,
    config::WalConfig,
};

/// WAL adapter for metadata storage
pub struct WalMetadataAdapter {
    wal_manager: Arc<RwLock<WalManager>>,
    partition: i32,
}

impl WalMetadataAdapter {
    /// Create a new WAL metadata adapter with recovery
    pub async fn new(wal_config: WalConfig) -> Result<Self> {
        // Use recover instead of new to load existing WAL data
        let wal_manager = WalManager::recover(&wal_config).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to recover WAL manager: {}", e)))?;

        Ok(Self {
            wal_manager: Arc::new(RwLock::new(wal_manager)),
            partition: 0, // Metadata always uses partition 0
        })
    }

    /// Create from existing WAL manager
    pub fn from_wal_manager(wal_manager: Arc<RwLock<WalManager>>) -> Self {
        Self {
            wal_manager,
            partition: 0,
        }
    }

    /// Ensure the metadata topic exists
    pub async fn ensure_metadata_topic(&self) -> Result<()> {
        // The WAL manager will create the partition on first write
        info!("Metadata topic {} ready for use", METADATA_TOPIC);
        Ok(())
    }

    /// Convert metadata event to WAL record
    async fn event_to_wal_record(&self, event: &MetadataEvent) -> Result<WalRecord> {
        let data = serde_json::to_vec(event)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to serialize event: {}", e)))?;

        // Get the next offset from WAL manager
        let tp = TopicPartition {
            topic: METADATA_TOPIC.to_string(),
            partition: self.partition,
        };
        let wal_manager = self.wal_manager.read().await;
        let next_offset = match wal_manager.get_latest_offset(tp).await {
            Ok(offset) => offset + 1,
            Err(e) => {
                // If the partition doesn't exist yet (first run), start at 0
                if e.to_string().contains("Segment not found") {
                    debug!("No WAL segments found, starting at offset 0");
                    0
                } else {
                    return Err(MetadataError::StorageError(format!("Failed to get latest offset: {}", e)));
                }
            }
        };

        Ok(WalRecord::new(
            next_offset,
            Some(event.event_id.as_bytes().to_vec()),
            data,
            event.timestamp.timestamp_millis(),
        ))
    }

    /// Convert WAL record to metadata event
    fn wal_record_to_event(&self, record: &WalRecord) -> Result<MetadataEvent> {
        // Metadata events use V1 records (individual events, not batches)
        match record {
            chronik_wal::record::WalRecord::V1 { value, .. } => {
                serde_json::from_slice(value)
                    .map_err(|e| MetadataError::SerializationError(format!("Failed to deserialize event: {}", e)))
            }
            _ => Err(MetadataError::SerializationError("Expected V1 record for metadata events".into()))
        }
    }
}

#[async_trait]
impl MetaLogWalInterface for WalMetadataAdapter {
    async fn append_metadata_event(&self, event: &MetadataEvent) -> Result<u64> {
        let wal_record = self.event_to_wal_record(event).await?;
        let tp = TopicPartition {
            topic: METADATA_TOPIC.to_string(),
            partition: self.partition,
        };

        // Append to WAL
        let mut wal_manager = self.wal_manager.write().await;
        let record_offset = wal_record.get_offset().unwrap_or(0);
        wal_manager.append(tp.topic.clone(), tp.partition, vec![wal_record.clone()]).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to append to WAL: {}", e)))?;

        debug!("Wrote metadata event {} at offset {}", event.event_id, record_offset);
        Ok(record_offset as u64)
    }

    async fn read_metadata_events(&self, from_offset: u64) -> Result<Vec<MetadataEvent>> {
        let tp = TopicPartition {
            topic: METADATA_TOPIC.to_string(),
            partition: self.partition,
        };

        // Read from WAL
        let wal_manager = self.wal_manager.read().await;
        let records = match wal_manager.read_from(
            &tp.topic,
            tp.partition,
            from_offset as i64,
            1000  // Max records to read
        ).await {
            Ok(records) => records,
            Err(e) => {
                // If the partition doesn't exist yet (first run), return empty
                if e.to_string().contains("Segment not found") {
                    debug!("No WAL segments found for metadata topic, returning empty");
                    return Ok(Vec::new());
                }
                return Err(MetadataError::StorageError(format!("Failed to read from WAL: {}", e)));
            }
        };

        // Convert WAL records back to events
        let mut events = Vec::new();
        for record in records {
            let record_offset = record.get_offset().unwrap_or(-1);
            match self.wal_record_to_event(&record) {
                Ok(event) => events.push(event),
                Err(e) => {
                    error!("Failed to deserialize event at offset {}: {}", record_offset, e);
                    // Skip corrupted events
                }
            }
        }

        debug!("Read {} metadata events from offset {}", events.len(), from_offset);
        Ok(events)
    }

    async fn get_latest_offset(&self) -> Result<u64> {
        let tp = TopicPartition {
            topic: METADATA_TOPIC.to_string(),
            partition: self.partition,
        };

        // Get the latest offset from WAL
        let wal_manager = self.wal_manager.read().await;
        let offset = match wal_manager.get_latest_offset(tp).await {
            Ok(offset) => offset,
            Err(e) => {
                // If the partition doesn't exist yet (first run), return 0
                if e.to_string().contains("Segment not found") {
                    debug!("No WAL segments found for metadata topic, returning offset 0");
                    return Ok(0);
                }
                return Err(MetadataError::StorageError(format!("Failed to get latest offset: {}", e)));
            }
        };

        Ok(offset as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_common::metadata::TopicConfig;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_wal_adapter_round_trip() {
        let temp_dir = tempdir().unwrap();
        let wal_config = WalConfig {
            enabled: true,
            data_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };

        let adapter = WalMetadataAdapter::new(wal_config).await.unwrap();

        // Create a test event
        let event = MetadataEvent::new(MetadataEventPayload::TopicCreated {
            name: "test-topic".to_string(),
            config: TopicConfig::default(),
        });

        // Write the event
        let offset = adapter.append_metadata_event(&event).await.unwrap();
        assert_eq!(offset, 0);

        // Read it back
        let events = adapter.read_metadata_events(0).await.unwrap();
        assert_eq!(events.len(), 1);

        // Verify the event matches
        assert_eq!(events[0].event_id, event.event_id);
        match &events[0].payload {
            MetadataEventPayload::TopicCreated { name, .. } => {
                assert_eq!(name, "test-topic");
            }
            _ => panic!("Unexpected event type"),
        }
    }

    #[tokio::test]
    async fn test_multiple_events() {
        let temp_dir = tempdir().unwrap();
        let wal_config = WalConfig {
            enabled: true,
            data_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };

        let adapter = WalMetadataAdapter::new(wal_config).await.unwrap();

        // Write multiple events
        for i in 0..5 {
            let event = MetadataEvent::new(MetadataEventPayload::TopicCreated {
                name: format!("topic-{}", i),
                config: TopicConfig::default(),
            });
            adapter.append_metadata_event(&event).await.unwrap();
        }

        // Read all events
        let events = adapter.read_metadata_events(0).await.unwrap();
        assert_eq!(events.len(), 5);

        // Read from offset 3
        let events = adapter.read_metadata_events(3).await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn test_latest_offset() {
        let temp_dir = tempdir().unwrap();
        let wal_config = WalConfig {
            enabled: true,
            data_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };

        let adapter = WalMetadataAdapter::new(wal_config).await.unwrap();

        // Initially should be 0
        let offset = adapter.get_latest_offset().await.unwrap();
        assert_eq!(offset, 0);

        // Write some events
        for _ in 0..3 {
            let event = MetadataEvent::new(MetadataEventPayload::TopicDeleted {
                name: "some-topic".to_string(),
            });
            adapter.append_metadata_event(&event).await.unwrap();
        }

        // Should now be 3
        let offset = adapter.get_latest_offset().await.unwrap();
        assert_eq!(offset, 3);
    }
}