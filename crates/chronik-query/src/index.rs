//! Index management for search functionality.

use chronik_common::{Result, Error};
use chronik_storage::SegmentReader;
use crate::search::SearchEngine;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

/// Index schema
#[derive(Debug, Clone)]
pub struct IndexSchema {
    /// Topics to index
    pub topics: Vec<String>,
    /// Batch size for indexing
    pub batch_size: usize,
    /// Commit interval
    pub commit_interval: Duration,
}

impl Default for IndexSchema {
    fn default() -> Self {
        Self {
            topics: vec![],
            batch_size: 1000,
            commit_interval: Duration::from_secs(10),
        }
    }
}

/// Indexer that reads from storage and indexes to search engine
pub struct Indexer {
    search_engine: Arc<SearchEngine>,
    segment_reader: Arc<SegmentReader>,
    schema: IndexSchema,
    state: Arc<RwLock<IndexerState>>,
}

/// Indexer state
#[derive(Debug)]
struct IndexerState {
    /// Last indexed offsets per topic-partition
    offsets: std::collections::HashMap<(String, i32), i64>,
    /// Running flag
    running: bool,
}

impl Indexer {
    /// Create a new indexer
    pub fn new(
        search_engine: Arc<SearchEngine>,
        segment_reader: Arc<SegmentReader>,
        schema: IndexSchema,
    ) -> Self {
        Self {
            search_engine,
            segment_reader,
            schema,
            state: Arc::new(RwLock::new(IndexerState {
                offsets: std::collections::HashMap::new(),
                running: false,
            })),
        }
    }
    
    /// Start the indexer
    pub async fn start(self: Arc<Self>) -> Result<()> {
        {
            let mut state = self.state.write().await;
            if state.running {
                return Err(Error::InvalidInput("Indexer already running".into()));
            }
            state.running = true;
        }
        
        // Spawn indexing task
        let indexer = self.clone();
        tokio::spawn(async move {
            if let Err(e) = indexer.run_indexing_loop().await {
                tracing::error!("Indexing loop failed: {}", e);
            }
        });
        
        // Spawn commit task
        let indexer = self.clone();
        tokio::spawn(async move {
            if let Err(e) = indexer.run_commit_loop().await {
                tracing::error!("Commit loop failed: {}", e);
            }
        });
        
        Ok(())
    }
    
    /// Stop the indexer
    pub async fn stop(&self) -> Result<()> {
        let mut state = self.state.write().await;
        state.running = false;
        Ok(())
    }
    
    /// Run the indexing loop
    async fn run_indexing_loop(&self) -> Result<()> {
        let mut interval = interval(Duration::from_millis(100));
        
        loop {
            interval.tick().await;
            
            // Check if still running
            {
                let state = self.state.read().await;
                if !state.running {
                    break;
                }
            }
            
            // Index new records
            if let Err(e) = self.index_new_records().await {
                tracing::error!("Failed to index records: {}", e);
            }
        }
        
        Ok(())
    }
    
    /// Run the commit loop
    async fn run_commit_loop(&self) -> Result<()> {
        let mut interval = interval(self.schema.commit_interval);
        
        loop {
            interval.tick().await;
            
            // Check if still running
            {
                let state = self.state.read().await;
                if !state.running {
                    break;
                }
            }
            
            // Commit changes
            if let Err(e) = self.search_engine.commit().await {
                tracing::error!("Failed to commit search index: {}", e);
            }
        }
        
        Ok(())
    }
    
    /// Index new records
    async fn index_new_records(&self) -> Result<()> {
        let mut state = self.state.write().await;
        
        for topic in &self.schema.topics {
            // For simplicity, assume single partition
            let partition = 0;
            let key = (topic.clone(), partition);
            
            // Get last indexed offset
            let start_offset = state.offsets.get(&key).copied().unwrap_or(0);
            
            // Fetch new records
            let fetch_result = self.segment_reader.fetch(
                topic,
                partition,
                start_offset,
                self.schema.batch_size as i32,
            ).await?;
            
            // Index records
            for record in &fetch_result.records {
                self.search_engine.index_message(
                    topic,
                    partition,
                    record.offset,
                    record.timestamp,
                    record.key.as_deref(),
                    &record.value,
                ).await?;
            }
            
            // Update offset
            if let Some(last_record) = fetch_result.records.last() {
                state.offsets.insert(key, last_record.offset + 1);
            }
        }
        
        Ok(())
    }
}