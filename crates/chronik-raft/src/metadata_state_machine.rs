//! Raft state machine for metadata replication
//!
//! This state machine applies committed metadata events to the underlying ChronikMetaLogStore.
//! It is used by the Raft replica for the `__meta` partition.

use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use chronik_common::metadata::{
    ChronikMetaLogStore,
    MetadataEvent,
    MetadataError,
};

/// Metadata state machine for Raft
///
/// This applies committed Raft entries (serialized MetadataEvents) to the
/// underlying ChronikMetaLogStore, ensuring all nodes have consistent metadata.
pub struct MetadataStateMachine {
    /// Underlying metadata store
    inner: Arc<ChronikMetaLogStore>,

    /// Last applied index (for tracking)
    last_applied: u64,
}

impl MetadataStateMachine {
    /// Create a new metadata state machine
    ///
    /// # Arguments
    /// * `inner` - The ChronikMetaLogStore to apply events to
    pub fn new(inner: Arc<ChronikMetaLogStore>) -> Self {
        info!("Creating metadata state machine");

        Self {
            inner,
            last_applied: 0,
        }
    }

    /// Get the last applied index
    pub fn last_applied(&self) -> u64 {
        self.last_applied
    }
}

// For Raft feature enabled builds
#[cfg(feature = "raft")]
use chronik_raft::{RaftEntry, RaftError, SnapshotData, StateMachine};

#[cfg(feature = "raft")]
type Result<T> = std::result::Result<T, RaftError>;

#[cfg(feature = "raft")]
#[async_trait]
impl StateMachine for MetadataStateMachine {
    /// Apply a committed Raft entry to the metadata store
    ///
    /// # Arguments
    /// * `entry` - Raft entry containing serialized MetadataEvent
    ///
    /// # Returns
    /// Ok with confirmation bytes, or Err if application failed
    async fn apply(&mut self, entry: &RaftEntry) -> Result<Bytes> {
        debug!(
            "STATE_MACHINE: Applying metadata Raft entry {} ({} bytes)",
            entry.index,
            entry.data.len()
        );

        // Deserialize MetadataEvent from entry data
        let event: MetadataEvent = bincode::deserialize(&entry.data)
            .map_err(|e| RaftError::SerializationError(format!(
                "Failed to deserialize metadata event: {}",
                e
            )))?;

        info!(
            "STATE_MACHINE: Applying metadata event at index {}: {:?}",
            entry.index,
            event.payload
        );

        // Apply to inner store (bypassing Raft layer)
        self.inner
            .append_event_direct(event.payload)
            .await
            .map_err(|e| RaftError::StorageError(format!(
                "Failed to apply metadata event: {}",
                e
            )))?;

        // Update last applied index
        self.last_applied = entry.index;

        debug!(
            "STATE_MACHINE: Successfully applied metadata entry {}",
            entry.index
        );

        Ok(Bytes::from(format!("applied:{}", entry.index)))
    }

    /// Create a snapshot of the metadata state
    ///
    /// # Arguments
    /// * `last_index` - Last included index in the snapshot
    /// * `last_term` - Last included term in the snapshot
    ///
    /// # Returns
    /// Snapshot data containing serialized metadata state
    async fn snapshot(&self, last_index: u64, last_term: u64) -> Result<SnapshotData> {
        info!(
            "Creating metadata snapshot: last_index={}, last_term={}",
            last_index, last_term
        );

        // Get current state from inner store
        let state = self.inner.state().read().clone();

        // Serialize state to bincode
        let data = bincode::serialize(&state)
            .map_err(|e| RaftError::SerializationError(format!(
                "Failed to serialize metadata state: {}",
                e
            )))?;

        info!(
            "Created metadata snapshot at index {}: {} bytes",
            last_index,
            data.len()
        );

        Ok(SnapshotData {
            last_index,
            last_term,
            conf_state: vec![],
            data,
        })
    }

    /// Restore metadata state from a snapshot
    ///
    /// # Arguments
    /// * `snapshot` - Snapshot data to restore from
    ///
    /// # Returns
    /// Ok if restore succeeded, Err otherwise
    async fn restore(&mut self, snapshot: &SnapshotData) -> Result<()> {
        info!(
            "Restoring metadata from snapshot: last_index={}",
            snapshot.last_index
        );

        // Deserialize state from snapshot data
        let state: super::MetadataState = bincode::deserialize(&snapshot.data)
            .map_err(|e| RaftError::SerializationError(format!(
                "Failed to deserialize metadata state: {}",
                e
            )))?;

        // Replace inner store state
        {
            let mut inner_state = self.inner.state().write();
            *inner_state = state;
        }

        // Update last applied index
        self.last_applied = snapshot.last_index;

        info!(
            "Restored metadata state from snapshot at index {}",
            snapshot.last_index
        );

        Ok(())
    }

    fn last_applied(&self) -> u64 {
        self.last_applied
    }
}

// Expose state() method on ChronikMetaLogStore for snapshot/restore

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{MockWal, MetadataEventPayload, TopicConfig};
    use std::path::PathBuf;

    #[cfg(feature = "raft")]
    #[tokio::test]
    async fn test_metadata_state_machine_apply() {
        // Create inner store
        let wal = Arc::new(MockWal::new());
        let inner = Arc::new(
            ChronikMetaLogStore::new(wal, PathBuf::from("/tmp/test_metadata_sm"))
                .await
                .unwrap(),
        );

        // Create state machine
        let mut sm = MetadataStateMachine::new(inner.clone());

        // Create a metadata event
        let event = MetadataEvent::new(MetadataEventPayload::TopicCreated {
            name: "test-topic".to_string(),
            config: TopicConfig::default(),
        });

        // Serialize to Raft entry
        let data = bincode::serialize(&event).unwrap();
        let raft_entry = RaftEntry {
            index: 1,
            term: 1,
            data,
        };

        // Apply via state machine
        let result = sm.apply(&raft_entry).await;
        assert!(result.is_ok());

        // Verify topic was created in inner store
        let topic = inner.get_topic("test-topic").await.unwrap();
        assert!(topic.is_some());

        // Verify last applied index
        assert_eq!(sm.last_applied(), 1);
    }

    #[cfg(feature = "raft")]
    #[tokio::test]
    async fn test_metadata_state_machine_snapshot() {
        // Create inner store with some data
        let wal = Arc::new(MockWal::new());
        let inner = Arc::new(
            ChronikMetaLogStore::new(wal, PathBuf::from("/tmp/test_metadata_sm_snap"))
                .await
                .unwrap(),
        );

        // Add some topics
        inner.create_topic("topic1", TopicConfig::default()).await.unwrap();
        inner.create_topic("topic2", TopicConfig::default()).await.unwrap();

        // Create state machine
        let sm = MetadataStateMachine::new(inner.clone());

        // Create snapshot
        let snapshot = sm.snapshot(10, 5).await.unwrap();

        assert_eq!(snapshot.last_index, 10);
        assert_eq!(snapshot.last_term, 5);
        assert!(!snapshot.data.is_empty());

        // Verify we can deserialize the state
        let state: super::MetadataState = bincode::deserialize(&snapshot.data).unwrap();
        assert_eq!(state.topics.len(), 2);
    }

    #[cfg(feature = "raft")]
    #[tokio::test]
    async fn test_metadata_state_machine_restore() {
        // Create inner store with initial data
        let wal = Arc::new(MockWal::new());
        let inner = Arc::new(
            ChronikMetaLogStore::new(wal, PathBuf::from("/tmp/test_metadata_sm_restore"))
                .await
                .unwrap(),
        );

        inner.create_topic("old-topic", TopicConfig::default()).await.unwrap();

        // Create state machine and snapshot
        let mut sm = MetadataStateMachine::new(inner.clone());
        let snapshot = sm.snapshot(5, 3).await.unwrap();

        // Create new inner store (simulating new node)
        let wal2 = Arc::new(MockWal::new());
        let inner2 = Arc::new(
            ChronikMetaLogStore::new(wal2, PathBuf::from("/tmp/test_metadata_sm_restore2"))
                .await
                .unwrap(),
        );

        // Create new state machine and restore from snapshot
        let mut sm2 = MetadataStateMachine::new(inner2.clone());
        sm2.restore(&snapshot).await.unwrap();

        // Verify state was restored
        let topics = inner2.list_topics().await.unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].name, "old-topic");

        assert_eq!(sm2.last_applied(), 5);
    }
}
