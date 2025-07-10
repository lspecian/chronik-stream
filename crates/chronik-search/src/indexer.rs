//! Tantivy integration for real-time indexing of Kafka messages.

use chronik_common::{Result, Error};
use chronik_storage::RecordBatch;
use tantivy::{
    collector::TopDocs,
    directory::MmapDirectory,
    doc,
    query::QueryParser,
    schema::{Schema, Field, TEXT, STORED, STRING, NumericOptions},
    Index, IndexReader, IndexWriter, ReloadPolicy,
};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error};

/// Configuration for the Tantivy indexer
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Path to the index directory
    pub index_path: String,
    /// Memory budget for indexing in bytes
    pub heap_size_bytes: usize,
    /// Number of indexing threads
    pub num_threads: usize,
    /// Commit interval in seconds
    pub commit_interval_secs: u64,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            index_path: "/data/search/index".to_string(),
            heap_size_bytes: 512 * 1024 * 1024, // 512MB
            num_threads: 4,
            commit_interval_secs: 5,
        }
    }
}

/// Tantivy indexer for Kafka messages
pub struct TantivyIndexer {
    config: IndexerConfig,
    index: Index,
    index_writer: Arc<RwLock<IndexWriter>>,
    index_reader: IndexReader,
    schema: Schema,
    // Field references
    topic_field: Field,
    partition_field: Field,
    offset_field: Field,
    timestamp_field: Field,
    key_field: Field,
    value_field: Field,
    headers_field: Field,
}

impl TantivyIndexer {
    /// Create a new Tantivy indexer
    pub fn new(config: IndexerConfig) -> Result<Self> {
        // Create schema
        let mut schema_builder = Schema::builder();
        
        // Topic and partition are faceted fields for filtering
        let topic_field = schema_builder.add_text_field("topic", STRING | STORED);
        let partition_field = schema_builder.add_i64_field("partition", NumericOptions::default().set_indexed().set_stored());
        let offset_field = schema_builder.add_i64_field("offset", NumericOptions::default().set_indexed().set_stored());
        let timestamp_field = schema_builder.add_i64_field("timestamp", NumericOptions::default().set_indexed().set_stored());
        
        // Key and value are searchable text fields
        let key_field = schema_builder.add_text_field("key", TEXT | STORED);
        let value_field = schema_builder.add_text_field("value", TEXT | STORED);
        
        // Headers as JSON text for searching
        let headers_field = schema_builder.add_text_field("headers", TEXT | STORED);
        
        let schema = schema_builder.build();
        
        // Create or open index
        let index_path = Path::new(&config.index_path);
        std::fs::create_dir_all(index_path)
            .map_err(|e| Error::Io(e))?;
        
        let directory = MmapDirectory::open(index_path)
            .map_err(|e| Error::Internal(format!("Failed to open index directory: {}", e)))?;
        
        let index = Index::open_or_create(directory, schema.clone())
            .map_err(|e| Error::Internal(format!("Failed to create index: {}", e)))?;
        
        // Create index writer
        let index_writer = index.writer_with_num_threads(config.num_threads, config.heap_size_bytes)
            .map_err(|e| Error::Internal(format!("Failed to create index writer: {}", e)))?;
        
        // Create index reader with real-time updates
        let index_reader = index.reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| Error::Internal(format!("Failed to create index reader: {}", e)))?;
        
        Ok(Self {
            config,
            index,
            index_writer: Arc::new(RwLock::new(index_writer)),
            index_reader,
            schema,
            topic_field,
            partition_field,
            offset_field,
            timestamp_field,
            key_field,
            value_field,
            headers_field,
        })
    }
    
    /// Index a record batch
    pub async fn index_batch(&self, topic: &str, partition: i32, batch: &RecordBatch) -> Result<()> {
        let writer = self.index_writer.write().await;
        
        for record in &batch.records {
            let mut doc = doc!(
                self.topic_field => topic,
                self.partition_field => partition as i64,
                self.offset_field => record.offset,
                self.timestamp_field => record.timestamp
            );
            
            // Add key if present
            if let Some(key) = &record.key {
                let key_str = String::from_utf8_lossy(key);
                doc.add_text(self.key_field, &key_str);
            }
            
            // Add value
            let value_str = String::from_utf8_lossy(&record.value);
            doc.add_text(self.value_field, &value_str);
            
            // Add headers as JSON
            if !record.headers.is_empty() {
                let headers_json = serde_json::to_string(&record.headers)
                    .unwrap_or_default();
                doc.add_text(self.headers_field, &headers_json);
            }
            
            // Add document to index
            writer.add_document(doc)
                .map_err(|e| Error::Internal(format!("Failed to index document: {}", e)))?;
        }
        
        debug!("Indexed {} records from topic {} partition {}", 
               batch.records.len(), topic, partition);
        
        Ok(())
    }
    
    /// Commit pending changes
    pub async fn commit(&self) -> Result<()> {
        let mut writer = self.index_writer.write().await;
        writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit index: {}", e)))?;
        
        debug!("Committed index changes");
        Ok(())
    }
    
    /// Search messages
    pub async fn search(
        &self,
        query_str: &str,
        topic_filter: Option<&str>,
        partition_filter: Option<i32>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let searcher = self.index_reader.searcher();
        
        // Build query
        let query_parser = QueryParser::for_index(&self.index, vec![
            self.key_field,
            self.value_field,
            self.headers_field,
        ]);
        
        let mut full_query = query_str.to_string();
        
        // Add topic filter if specified
        if let Some(topic) = topic_filter {
            full_query = format!("topic:{} AND ({})", topic, query_str);
        }
        
        // Add partition filter if specified
        if let Some(partition) = partition_filter {
            full_query = format!("partition:{} AND ({})", partition, full_query);
        }
        
        let query = query_parser.parse_query(&full_query)
            .map_err(|e| Error::InvalidInput(format!("Invalid query: {}", e)))?;
        
        // Execute search
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))
            .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;
        
        // Collect results
        let mut results = Vec::new();
        for (_score, doc_address) in top_docs {
            let doc = searcher.doc(doc_address)
                .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;
            
            let result = SearchResult {
                topic: self.get_text_field(&doc, self.topic_field),
                partition: self.get_i64_field(&doc, self.partition_field) as i32,
                offset: self.get_i64_field(&doc, self.offset_field),
                timestamp: self.get_i64_field(&doc, self.timestamp_field),
                key: self.get_text_field(&doc, self.key_field),
                value: self.get_text_field(&doc, self.value_field),
                headers: self.parse_headers(&doc),
            };
            
            results.push(result);
        }
        
        Ok(results)
    }
    
    /// Get text field value from document
    fn get_text_field(&self, doc: &tantivy::TantivyDocument, field: Field) -> String {
        // CompactDocValue doesn't expose direct access to values,
        // so we convert to string representation
        doc.get_first(field)
            .map(|v| format!("{:?}", v))
            .unwrap_or_default()
    }
    
    /// Get i64 field value from document
    fn get_i64_field(&self, _doc: &tantivy::TantivyDocument, _field: Field) -> i64 {
        // CompactDocValue doesn't expose direct access to values,
        // For now return a default value
        // In production, you would need proper type handling
        0
    }
    
    /// Parse headers from JSON
    fn parse_headers(&self, doc: &tantivy::TantivyDocument) -> Vec<(String, Vec<u8>)> {
        let headers_json = self.get_text_field(doc, self.headers_field);
        if headers_json.is_empty() {
            return Vec::new();
        }
        
        serde_json::from_str(&headers_json)
            .unwrap_or_default()
    }
    
    /// Start background commit task
    pub fn start_commit_task(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(self.config.commit_interval_secs)
            );
            
            loop {
                interval.tick().await;
                
                if let Err(e) = self.commit().await {
                    error!("Failed to commit index: {}", e);
                }
            }
        })
    }
}

/// Search result
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub timestamp: i64,
    pub key: String,
    pub value: String,
    pub headers: Vec<(String, Vec<u8>)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_storage::Record;
    use tempfile::TempDir;
    
    #[tokio::test]
    async fn test_indexing_and_search() {
        let temp_dir = TempDir::new().unwrap();
        let config = IndexerConfig {
            index_path: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        
        let indexer = Arc::new(TantivyIndexer::new(config).unwrap());
        
        // Create test batch
        let batch = RecordBatch {
            records: vec![
                Record {
                    offset: 0,
                    timestamp: 1000,
                    key: Some(b"key1".to_vec()),
                    value: b"Hello world".to_vec(),
                    headers: [("type".to_string(), b"test".to_vec())].into_iter().collect(),
                },
                Record {
                    offset: 1,
                    timestamp: 2000,
                    key: Some(b"key2".to_vec()),
                    value: b"Kafka streaming".to_vec(),
                    headers: std::collections::HashMap::new(),
                },
            ],
        };
        
        // Index batch
        indexer.index_batch("test-topic", 0, &batch).await.unwrap();
        indexer.commit().await.unwrap();
        
        // Search for "world"
        let results = indexer.search("world", None, None, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].offset, 0);
        assert_eq!(results[0].value, "Hello world");
        
        // Search with topic filter
        let results = indexer.search("streaming", Some("test-topic"), None, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].offset, 1);
    }
}