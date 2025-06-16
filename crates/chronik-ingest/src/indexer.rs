//! Real-time indexing pipeline for processing records.

use chronik_common::{Result, Error};
use chronik_storage::{Record, RecordBatch, SegmentWriter};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tokio::time::interval;

/// Indexing configuration
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Batch size for indexing
    pub batch_size: usize,
    /// Batch timeout
    pub batch_timeout: Duration,
    /// Max segment size in bytes
    pub max_segment_size: u64,
    /// Compression codec
    pub compression_codec: String,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            batch_size: 10000,
            batch_timeout: Duration::from_secs(1),
            max_segment_size: 1024 * 1024 * 1024, // 1GB
            compression_codec: "snappy".to_string(),
        }
    }
}

/// Record to be indexed
#[derive(Debug, Clone)]
pub struct IndexRecord {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub timestamp: i64,
    pub key: Option<Vec<u8>>,
    pub value: Vec<u8>,
    pub headers: HashMap<String, Vec<u8>>,
}

/// Indexing statistics
#[derive(Debug, Default, Clone)]
pub struct IndexerStats {
    pub records_indexed: u64,
    pub bytes_indexed: u64,
    pub segments_created: u64,
    pub indexing_errors: u64,
}

/// Real-time indexer
pub struct Indexer {
    config: IndexerConfig,
    stats: Arc<RwLock<IndexerStats>>,
    running: Arc<RwLock<bool>>,
}

impl Indexer {
    /// Create a new indexer
    pub fn new(config: IndexerConfig) -> Self {
        Self {
            config,
            stats: Arc::new(RwLock::new(IndexerStats::default())),
            running: Arc::new(RwLock::new(false)),
        }
    }
    
    /// Start the indexing pipeline
    pub async fn start(
        &self,
        mut receiver: mpsc::Receiver<IndexRecord>,
        segment_writer: Arc<SegmentWriter>,
    ) -> Result<JoinHandle<Result<()>>> {
        {
            let mut running = self.running.write().await;
            if *running {
                return Err(Error::Internal("Indexer already running".into()));
            }
            *running = true;
        }
        
        let config = self.config.clone();
        let stats = self.stats.clone();
        let running = self.running.clone();
        
        let handle = tokio::spawn(async move {
            let mut batch: HashMap<(String, i32), Vec<IndexRecord>> = HashMap::new();
            let mut batch_timer = interval(config.batch_timeout);
            
            loop {
                tokio::select! {
                    Some(record) = receiver.recv() => {
                        let key = (record.topic.clone(), record.partition);
                        batch.entry(key).or_insert_with(Vec::new).push(record);
                        
                        // Check if we should flush
                        let should_flush = batch.values()
                            .any(|records| records.len() >= config.batch_size);
                        
                        if should_flush {
                            flush_batch(&mut batch, &segment_writer, &stats, &config).await?;
                        }
                    }
                    _ = batch_timer.tick() => {
                        if !batch.is_empty() {
                            flush_batch(&mut batch, &segment_writer, &stats, &config).await?;
                        }
                    }
                    else => {
                        // Channel closed
                        if !batch.is_empty() {
                            flush_batch(&mut batch, &segment_writer, &stats, &config).await?;
                        }
                        break;
                    }
                }
                
                if !*running.read().await {
                    break;
                }
            }
            
            Ok(())
        });
        
        Ok(handle)
    }
    
    /// Stop the indexer
    pub async fn stop(&self) -> Result<()> {
        let mut running = self.running.write().await;
        *running = false;
        Ok(())
    }
    
    /// Get indexing statistics
    pub async fn stats(&self) -> IndexerStats {
        self.stats.read().await.clone()
    }
}

/// Flush a batch of records to storage
async fn flush_batch(
    batch: &mut HashMap<(String, i32), Vec<IndexRecord>>,
    segment_writer: &Arc<SegmentWriter>,
    stats: &Arc<RwLock<IndexerStats>>,
    _config: &IndexerConfig,
) -> Result<()> {
    for ((topic, partition), records) in batch.drain() {
        if records.is_empty() {
            continue;
        }
        
        // Convert to storage records
        let storage_records: Vec<Record> = records
            .iter()
            .map(|r| Record {
                offset: r.offset,
                timestamp: r.timestamp,
                key: r.key.clone(),
                value: r.value.clone(),
                headers: r.headers.clone(),
            })
            .collect();
        
        // Create record batch
        let batch = RecordBatch {
            records: storage_records,
        };
        
        // Calculate batch size
        let batch_size: u64 = records
            .iter()
            .map(|r| {
                let key_size = r.key.as_ref().map(|k| k.len()).unwrap_or(0);
                let value_size = r.value.len();
                let headers_size: usize = r.headers.values().map(|v| v.len()).sum();
                (key_size + value_size + headers_size + 24) as u64 // 24 bytes for offset/timestamp
            })
            .sum();
        
        // Write to segment
        match segment_writer.write_batch(&topic, partition, batch).await {
            Ok(_) => {
                let mut stats = stats.write().await;
                stats.records_indexed += records.len() as u64;
                stats.bytes_indexed += batch_size;
            }
            Err(e) => {
                tracing::error!("Failed to write batch for {}:{}: {}", topic, partition, e);
                let mut stats = stats.write().await;
                stats.indexing_errors += 1;
            }
        }
    }
    
    Ok(())
}

/// Create a timestamp from SystemTime
pub fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[tokio::test]
    async fn test_indexer_basic() {
        let temp_dir = TempDir::new().unwrap();
        let config = IndexerConfig::default();
        
        // Create mock segment writer
        let segment_writer = Arc::new(
            SegmentWriter::new(
                chronik_storage::SegmentWriterConfig {
                    data_dir: temp_dir.path().to_path_buf(),
                    compression_codec: "snappy".to_string(),
                    max_segment_size: 1024 * 1024,
                }
            ).await.unwrap()
        );
        
        let indexer = Indexer::new(config);
        let (sender, receiver) = mpsc::channel(1000);
        
        let handle = indexer.start(receiver, segment_writer).await.unwrap();
        
        // Send some records
        for i in 0..10 {
            let record = IndexRecord {
                topic: "test".to_string(),
                partition: 0,
                offset: i,
                timestamp: current_timestamp(),
                key: Some(format!("key-{}", i).into_bytes()),
                value: format!("value-{}", i).into_bytes(),
                headers: HashMap::new(),
            };
            sender.send(record).await.unwrap();
        }
        
        // Wait a bit for processing
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Check stats
        let stats = indexer.stats().await;
        assert_eq!(stats.records_indexed, 10);
        assert!(stats.bytes_indexed > 0);
        
        // Stop indexer
        indexer.stop().await.unwrap();
        drop(sender);
        handle.await.unwrap().unwrap();
    }
}