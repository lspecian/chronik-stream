//! TiKV-based Raft storage implementation.

use chronik_common::{Result, Error};
use raft::{
    prelude::*, Error as RaftError, RaftState, Storage, StorageError, GetEntriesContext,
};
use protobuf::Message;
use tikv_client::{RawClient, KvPair};
use std::sync::{Arc, RwLock};

/// Keys for state storage
const HARD_STATE_KEY: &[u8] = b"raft:hard_state";
const CONF_STATE_KEY: &[u8] = b"raft:conf_state";
const LAST_INDEX_KEY: &[u8] = b"raft:last_index";
const SNAPSHOT_KEY: &[u8] = b"raft:snapshot";

/// TiKV-based Raft storage
pub struct TiKVRaftStorage {
    client: Arc<RawClient>,
    raft_state: Arc<RwLock<RaftState>>,
    entries_cache: Arc<RwLock<Vec<Entry>>>,
}

impl TiKVRaftStorage {
    /// Create new TiKV Raft storage
    pub async fn new(pd_endpoints: Vec<String>) -> Result<Self> {
        let client = RawClient::new(pd_endpoints).await
            .map_err(|e| Error::Storage(format!("Failed to connect to TiKV: {}", e)))?;
        
        let client = Arc::new(client);
        
        // Load initial state
        let raft_state = Self::load_raft_state(&client).await?;
        
        Ok(Self {
            client,
            raft_state: Arc::new(RwLock::new(raft_state)),
            entries_cache: Arc::new(RwLock::new(Vec::new())),
        })
    }
    
    /// Load Raft state from storage
    async fn load_raft_state(client: &RawClient) -> Result<RaftState> {
        let hard_state = if let Some(data) = client.get(HARD_STATE_KEY.to_vec()).await
            .map_err(|e| Error::Storage(format!("Failed to load hard state: {}", e)))? 
        {
            HardState::parse_from_bytes(&data)
                .map_err(|e| Error::Storage(format!("Failed to parse hard state: {}", e)))?
        } else {
            HardState::default()
        };
        
        let conf_state = if let Some(data) = client.get(CONF_STATE_KEY.to_vec()).await
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
    pub async fn save_hard_state(&self, hs: &HardState) -> Result<()> {
        let data = hs.write_to_bytes()
            .map_err(|e| Error::Storage(format!("Failed to serialize hard state: {}", e)))?;
        self.client.put(HARD_STATE_KEY.to_vec(), data).await
            .map_err(|e| Error::Storage(format!("Failed to save hard state: {}", e)))?;
        
        let mut state = self.raft_state.write().unwrap();
        state.hard_state = hs.clone();
        
        Ok(())
    }
    
    /// Save conf state
    pub async fn save_conf_state(&self, cs: &ConfState) -> Result<()> {
        let data = cs.write_to_bytes()
            .map_err(|e| Error::Storage(format!("Failed to serialize conf state: {}", e)))?;
        self.client.put(CONF_STATE_KEY.to_vec(), data).await
            .map_err(|e| Error::Storage(format!("Failed to save conf state: {}", e)))?;
        
        let mut state = self.raft_state.write().unwrap();
        state.conf_state = cs.clone();
        
        Ok(())
    }
    
    /// Append entries to the log
    pub async fn append_entries(&self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        
        let mut batch = Vec::new();
        for entry in entries {
            let key = format!("raft:entry:{:020}", entry.index).into_bytes();
            let value = entry.write_to_bytes()
                .map_err(|e| Error::Storage(format!("Failed to serialize entry: {}", e)))?;
            batch.push((key, value));
        }
        
        // Batch put all entries
        self.client.batch_put(batch).await
            .map_err(|e| Error::Storage(format!("Failed to append entries: {}", e)))?;
        
        // Update last index
        let last_index = entries.last().unwrap().index;
        self.client.put(LAST_INDEX_KEY.to_vec(), last_index.to_be_bytes().to_vec()).await
            .map_err(|e| Error::Storage(format!("Failed to update last index: {}", e)))?;
        
        // Update cache
        let mut cache = self.entries_cache.write().unwrap();
        cache.extend_from_slice(entries);
        
        Ok(())
    }
    
    /// Get entries from storage
    async fn get_entries_impl(&self, low: u64, high: u64, max_size: u64) -> Result<Vec<Entry>> {
        if low >= high {
            return Ok(vec![]);
        }
        
        // Try cache first
        {
            let cache = self.entries_cache.read().unwrap();
            if !cache.is_empty() {
                let start = cache.binary_search_by(|e| e.index.cmp(&low)).unwrap_or_else(|x| x);
                let end = cache.binary_search_by(|e| e.index.cmp(&high)).unwrap_or_else(|x| x);
                
                if start < cache.len() && cache[start].index == low {
                    let cached_entries: Vec<Entry> = cache[start..end].to_vec();
                    if cached_entries.last().map(|e| e.index + 1) == Some(high) {
                        return Ok(cached_entries);
                    }
                }
            }
        }
        
        // Load from TiKV
        let start_key = format!("raft:entry:{:020}", low).into_bytes();
        let end_key = format!("raft:entry:{:020}", high).into_bytes();
        
        let pairs = self.client.scan(start_key..end_key, (high - low) as u32).await
            .map_err(|e| Error::Storage(format!("Failed to scan entries: {}", e)))?;
        
        let mut entries = Vec::new();
        let mut size = 0u64;
        
        for pair in pairs {
            let entry = Entry::parse_from_bytes(&pair.1)
                .map_err(|e| Error::Storage(format!("Failed to parse entry: {}", e)))?;
            
            size += pair.1.len() as u64;
            entries.push(entry);
            
            if size > max_size {
                break;
            }
        }
        
        Ok(entries)
    }
    
    /// Create a snapshot
    pub async fn create_snapshot(&self, index: u64, cs: ConfState, data: Vec<u8>) -> Result<()> {
        let snapshot = Snapshot {
            data: data.into(),
            metadata: Some(SnapshotMetadata {
                conf_state: Some(cs).into(),
                index,
                term: self.raft_state.read().unwrap().hard_state.term,
                ..Default::default()
            }).into(),
            ..Default::default()
        };
        
        let value = snapshot.write_to_bytes()
            .map_err(|e| Error::Storage(format!("Failed to serialize snapshot: {}", e)))?;
        
        self.client.put(SNAPSHOT_KEY.to_vec(), value).await
            .map_err(|e| Error::Storage(format!("Failed to save snapshot: {}", e)))?;
        
        // Delete log entries before snapshot
        let start_key = b"raft:entry:".to_vec();
        let end_key = format!("raft:entry:{:020}", index).into_bytes();
        
        let pairs = self.client.scan(start_key..end_key, 10000).await
            .map_err(|e| Error::Storage(format!("Failed to scan keys for deletion: {}", e)))?;
        
        let keys_to_delete: Vec<Vec<u8>> = pairs.into_iter().map(|pair| pair.0.into()).collect();
        
        if !keys_to_delete.is_empty() {
            self.client.batch_delete(keys_to_delete).await
                .map_err(|e| Error::Storage(format!("Failed to delete old entries: {}", e)))?;
        }
        
        Ok(())
    }
}

impl Storage for TiKVRaftStorage {
    fn initial_state(&self) -> raft::Result<RaftState> {
        Ok(self.raft_state.read().unwrap().clone())
    }
    
    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> raft::Result<Vec<Entry>> {
        let max_size = max_size.into().unwrap_or(u64::MAX);
        
        // Use tokio runtime to execute async operation
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| RaftError::Store(StorageError::Other("No tokio runtime".into())))?;
        
        runtime.block_on(async move {
            self.get_entries_impl(low, high, max_size).await
                .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))
        })
    }
    
    fn term(&self, idx: u64) -> raft::Result<u64> {
        let entries = self.entries(idx, idx + 1, None, GetEntriesContext::empty(false))?;
        if entries.is_empty() {
            Err(RaftError::Store(StorageError::Unavailable))
        } else {
            Ok(entries[0].term)
        }
    }
    
    fn first_index(&self) -> raft::Result<u64> {
        // Check if we have a snapshot
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| RaftError::Store(StorageError::Other("No tokio runtime".into())))?;
        
        let client = Arc::clone(&self.client);
        
        let snapshot_index = runtime.block_on(async move {
            if let Some(data) = client.get(SNAPSHOT_KEY.to_vec()).await
                .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))? {
                let snapshot = Snapshot::parse_from_bytes(&data)
                    .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))?;
                if let Some(metadata) = snapshot.metadata.as_ref() {
                    Ok::<u64, RaftError>(metadata.index + 1)
                } else {
                    Ok(1)
                }
            } else {
                Ok(1)
            }
        })?;
        
        Ok(snapshot_index)
    }
    
    fn last_index(&self) -> raft::Result<u64> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| RaftError::Store(StorageError::Other("No tokio runtime".into())))?;
        
        let client = Arc::clone(&self.client);
        
        runtime.block_on(async move {
            if let Some(data) = client.get(LAST_INDEX_KEY.to_vec()).await
                .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))? {
                let index = u64::from_be_bytes(data.try_into().unwrap_or_default());
                Ok(index)
            } else {
                // Check snapshot
                if let Some(data) = client.get(SNAPSHOT_KEY.to_vec()).await
                    .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))? {
                    let snapshot = Snapshot::parse_from_bytes(&data)
                        .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))?;
                    if let Some(metadata) = snapshot.metadata.as_ref() {
                        Ok::<u64, RaftError>(metadata.index)
                    } else {
                        Ok(0)
                    }
                } else {
                    Ok(0)
                }
            }
        })
    }
    
    fn snapshot(&self, _request_index: u64, _to: u64) -> raft::Result<Snapshot> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| RaftError::Store(StorageError::Other("No tokio runtime".into())))?;
        
        let client = Arc::clone(&self.client);
        
        runtime.block_on(async move {
            if let Some(data) = client.get(SNAPSHOT_KEY.to_vec()).await
                .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))? {
                let snapshot = Snapshot::parse_from_bytes(&data)
                    .map_err(|e| RaftError::Store(StorageError::Other(e.to_string().into())))?;
                Ok(snapshot)
            } else {
                Err(RaftError::Store(StorageError::SnapshotTemporarilyUnavailable))
            }
        })
    }
}