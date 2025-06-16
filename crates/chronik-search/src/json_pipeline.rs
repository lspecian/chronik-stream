//! JSON document processing pipeline that bridges Kafka messages to the real-time indexer.

use crate::realtime_indexer::{JsonDocument, RealtimeIndexer, RealtimeIndexerConfig};
use chronik_common::{Result, Error};
use chronik_storage::RecordBatch;
use serde_json::{Value as JsonValue, Map as JsonMap};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, instrument};

/// Configuration for JSON pipeline
#[derive(Debug, Clone)]
pub struct JsonPipelineConfig {
    /// Channel buffer size
    pub channel_buffer_size: usize,
    /// Whether to parse message keys as JSON
    pub parse_keys_as_json: bool,
    /// Whether to include Kafka metadata in indexed documents
    pub include_kafka_metadata: bool,
    /// Field name for storing raw Kafka key (if not JSON)
    pub key_field_name: String,
    /// Field name for storing Kafka headers
    pub headers_field_name: String,
}

impl Default for JsonPipelineConfig {
    fn default() -> Self {
        Self {
            channel_buffer_size: 10000,
            parse_keys_as_json: true,
            include_kafka_metadata: true,
            key_field_name: "_kafka_key".to_string(),
            headers_field_name: "_kafka_headers".to_string(),
        }
    }
}

/// JSON processing pipeline that converts Kafka messages to indexed documents
pub struct JsonPipeline {
    config: JsonPipelineConfig,
    indexer: Arc<RealtimeIndexer>,
    sender: mpsc::Sender<JsonDocument>,
    stats: Arc<RwLock<PipelineStats>>,
}

/// Pipeline statistics
#[derive(Debug, Default)]
pub struct PipelineStats {
    pub messages_processed: u64,
    pub parse_errors: u64,
    pub indexing_errors: u64,
    pub bytes_processed: u64,
}

impl JsonPipeline {
    /// Create a new JSON pipeline
    pub async fn new(
        pipeline_config: JsonPipelineConfig,
        indexer_config: RealtimeIndexerConfig,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::channel(pipeline_config.channel_buffer_size);
        
        let indexer = Arc::new(RealtimeIndexer::new(indexer_config)?);
        
        // Start the indexer with the receiver
        let indexer_clone = Arc::clone(&indexer);
        tokio::spawn(async move {
            if let Err(e) = indexer_clone.start(receiver).await {
                error!("Failed to start indexer: {}", e);
            }
        });
        
        Ok(Self {
            config: pipeline_config,
            indexer,
            sender,
            stats: Arc::new(RwLock::new(PipelineStats::default())),
        })
    }
    
    /// Process a batch of Kafka records
    #[instrument(skip(self, batch))]
    pub async fn process_batch(
        &self,
        topic: &str,
        partition: i32,
        batch: &RecordBatch,
    ) -> Result<()> {
        let mut stats = self.stats.write().await;
        
        for record in &batch.records {
            // Parse the value as JSON
            let content = match serde_json::from_slice::<JsonValue>(&record.value) {
                Ok(json) => json,
                Err(e) => {
                    debug!("Failed to parse message as JSON: {}", e);
                    stats.parse_errors += 1;
                    
                    // Try to create a JSON object with the raw value
                    let mut obj = JsonMap::new();
                    obj.insert(
                        "raw_value".to_string(),
                        JsonValue::String(String::from_utf8_lossy(&record.value).to_string())
                    );
                    JsonValue::Object(obj)
                }
            };
            
            // Build the document
            let mut doc_content = content;
            
            // Add Kafka metadata if configured
            if self.config.include_kafka_metadata {
                if let JsonValue::Object(ref mut obj) = doc_content {
                    // Add key
                    if let Some(key) = &record.key {
                        if self.config.parse_keys_as_json {
                            if let Ok(key_json) = serde_json::from_slice::<JsonValue>(key) {
                                obj.insert("_key".to_string(), key_json);
                            } else {
                                obj.insert(
                                    self.config.key_field_name.clone(),
                                    JsonValue::String(String::from_utf8_lossy(key).to_string())
                                );
                            }
                        } else {
                            obj.insert(
                                self.config.key_field_name.clone(),
                                JsonValue::String(String::from_utf8_lossy(key).to_string())
                            );
                        }
                    }
                    
                    // Add headers
                    if !record.headers.is_empty() {
                        let headers_json: JsonMap<String, JsonValue> = record.headers
                            .iter()
                            .map(|(k, v)| {
                                (k.clone(), JsonValue::String(String::from_utf8_lossy(v).to_string()))
                            })
                            .collect();
                        obj.insert(
                            self.config.headers_field_name.clone(),
                            JsonValue::Object(headers_json)
                        );
                    }
                }
            }
            
            // Create document ID
            let doc_id = format!("{}-{}-{}", topic, partition, record.offset);
            
            let document = JsonDocument {
                id: doc_id,
                topic: topic.to_string(),
                partition,
                offset: record.offset,
                timestamp: record.timestamp,
                content: doc_content,
                metadata: None,
            };
            
            // Send to indexer
            if let Err(e) = self.sender.send(document).await {
                error!("Failed to send document to indexer: {}", e);
                stats.indexing_errors += 1;
            } else {
                stats.messages_processed += 1;
                stats.bytes_processed += record.value.len() as u64;
                if let Some(key) = &record.key {
                    stats.bytes_processed += key.len() as u64;
                }
            }
        }
        
        Ok(())
    }
    
    /// Get pipeline statistics
    pub async fn stats(&self) -> PipelineStats {
        self.stats.read().await.clone()
    }
    
    /// Get indexer metrics
    pub fn indexer_metrics(&self) -> crate::realtime_indexer::IndexingMetricsSnapshot {
        self.indexer.metrics()
    }
    
    /// Shutdown the pipeline
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down JSON pipeline");
        
        // Close the channel
        drop(self.sender.clone());
        
        // Stop the indexer
        self.indexer.stop().await?;
        
        Ok(())
    }
}

/// Builder for creating a JSON pipeline with custom configuration
pub struct JsonPipelineBuilder {
    pipeline_config: JsonPipelineConfig,
    indexer_config: RealtimeIndexerConfig,
}

impl JsonPipelineBuilder {
    /// Create a new builder with default configuration
    pub fn new() -> Self {
        Self {
            pipeline_config: JsonPipelineConfig::default(),
            indexer_config: RealtimeIndexerConfig::default(),
        }
    }
    
    /// Set the channel buffer size
    pub fn channel_buffer_size(mut self, size: usize) -> Self {
        self.pipeline_config.channel_buffer_size = size;
        self
    }
    
    /// Set whether to parse keys as JSON
    pub fn parse_keys_as_json(mut self, parse: bool) -> Self {
        self.pipeline_config.parse_keys_as_json = parse;
        self
    }
    
    /// Set the indexing memory budget
    pub fn indexing_memory_budget(mut self, bytes: usize) -> Self {
        self.indexer_config.indexing_memory_budget = bytes;
        self
    }
    
    /// Set the batch size
    pub fn batch_size(mut self, size: usize) -> Self {
        self.indexer_config.batch_size = size;
        self
    }
    
    /// Set the number of indexing threads
    pub fn num_indexing_threads(mut self, threads: usize) -> Self {
        self.indexer_config.num_indexing_threads = threads;
        self
    }
    
    /// Build the pipeline
    pub async fn build(self) -> Result<JsonPipeline> {
        JsonPipeline::new(self.pipeline_config, self.indexer_config).await
    }
}

impl Default for JsonPipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for PipelineStats {
    fn clone(&self) -> Self {
        Self {
            messages_processed: self.messages_processed,
            parse_errors: self.parse_errors,
            indexing_errors: self.indexing_errors,
            bytes_processed: self.bytes_processed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_storage::Record;
    use tempfile::TempDir;
    use std::collections::HashMap;
    
    #[tokio::test]
    async fn test_json_pipeline_basic() {
        let temp_dir = TempDir::new().unwrap();
        
        let pipeline = JsonPipelineBuilder::new()
            .channel_buffer_size(100)
            .batch_size(5)
            .build()
            .await
            .unwrap();
        
        // Create test batch with JSON messages
        let batch = RecordBatch {
            records: vec![
                Record {
                    offset: 1,
                    timestamp: 1000,
                    key: Some(b"key1".to_vec()),
                    value: r#"{"name": "test1", "value": 42}"#.as_bytes().to_vec(),
                    headers: HashMap::new(),
                },
                Record {
                    offset: 2,
                    timestamp: 2000,
                    key: Some(b"key2".to_vec()),
                    value: r#"{"name": "test2", "value": 84, "nested": {"field": "value"}}"#.as_bytes().to_vec(),
                    headers: [("type".to_string(), b"test".to_vec())].into_iter().collect(),
                },
            ],
        };
        
        pipeline.process_batch("test-topic", 0, &batch).await.unwrap();
        
        // Wait for processing
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        let stats = pipeline.stats().await;
        assert_eq!(stats.messages_processed, 2);
        assert_eq!(stats.parse_errors, 0);
        
        let indexer_metrics = pipeline.indexer_metrics();
        assert!(indexer_metrics.documents_indexed > 0);
        
        pipeline.shutdown().await.unwrap();
    }
    
    #[tokio::test]
    async fn test_json_pipeline_with_non_json() {
        let temp_dir = TempDir::new().unwrap();
        
        let pipeline = JsonPipelineBuilder::new()
            .build()
            .await
            .unwrap();
        
        // Create batch with non-JSON message
        let batch = RecordBatch {
            records: vec![
                Record {
                    offset: 1,
                    timestamp: 1000,
                    key: None,
                    value: b"This is not JSON".to_vec(),
                    headers: HashMap::new(),
                },
            ],
        };
        
        pipeline.process_batch("test-topic", 0, &batch).await.unwrap();
        
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        let stats = pipeline.stats().await;
        assert_eq!(stats.messages_processed, 1);
        assert_eq!(stats.parse_errors, 1);
        
        pipeline.shutdown().await.unwrap();
    }
    
    #[tokio::test]
    async fn test_json_pipeline_with_key_parsing() {
        let temp_dir = TempDir::new().unwrap();
        
        let pipeline = JsonPipelineBuilder::new()
            .parse_keys_as_json(true)
            .build()
            .await
            .unwrap();
        
        // Create batch with JSON key
        let batch = RecordBatch {
            records: vec![
                Record {
                    offset: 1,
                    timestamp: 1000,
                    key: Some(r#"{"id": "user123"}"#.as_bytes().to_vec()),
                    value: r#"{"action": "login"}"#.as_bytes().to_vec(),
                    headers: HashMap::new(),
                },
            ],
        };
        
        pipeline.process_batch("events", 0, &batch).await.unwrap();
        
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        let stats = pipeline.stats().await;
        assert_eq!(stats.messages_processed, 1);
        assert_eq!(stats.parse_errors, 0);
        
        pipeline.shutdown().await.unwrap();
    }
}