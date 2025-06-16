//! Persistent storage backend for Raft.

use chronik_common::{Result, Error};
use raft::{
    prelude::*, Error as RaftError, RaftState, Storage, StorageError, GetEntriesContext,
};
use protobuf::Message;
use sled::{Db, Tree};
use std::sync::{Arc, RwLock};

/// Persistent Raft storage using Sled
pub struct SledRaftStorage {
    db: Arc<Db>,
    entries_tree: Arc<Tree>,
    state_tree: Arc<Tree>,
    snapshot_tree: Arc<Tree>,
    raft_state: Arc<RwLock<RaftState>>,
}

/// Keys for state storage
const HARD_STATE_KEY: &[u8] = b"hard_state";
const CONF_STATE_KEY: &[u8] = b"conf_state";
const LAST_INDEX_KEY: &[u8] = b"last_index";

impl SledRaftStorage {
    /// Create new storage instance
    pub fn new(path: &str) -> Result<Self> {
        let db = sled::open(path)
            .map_err(|e| Error::Storage(format!("Failed to open sled db: {}", e)))?;
        
        let entries_tree = db.open_tree("raft_entries")
            .map_err(|e| Error::Storage(format!("Failed to open entries tree: {}", e)))?;
        
        let state_tree = db.open_tree("raft_state")
            .map_err(|e| Error::Storage(format!("Failed to open state tree: {}", e)))?;
        
        let snapshot_tree = db.open_tree("raft_snapshot")
            .map_err(|e| Error::Storage(format!("Failed to open snapshot tree: {}", e)))?;
        
        // Load initial state
        let raft_state = Self::load_raft_state(&state_tree)?;
        
        Ok(Self {
            db: Arc::new(db),
            entries_tree: Arc::new(entries_tree),
            state_tree: Arc::new(state_tree),
            snapshot_tree: Arc::new(snapshot_tree),
            raft_state: Arc::new(RwLock::new(raft_state)),
        })
    }
    
    /// Load Raft state from storage
    fn load_raft_state(state_tree: &Tree) -> Result<RaftState> {
        let hard_state = if let Some(data) = state_tree.get(HARD_STATE_KEY)
            .map_err(|e| Error::Storage(format!("Failed to load hard state: {}", e)))? 
        {
            HardState::parse_from_bytes(&data)
                .map_err(|e| Error::Storage(format!("Failed to parse hard state: {}", e)))?
        } else {
            HardState::default()
        };
        
        let conf_state = if let Some(data) = state_tree.get(CONF_STATE_KEY)
            .map_err(|e| Error::Storage(format!("Failed to load conf state: {}", e)))? 
        {
            ConfState::parse_from_bytes(&data)
                .map_err(|e| Error::Storage(format!("Failed to parse conf state: {}", e)))?
        } else {
            ConfState::default()
        };
        
        Ok(RaftState {
            hard_state,
            conf_state,
        })
    }
    
    /// Save hard state
    pub fn save_hard_state(&self, hs: &HardState) -> Result<()> {
        let data = hs.write_to_bytes()
            .map_err(|e| Error::Storage(format!("Failed to serialize hard state: {}", e)))?;
        self.state_tree.insert(HARD_STATE_KEY, data)
            .map_err(|e| Error::Storage(format!("Failed to save hard state: {}", e)))?;
        
        self.state_tree.flush()
            .map_err(|e| Error::Storage(format!("Failed to flush state: {}", e)))?;
        
        let mut state = self.raft_state.write().unwrap();
        state.hard_state = hs.clone();
        
        Ok(())
    }
    
    /// Save conf state
    pub fn save_conf_state(&self, cs: &ConfState) -> Result<()> {
        let data = cs.write_to_bytes()
            .map_err(|e| Error::Storage(format!("Failed to serialize conf state: {}", e)))?;
        self.state_tree.insert(CONF_STATE_KEY, data)
            .map_err(|e| Error::Storage(format!("Failed to save conf state: {}", e)))?;
        
        self.state_tree.flush()
            .map_err(|e| Error::Storage(format!("Failed to flush state: {}", e)))?;
        
        let mut state = self.raft_state.write().unwrap();
        state.conf_state = cs.clone();
        
        Ok(())
    }
    
    /// Append entries to log
    pub fn append_entries(&self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        
        let mut batch = sled::Batch::default();
        
        for entry in entries {
            let key = entry.index.to_be_bytes();
            let data = entry.write_to_bytes()
                .map_err(|e| Error::Storage(format!("Failed to serialize entry: {}", e)))?;
            batch.insert(key.as_ref(), data);
        }
        
        // Update last index
        let last_index = entries.last().unwrap().index;
        batch.insert(LAST_INDEX_KEY, last_index.to_be_bytes().as_ref());
        
        self.entries_tree.apply_batch(batch)
            .map_err(|e| Error::Storage(format!("Failed to append entries: {}", e)))?;
        
        self.entries_tree.flush()
            .map_err(|e| Error::Storage(format!("Failed to flush entries: {}", e)))?;
        
        Ok(())
    }
    
    /// Get entries in range
    pub fn get_entries(&self, low: u64, high: u64, max_size: u64) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut size = 0u64;
        
        let start = low.to_be_bytes();
        let end = high.to_be_bytes();
        
        for result in self.entries_tree.range(start..end) {
            let (_, data) = result
                .map_err(|e| Error::Storage(format!("Failed to read entry: {}", e)))?;
            
            let entry = Entry::parse_from_bytes(&data)
                .map_err(|e| Error::Storage(format!("Failed to parse entry: {}", e)))?;
            size += data.len() as u64;
            entries.push(entry);
            
            if size >= max_size {
                break;
            }
        }
        
        Ok(entries)
    }
    
    /// Get first index
    pub fn first_index(&self) -> Result<u64> {
        if let Some((key, _)) = self.entries_tree.first()
            .map_err(|e| Error::Storage(format!("Failed to get first entry: {}", e)))? 
        {
            Ok(u64::from_be_bytes(key.as_ref().try_into().unwrap()))
        } else {
            Ok(1)
        }
    }
    
    /// Get last index
    pub fn last_index(&self) -> Result<u64> {
        if let Some(data) = self.entries_tree.get(LAST_INDEX_KEY)
            .map_err(|e| Error::Storage(format!("Failed to get last index: {}", e)))? 
        {
            Ok(u64::from_be_bytes(data.as_ref().try_into().unwrap()))
        } else {
            Ok(0)
        }
    }
    
    /// Truncate log entries
    pub fn truncate(&self, from: u64) -> Result<()> {
        let start = from.to_be_bytes();
        
        let mut batch = sled::Batch::default();
        
        for result in self.entries_tree.range(start..) {
            let (key, _) = result
                .map_err(|e| Error::Storage(format!("Failed to read entry: {}", e)))?;
            batch.remove(key);
        }
        
        // Update last index
        if from > 1 {
            let new_last = from - 1;
            batch.insert(LAST_INDEX_KEY, new_last.to_be_bytes().as_ref());
        } else {
            batch.remove(LAST_INDEX_KEY);
        }
        
        self.entries_tree.apply_batch(batch)
            .map_err(|e| Error::Storage(format!("Failed to truncate: {}", e)))?;
        
        self.entries_tree.flush()
            .map_err(|e| Error::Storage(format!("Failed to flush after truncate: {}", e)))?;
        
        Ok(())
    }
    
    /// Create and save a snapshot
    pub fn create_snapshot(&self, index: u64, term: u64, nodes: ConfState, data: Vec<u8>) -> Result<()> {
        let mut snapshot = Snapshot::new();
        snapshot.set_data(data.into());
        
        let mut metadata = SnapshotMetadata::new();
        metadata.set_conf_state(nodes);
        metadata.set_index(index);
        metadata.set_term(term);
        snapshot.set_metadata(metadata);
        
        let snapshot_data = snapshot.write_to_bytes()
            .map_err(|e| Error::Storage(format!("Failed to serialize snapshot: {}", e)))?;
        self.snapshot_tree.insert(b"current", snapshot_data)
            .map_err(|e| Error::Storage(format!("Failed to save snapshot: {}", e)))?;
        
        self.snapshot_tree.flush()
            .map_err(|e| Error::Storage(format!("Failed to flush snapshot: {}", e)))?;
        
        // Remove entries before snapshot
        self.compact(index)?;
        
        Ok(())
    }
    
    /// Get current snapshot
    pub fn snapshot(&self) -> Result<Snapshot> {
        if let Some(data) = self.snapshot_tree.get(b"current")
            .map_err(|e| Error::Storage(format!("Failed to get snapshot: {}", e)))? 
        {
            Ok(Snapshot::parse_from_bytes(&data)
                .map_err(|e| Error::Storage(format!("Failed to parse snapshot: {}", e)))?)
        } else {
            Ok(Snapshot::default())
        }
    }
    
    /// Compact log entries before index
    fn compact(&self, compact_index: u64) -> Result<()> {
        let end = compact_index.to_be_bytes();
        
        let mut batch = sled::Batch::default();
        
        for result in self.entries_tree.range(..end) {
            let (key, _) = result
                .map_err(|e| Error::Storage(format!("Failed to read entry: {}", e)))?;
            batch.remove(key);
        }
        
        self.entries_tree.apply_batch(batch)
            .map_err(|e| Error::Storage(format!("Failed to compact: {}", e)))?;
        
        Ok(())
    }
}

impl Storage for SledRaftStorage {
    fn initial_state(&self) -> raft::Result<RaftState> {
        Ok(self.raft_state.read().unwrap().clone())
    }
    
    fn entries(&self, low: u64, high: u64, max_size: impl Into<Option<u64>>, _context: GetEntriesContext) -> raft::Result<Vec<Entry>> {
        let max_size = max_size.into().unwrap_or(u64::MAX);
        self.get_entries(low, high, max_size)
            .map_err(|e| RaftError::Store(StorageError::Other(Box::new(e))))
    }
    
    fn term(&self, idx: u64) -> raft::Result<u64> {
        if idx == 0 {
            return Ok(0);
        }
        
        let key = idx.to_be_bytes();
        if let Some(data) = self.entries_tree.get(key)
            .map_err(|e| RaftError::Store(StorageError::Other(e.into())))? 
        {
            let entry = Entry::parse_from_bytes(&data)
                .map_err(|e| RaftError::Store(StorageError::Other(e.into())))?;
            Ok(entry.term)
        } else {
            Err(RaftError::Store(StorageError::Compacted))
        }
    }
    
    fn first_index(&self) -> raft::Result<u64> {
        self.first_index()
            .map_err(|e| RaftError::Store(StorageError::Other(Box::new(e))))
    }
    
    fn last_index(&self) -> raft::Result<u64> {
        self.last_index()
            .map_err(|e| RaftError::Store(StorageError::Other(Box::new(e))))
    }
    
    fn snapshot(&self, _request_index: u64, _to: u64) -> raft::Result<Snapshot> {
        self.snapshot()
            .map_err(|e| RaftError::Store(StorageError::Other(Box::new(e))))
    }
}