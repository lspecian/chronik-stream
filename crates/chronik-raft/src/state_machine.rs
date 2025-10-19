//! State machine abstraction for applying committed Raft entries
//!
//! This module defines the trait that applications must implement to apply
//! committed log entries to their state. In Chronik's case, this means writing
//! messages to segment storage and updating high watermarks.

use crate::{RaftEntry, Result};
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Snapshot metadata for state machine checkpoints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotData {
    /// Last included index in snapshot
    pub last_index: u64,

    /// Last included term in snapshot
    pub last_term: u64,

    /// Configuration state at snapshot time
    pub conf_state: Vec<u64>,

    /// Application-specific snapshot data
    pub data: Vec<u8>,
}

/// State machine trait for applying committed Raft entries
///
/// Applications must implement this trait to define how committed
/// log entries are applied to their state.
#[async_trait]
pub trait StateMachine: Send + Sync {
    /// Apply a committed entry to the state machine
    ///
    /// This is called for each committed entry in order. Implementations
    /// must be idempotent as entries may be applied multiple times during
    /// recovery.
    ///
    /// # Arguments
    /// * `entry` - The committed Raft entry to apply
    ///
    /// # Returns
    /// The result of applying the entry (e.g., response data)
    async fn apply(&mut self, entry: &RaftEntry) -> Result<Bytes>;

    /// Create a snapshot of the current state
    ///
    /// Snapshots allow new nodes to catch up quickly without replaying
    /// the entire log. This should return a consistent point-in-time
    /// snapshot of the state machine.
    ///
    /// # Arguments
    /// * `last_index` - Last log index included in snapshot
    /// * `last_term` - Last log term included in snapshot
    ///
    /// # Returns
    /// Serialized snapshot data
    async fn snapshot(&self, last_index: u64, last_term: u64) -> Result<SnapshotData>;

    /// Restore state from a snapshot
    ///
    /// This is called when installing a snapshot received from the leader
    /// or loading from disk during recovery.
    ///
    /// # Arguments
    /// * `snapshot` - Snapshot data to restore
    async fn restore(&mut self, snapshot: &SnapshotData) -> Result<()>;

    /// Get the last applied index
    ///
    /// Used to track how far the state machine has applied the log.
    fn last_applied(&self) -> u64;
}

/// In-memory state machine for testing
///
/// Stores applied entries in memory without persistence.
pub struct MemoryStateMachine {
    /// Last applied index
    last_applied: u64,

    /// In-memory key-value store
    data: std::collections::HashMap<String, Vec<u8>>,
}

impl MemoryStateMachine {
    /// Create a new in-memory state machine
    pub fn new() -> Self {
        Self {
            last_applied: 0,
            data: std::collections::HashMap::new(),
        }
    }

    /// Get the number of entries applied
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Get a value by key
    pub fn get(&self, key: &str) -> Option<&Vec<u8>> {
        self.data.get(key)
    }
}

impl Default for MemoryStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StateMachine for MemoryStateMachine {
    async fn apply(&mut self, entry: &RaftEntry) -> Result<Bytes> {
        // Parse entry data as key-value pair
        // Format: <key_len:u32><key:bytes><value:bytes>
        if entry.data.len() < 4 {
            return Ok(Bytes::from("empty"));
        }

        let key_len = u32::from_be_bytes([
            entry.data[0],
            entry.data[1],
            entry.data[2],
            entry.data[3],
        ]) as usize;

        if entry.data.len() < 4 + key_len {
            return Ok(Bytes::from("malformed"));
        }

        let key = String::from_utf8_lossy(&entry.data[4..4 + key_len]).to_string();
        let value = entry.data[4 + key_len..].to_vec();

        self.data.insert(key.clone(), value.clone());
        self.last_applied = entry.index;

        tracing::debug!(
            "Applied entry {} to state machine: key={}, value_len={}",
            entry.index,
            key,
            value.len()
        );

        Ok(Bytes::from(format!("applied:{}", entry.index)))
    }

    async fn snapshot(&self, last_index: u64, last_term: u64) -> Result<SnapshotData> {
        // Serialize the entire data map
        let data = bincode::serialize(&self.data)
            .map_err(|e| crate::RaftError::SerializationError(e.to_string()))?;

        Ok(SnapshotData {
            last_index,
            last_term,
            conf_state: vec![],
            data,
        })
    }

    async fn restore(&mut self, snapshot: &SnapshotData) -> Result<()> {
        // Deserialize the data map
        let data: std::collections::HashMap<String, Vec<u8>> = bincode::deserialize(&snapshot.data)
            .map_err(|e| crate::RaftError::SerializationError(e.to_string()))?;

        self.data = data;
        self.last_applied = snapshot.last_index;

        tracing::info!(
            "Restored state machine from snapshot: last_index={}, entries={}",
            snapshot.last_index,
            self.data.len()
        );

        Ok(())
    }

    fn last_applied(&self) -> u64 {
        self.last_applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_kv_entry(index: u64, term: u64, key: &str, value: &[u8]) -> RaftEntry {
        let key_bytes = key.as_bytes();
        let key_len = (key_bytes.len() as u32).to_be_bytes();

        let mut data = Vec::new();
        data.extend_from_slice(&key_len);
        data.extend_from_slice(key_bytes);
        data.extend_from_slice(value);

        RaftEntry::new(term, index, data)
    }

    #[tokio::test]
    async fn test_apply_entry() {
        let mut sm = MemoryStateMachine::new();

        let entry = make_kv_entry(1, 1, "key1", b"value1");
        let result = sm.apply(&entry).await.unwrap();

        assert_eq!(result, Bytes::from("applied:1"));
        assert_eq!(sm.last_applied(), 1);
        assert_eq!(sm.get("key1"), Some(&b"value1".to_vec()));
    }

    #[tokio::test]
    async fn test_apply_multiple_entries() {
        let mut sm = MemoryStateMachine::new();

        let entries = vec![
            make_kv_entry(1, 1, "key1", b"value1"),
            make_kv_entry(2, 1, "key2", b"value2"),
            make_kv_entry(3, 2, "key3", b"value3"),
        ];

        for entry in &entries {
            sm.apply(entry).await.unwrap();
        }

        assert_eq!(sm.last_applied(), 3);
        assert_eq!(sm.len(), 3);
        assert_eq!(sm.get("key1"), Some(&b"value1".to_vec()));
        assert_eq!(sm.get("key2"), Some(&b"value2".to_vec()));
        assert_eq!(sm.get("key3"), Some(&b"value3".to_vec()));
    }

    #[tokio::test]
    async fn test_snapshot_and_restore() {
        let mut sm = MemoryStateMachine::new();

        // Apply some entries
        let entries = vec![
            make_kv_entry(1, 1, "key1", b"value1"),
            make_kv_entry(2, 1, "key2", b"value2"),
        ];

        for entry in &entries {
            sm.apply(entry).await.unwrap();
        }

        // Create snapshot
        let snapshot = sm.snapshot(2, 1).await.unwrap();
        assert_eq!(snapshot.last_index, 2);
        assert_eq!(snapshot.last_term, 1);

        // Create new state machine and restore
        let mut sm2 = MemoryStateMachine::new();
        sm2.restore(&snapshot).await.unwrap();

        assert_eq!(sm2.last_applied(), 2);
        assert_eq!(sm2.len(), 2);
        assert_eq!(sm2.get("key1"), Some(&b"value1".to_vec()));
        assert_eq!(sm2.get("key2"), Some(&b"value2".to_vec()));
    }

    #[tokio::test]
    async fn test_idempotent_apply() {
        let mut sm = MemoryStateMachine::new();

        let entry = make_kv_entry(1, 1, "key1", b"value1");

        // Apply same entry twice
        sm.apply(&entry).await.unwrap();
        sm.apply(&entry).await.unwrap();

        // Should only have one entry (overwritten)
        assert_eq!(sm.last_applied(), 1);
        assert_eq!(sm.len(), 1);
    }

    #[tokio::test]
    async fn test_empty_state_machine() {
        let sm = MemoryStateMachine::new();

        assert_eq!(sm.last_applied(), 0);
        assert_eq!(sm.len(), 0);
        assert!(sm.is_empty());
    }
}
