//! Raft log storage implementations
//!
//! This module provides persistent storage for Raft log entries.
//! STUB: To be fully implemented in Phase 2.

use crate::{Result, RaftError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Topic name for Raft internal metadata
pub const RAFT_TOPIC: &str = "__raft_internal";

/// Partition number for Raft internal metadata
pub const RAFT_PARTITION: i32 = 0;

/// A single log entry in the Raft log
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RaftEntry {
    /// Log entry index (monotonically increasing from 1)
    pub index: u64,

    /// Raft term when this entry was created
    pub term: u64,

    /// Entry payload (serialized command)
    pub data: Vec<u8>,
}

impl RaftEntry {
    /// Create a new RaftEntry
    pub fn new(term: u64, index: u64, data: Vec<u8>) -> Self {
        Self { index, term, data }
    }

    /// Serialize entry to bytes using bincode
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|e| RaftError::SerializationError(e.to_string()))
    }

    /// Deserialize entry from bytes using bincode
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data).map_err(|e| RaftError::SerializationError(e.to_string()))
    }
}

/// Trait for Raft log storage backends
#[async_trait]
pub trait RaftLogStorage: Send + Sync {
    /// Append entries to the log
    async fn append(&self, entries: Vec<RaftEntry>) -> Result<()>;

    /// Get entry at specific index
    async fn get(&self, index: u64) -> Result<Option<RaftEntry>>;

    /// Get entries in range [start, end)
    async fn range(&self, start: u64, end: u64) -> Result<Vec<RaftEntry>>;

    /// Get first log index
    async fn first_index(&self) -> Result<u64>;

    /// Get last log index
    async fn last_index(&self) -> Result<u64>;

    /// Truncate log after index (delete entries > index)
    async fn truncate_after(&self, index: u64) -> Result<()>;
}

/// In-memory log storage for testing
pub struct MemoryLogStorage {
    entries: Arc<RwLock<BTreeMap<u64, RaftEntry>>>,
}

impl MemoryLogStorage {
    /// Create a new in-memory log storage
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
}

impl Default for MemoryLogStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RaftLogStorage for MemoryLogStorage {
    async fn append(&self, entries: Vec<RaftEntry>) -> Result<()> {
        let mut log = self.entries.write().unwrap();
        for entry in entries {
            log.insert(entry.index, entry);
        }
        Ok(())
    }

    async fn get(&self, index: u64) -> Result<Option<RaftEntry>> {
        let log = self.entries.read().unwrap();
        Ok(log.get(&index).cloned())
    }

    async fn range(&self, start: u64, end: u64) -> Result<Vec<RaftEntry>> {
        if start > end {
            return Err(RaftError::InvalidRange { start, end });
        }
        let log = self.entries.read().unwrap();
        Ok(log
            .range(start..end)
            .map(|(_, entry)| entry.clone())
            .collect())
    }

    async fn first_index(&self) -> Result<u64> {
        let log = self.entries.read().unwrap();
        Ok(log.keys().next().copied().unwrap_or(0))
    }

    async fn last_index(&self) -> Result<u64> {
        let log = self.entries.read().unwrap();
        Ok(log.keys().next_back().copied().unwrap_or(0))
    }

    async fn truncate_after(&self, index: u64) -> Result<()> {
        let mut log = self.entries.write().unwrap();
        log.retain(|&k, _| k <= index);
        Ok(())
    }
}
