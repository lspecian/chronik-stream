//! Integration between the REST API and TantivyIndexer for Kafka message indexing.

use crate::{
    api::{SearchApi, IndexMapping, FieldMapping},
    indexer::{TantivyIndexer, IndexerConfig, SearchResult},
};
use chronik_common::{Result, Error};
use chronik_storage::RecordBatch;
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, info, error};

/// Integration adapter that bridges TantivyIndexer with the REST API
pub struct SearchIntegration {
    api: Arc<SearchApi>,
    indexer: Option<Arc<TantivyIndexer>>,
}

impl SearchIntegration {
    /// Create a new search integration
    pub async fn new(api: Arc<SearchApi>) -> Result<Self> {
        Ok(Self {
            api,
            indexer: None,
        })
    }

    /// Initialize the Kafka message indexer
    pub async fn init_kafka_indexer(&mut self, config: IndexerConfig) -> Result<()> {
        // Create the TantivyIndexer
        let indexer = Arc::new(TantivyIndexer::new(config)?);
        
        // Start the commit task
        let indexer_clone = indexer.clone();
        indexer_clone.start_commit_task();
        
        self.indexer = Some(indexer);
        
        // Create Kafka index in the API
        self.create_kafka_indices().await?;
        
        info!("Initialized Kafka message indexer");
        Ok(())
    }

    /// Create indices for Kafka messages
    async fn create_kafka_indices(&self) -> Result<()> {
        // Create mapping for Kafka messages
        let mut properties = HashMap::new();
        
        // ID field
        properties.insert("_id".to_string(), FieldMapping {
            field_type: "keyword".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        // Topic field
        properties.insert("topic".to_string(), FieldMapping {
            field_type: "keyword".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        // Partition field
        properties.insert("partition".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        // Offset field
        properties.insert("offset".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        // Timestamp field
        properties.insert("timestamp".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        // Key field (searchable text)
        properties.insert("key".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        // Value field (searchable text)
        properties.insert("value".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        // Headers field (searchable JSON)
        properties.insert("headers".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        let mapping = IndexMapping { properties };
        
        // Create the main kafka messages index
        self.api.create_index_with_mapping("kafka-messages".to_string(), mapping.clone()).await
            .or_else(|e| {
                // Ignore if index already exists
                if e.to_string().contains("already exists") {
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
        
        debug!("Created kafka-messages index");
        Ok(())
    }

    /// Index a batch of Kafka messages
    pub async fn index_kafka_batch(
        &self,
        topic: &str,
        partition: i32,
        batch: &RecordBatch,
    ) -> Result<()> {
        // Use the TantivyIndexer if available for better performance
        if let Some(indexer) = &self.indexer {
            return indexer.index_batch(topic, partition, batch).await;
        }

        // Otherwise, index through the REST API
        for record in &batch.records {
            let doc_id = format!("{}-{}-{}", topic, partition, record.offset);
            
            let mut doc = serde_json::Map::new();
            doc.insert("_id".to_string(), serde_json::json!(doc_id));
            doc.insert("topic".to_string(), serde_json::json!(topic));
            doc.insert("partition".to_string(), serde_json::json!(partition));
            doc.insert("offset".to_string(), serde_json::json!(record.offset));
            doc.insert("timestamp".to_string(), serde_json::json!(record.timestamp));
            
            if let Some(key) = &record.key {
                let key_str = String::from_utf8_lossy(key);
                doc.insert("key".to_string(), serde_json::json!(key_str));
            }
            
            let value_str = String::from_utf8_lossy(&record.value);
            doc.insert("value".to_string(), serde_json::json!(value_str));
            
            if !record.headers.is_empty() {
                let headers_json = serde_json::to_string(&record.headers)
                    .unwrap_or_default();
                doc.insert("headers".to_string(), serde_json::json!(headers_json));
            }
            
            // Index document through API
            self.index_document("kafka-messages", &doc_id, serde_json::Value::Object(doc)).await?;
        }
        
        debug!("Indexed {} records from topic {} partition {}", 
               batch.records.len(), topic, partition);
        Ok(())
    }

    /// Index a document through the API
    async fn index_document(
        &self,
        _index: &str,
        _id: &str,
        _document: serde_json::Value,
    ) -> Result<()> {
        // This would normally use the HTTP client to call the API
        // For now, we'll directly use the API's internal method
        // In production, this would be an HTTP request
        
        error!("Direct document indexing not implemented - use HTTP API");
        Err(Error::Internal("Direct indexing not implemented".to_string()))
    }

    /// Search for messages across all indices
    pub async fn search_messages(
        &self,
        query: &str,
        topic_filter: Option<&str>,
        partition_filter: Option<i32>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        // If we have a direct indexer, use it
        if let Some(indexer) = &self.indexer {
            return indexer.search(query, topic_filter, partition_filter, limit).await;
        }

        // Otherwise, use the REST API
        error!("REST API search not implemented - use HTTP API");
        Err(Error::Internal("REST API search not implemented".to_string()))
    }

    /// Create a per-topic index
    pub async fn create_topic_index(&self, topic: &str) -> Result<()> {
        let index_name = format!("kafka-topic-{}", topic.replace('.', "_"));
        
        // Same mapping as the main index
        let mapping = self.create_kafka_mapping();
        
        self.api.create_index_with_mapping(index_name, mapping).await
            .or_else(|e| {
                // Ignore if index already exists
                if e.to_string().contains("already exists") {
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
        
        debug!("Created index for topic: {}", topic);
        Ok(())
    }

    /// Helper to create the standard Kafka message mapping
    fn create_kafka_mapping(&self) -> IndexMapping {
        let mut properties = HashMap::new();
        
        properties.insert("_id".to_string(), FieldMapping {
            field_type: "keyword".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("topic".to_string(), FieldMapping {
            field_type: "keyword".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("partition".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("offset".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("timestamp".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("key".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("value".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("headers".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        IndexMapping { properties }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_search_integration() {
        let api = Arc::new(SearchApi::new().unwrap());
        let mut integration = SearchIntegration::new(api).await.unwrap();
        
        // Create temp directory for index
        let temp_dir = TempDir::new().unwrap();
        let config = IndexerConfig {
            index_path: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        
        // Initialize indexer
        integration.init_kafka_indexer(config).await.unwrap();
        
        // Test creating topic index
        integration.create_topic_index("test-topic").await.unwrap();
    }
}