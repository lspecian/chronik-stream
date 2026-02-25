//! Simple search engine implementation.

use chronik_common::Result;
use chronik_storage::Record;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Search query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Query string
    pub query: String,
    /// Topic filter
    pub topic: Option<String>,
    /// Partition filter
    pub partition: Option<i32>,
    /// Start time filter (epoch millis)
    pub start_time: Option<i64>,
    /// End time filter (epoch millis)
    pub end_time: Option<i64>,
    /// Maximum results
    pub limit: usize,
    /// Offset for pagination
    pub offset: usize,
}

/// Search result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Total number of hits
    pub total_hits: usize,
    /// Search results
    pub hits: Vec<SearchHit>,
    /// Query execution time in milliseconds
    pub took_ms: u64,
}

/// Individual search hit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// Topic name
    pub topic: String,
    /// Partition number
    pub partition: i32,
    /// Message offset
    pub offset: i64,
    /// Timestamp (epoch millis)
    pub timestamp: i64,
    /// Message key (if present)
    pub key: Option<String>,
    /// Message value
    pub value: String,
    /// Relevance score
    pub score: f32,
}

/// Message entry for indexing
#[derive(Debug, Clone)]
struct MessageEntry {
    topic: String,
    partition: i32,
    offset: i64,
    timestamp: i64,
    key: Option<String>,
    value: String,
}

/// Simple in-memory search engine
pub struct SearchEngine {
    /// Messages indexed by topic-partition
    messages: Arc<RwLock<HashMap<(String, i32), Vec<MessageEntry>>>>,
    /// Index path (for future disk-based implementation)
    _index_path: Arc<Path>,
}

impl SearchEngine {
    /// Create a new search engine
    pub fn new(index_path: &Path) -> Result<Self> {
        Ok(Self {
            messages: Arc::new(RwLock::new(HashMap::new())),
            _index_path: Arc::from(index_path),
        })
    }
    
    /// Index a message
    pub async fn index_message(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        timestamp: i64,
        key: Option<&[u8]>,
        value: &[u8],
    ) -> Result<()> {
        let key_str = key.and_then(|k| std::str::from_utf8(k).ok()).map(|s| s.to_string());
        let value_str = std::str::from_utf8(value)
            .unwrap_or("(binary data)")
            .to_string();
        
        let entry = MessageEntry {
            topic: topic.to_string(),
            partition,
            offset,
            timestamp,
            key: key_str,
            value: value_str,
        };
        
        let mut messages = self.messages.write().await;
        messages
            .entry((topic.to_string(), partition))
            .or_insert_with(Vec::new)
            .push(entry);
        
        Ok(())
    }
    
    /// Commit pending changes (no-op for in-memory implementation)
    pub async fn commit(&self) -> Result<()> {
        Ok(())
    }
    
    /// Search messages
    pub async fn search(&self, query: SearchQuery) -> Result<SearchResult> {
        let start = std::time::Instant::now();
        
        let messages = self.messages.read().await;
        let mut all_hits = Vec::new();
        
        // Search across all messages
        for ((topic, partition), entries) in messages.iter() {
            // Apply topic filter
            if let Some(ref filter_topic) = query.topic {
                if topic != filter_topic {
                    continue;
                }
            }
            
            // Apply partition filter
            if let Some(filter_partition) = query.partition {
                if *partition != filter_partition {
                    continue;
                }
            }
            
            // Search entries
            for entry in entries {
                // Apply time filters
                if let Some(start_time) = query.start_time {
                    if entry.timestamp < start_time {
                        continue;
                    }
                }
                
                if let Some(end_time) = query.end_time {
                    if entry.timestamp > end_time {
                        continue;
                    }
                }
                
                // Text search
                let score = if !query.query.is_empty() {
                    let query_lower = query.query.to_lowercase();
                    let value_lower = entry.value.to_lowercase();
                    
                    if value_lower.contains(&query_lower) {
                        // Simple scoring based on match position
                        let pos = value_lower.find(&query_lower).unwrap_or(0);
                        1.0 - (pos as f32 / value_lower.len() as f32)
                    } else {
                        continue;
                    }
                } else {
                    1.0 // Full score if no text query
                };
                
                all_hits.push(SearchHit {
                    topic: entry.topic.clone(),
                    partition: entry.partition,
                    offset: entry.offset,
                    timestamp: entry.timestamp,
                    key: entry.key.clone(),
                    value: entry.value.clone(),
                    score,
                });
            }
        }
        
        // Sort by score (descending) and timestamp (descending)
        all_hits.sort_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.timestamp.cmp(&a.timestamp))
        });
        
        // Apply pagination
        let total_hits = all_hits.len();
        let hits: Vec<SearchHit> = all_hits
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .collect();
        
        Ok(SearchResult {
            total_hits,
            hits,
            took_ms: start.elapsed().as_millis() as u64,
        })
    }
    
    /// Index a batch of messages
    pub async fn index_batch(&self, records: Vec<Record>, topic: &str, partition: i32) -> Result<()> {
        for record in records {
            self.index_message(
                topic,
                partition,
                record.offset,
                record.timestamp,
                record.key.as_deref(),
                &record.value,
            ).await?;
        }
        Ok(())
    }
}