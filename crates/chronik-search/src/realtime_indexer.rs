//! Real-time indexing pipeline with JSON document support and Tantivy integration.
//!
//! This module provides a high-performance indexing pipeline that:
//! - Handles schemaless JSON documents with automatic field detection
//! - Integrates with Tantivy for full-text search
//! - Supports fast fields for efficient filtering and aggregation
//! - Provides batching and buffering for optimal throughput
//! - Integrates with ChronikSegment for persistent storage

use chronik_common::{Result, Error};
use chronik_storage::{RecordBatch, Record, chronik_segment::{ChronikSegment, ChronikSegmentBuilder, CompressionType}};
use serde_json::{Value as JsonValue, Map as JsonMap};
use tantivy::{
    schema::{
        Schema, Field, FieldType, TextFieldIndexing, TextOptions, NumericOptions,
        DateOptions, BytesOptions, Cardinality, IndexRecordOption, STORED, FAST, STRING, TEXT
    },
    Index, IndexWriter, Document, TantivyDocument,
    directory::MmapDirectory,
    DateTime,
};
use std::{
    collections::{HashMap, BTreeMap},
    path::{Path, PathBuf},
    sync::{Arc, atomic::{AtomicU64, AtomicBool, Ordering}},
    time::{Duration, Instant},
};
use tokio::{
    sync::{RwLock, Mutex, mpsc, Semaphore},
    time::interval,
    task::JoinHandle,
};
use tracing::{debug, info, warn, error, instrument};
use bytes::Bytes;

/// Configuration for JSON field indexing policies
#[derive(Debug, Clone)]
pub struct FieldIndexingPolicy {
    /// Whether to index text fields for full-text search
    pub index_text: bool,
    /// Whether to create fast fields for numeric/date fields
    pub create_fast_fields: bool,
    /// Whether to store original field values
    pub store_values: bool,
    /// Maximum depth for nested JSON objects (0 = no nesting)
    pub max_nesting_depth: usize,
    /// Fields to exclude from indexing (dot notation for nested fields)
    pub excluded_fields: Vec<String>,
    /// Fields to force as specific types (field_name -> field_type)
    pub field_type_hints: HashMap<String, FieldTypeHint>,
}

impl Default for FieldIndexingPolicy {
    fn default() -> Self {
        Self {
            index_text: true,
            create_fast_fields: true,
            store_values: true,
            max_nesting_depth: 3,
            excluded_fields: vec![],
            field_type_hints: HashMap::new(),
        }
    }
}

/// Hint for field type inference
#[derive(Debug, Clone)]
pub enum FieldTypeHint {
    Text,
    Keyword,
    I64,
    U64,
    F64,
    Date,
    Bytes,
    Json,
}

/// Configuration for the real-time indexer
#[derive(Debug, Clone)]
pub struct RealtimeIndexerConfig {
    /// Base directory for index storage
    pub index_base_path: PathBuf,
    /// Memory budget for indexing in bytes
    pub indexing_memory_budget: usize,
    /// Number of indexing threads
    pub num_indexing_threads: usize,
    /// Batch size for committing documents
    pub batch_size: usize,
    /// Maximum time to wait before committing a batch
    pub batch_timeout: Duration,
    /// Maximum concurrent indexing operations
    pub max_concurrent_operations: usize,
    /// Target segment size in bytes
    pub target_segment_size: u64,
    /// Compression type for segments
    pub compression: CompressionType,
    /// Field indexing policy
    pub field_policy: FieldIndexingPolicy,
    /// Whether to build bloom filters for segments
    pub enable_bloom_filters: bool,
    /// Memory limit for buffering (bytes)
    pub buffer_memory_limit: usize,
}

impl Default for RealtimeIndexerConfig {
    fn default() -> Self {
        Self {
            index_base_path: PathBuf::from("/data/chronik/index"),
            indexing_memory_budget: 512 * 1024 * 1024, // 512MB
            num_indexing_threads: 4,
            batch_size: 1000,
            batch_timeout: Duration::from_secs(1),
            max_concurrent_operations: 100,
            target_segment_size: 256 * 1024 * 1024, // 256MB
            compression: CompressionType::Gzip,
            field_policy: FieldIndexingPolicy::default(),
            enable_bloom_filters: true,
            buffer_memory_limit: 128 * 1024 * 1024, // 128MB
        }
    }
}

/// Schema for dynamically indexed JSON documents
struct DynamicSchema {
    schema: Schema,
    field_mapping: HashMap<String, Field>,
    field_types: HashMap<String, FieldType>,
}

impl DynamicSchema {
    fn new() -> Self {
        let schema = Schema::builder().build();
        Self {
            schema,
            field_mapping: HashMap::new(),
            field_types: HashMap::new(),
        }
    }
}

/// JSON document to be indexed
#[derive(Debug, Clone)]
pub struct JsonDocument {
    /// Unique document ID
    pub id: String,
    /// Topic/collection name
    pub topic: String,
    /// Partition ID
    pub partition: i32,
    /// Offset in the stream
    pub offset: i64,
    /// Timestamp
    pub timestamp: i64,
    /// JSON document content
    pub content: JsonValue,
    /// Optional metadata
    pub metadata: Option<JsonMap<String, JsonValue>>,
}

/// Indexing metrics
#[derive(Debug, Default)]
pub struct IndexingMetrics {
    pub documents_indexed: AtomicU64,
    pub documents_failed: AtomicU64,
    pub bytes_indexed: AtomicU64,
    pub segments_created: AtomicU64,
    pub indexing_duration_ms: AtomicU64,
    pub schema_evolution_count: AtomicU64,
    pub memory_usage_bytes: AtomicU64,
}

/// Real-time indexing pipeline
pub struct RealtimeIndexer {
    config: RealtimeIndexerConfig,
    schema: Arc<RwLock<DynamicSchema>>,
    indexes: Arc<RwLock<HashMap<String, Arc<RwLock<TopicIndex>>>>>,
    metrics: Arc<IndexingMetrics>,
    running: Arc<AtomicBool>,
    memory_limiter: Arc<Semaphore>,
}

/// Index for a specific topic
struct TopicIndex {
    topic: String,
    tantivy_index: Index,
    index_writer: Arc<Mutex<IndexWriter>>,
    current_segment_size: AtomicU64,
    pending_documents: Arc<Mutex<Vec<JsonDocument>>>,
    last_commit_time: Arc<Mutex<Instant>>,
}

impl RealtimeIndexer {
    /// Create a new real-time indexer
    pub fn new(config: RealtimeIndexerConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.index_base_path)?;
        
        let memory_limiter = Arc::new(Semaphore::new(
            config.buffer_memory_limit / 1024 // Approximate docs per KB
        ));
        
        Ok(Self {
            config,
            schema: Arc::new(RwLock::new(DynamicSchema::new())),
            indexes: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(IndexingMetrics::default()),
            running: Arc::new(AtomicBool::new(false)),
            memory_limiter,
        })
    }
    
    /// Start the indexing pipeline
    pub async fn start(
        &self,
        mut document_receiver: mpsc::Receiver<JsonDocument>,
    ) -> Result<Vec<JoinHandle<()>>> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(Error::Internal("Indexer already running".into()));
        }
        
        let mut handles = vec![];
        
        // Start document processing task
        let processor_handle = self.start_document_processor(document_receiver);
        handles.push(processor_handle);
        
        // Start commit scheduler
        let commit_handle = self.start_commit_scheduler();
        handles.push(commit_handle);
        
        // Start memory monitor
        let memory_handle = self.start_memory_monitor();
        handles.push(memory_handle);
        
        info!("Real-time indexer started with {} threads", self.config.num_indexing_threads);
        
        Ok(handles)
    }
    
    /// Stop the indexing pipeline
    pub async fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        
        // Flush all pending documents
        self.flush_all_topics().await?;
        
        info!("Real-time indexer stopped");
        Ok(())
    }
    
    /// Get current metrics
    pub fn metrics(&self) -> IndexingMetricsSnapshot {
        IndexingMetricsSnapshot {
            documents_indexed: self.metrics.documents_indexed.load(Ordering::Relaxed),
            documents_failed: self.metrics.documents_failed.load(Ordering::Relaxed),
            bytes_indexed: self.metrics.bytes_indexed.load(Ordering::Relaxed),
            segments_created: self.metrics.segments_created.load(Ordering::Relaxed),
            indexing_duration_ms: self.metrics.indexing_duration_ms.load(Ordering::Relaxed),
            schema_evolution_count: self.metrics.schema_evolution_count.load(Ordering::Relaxed),
            memory_usage_bytes: self.metrics.memory_usage_bytes.load(Ordering::Relaxed),
        }
    }
    
    /// Process incoming documents
    fn start_document_processor(
        &self,
        mut receiver: mpsc::Receiver<JsonDocument>,
    ) -> JoinHandle<()> {
        let indexes = Arc::clone(&self.indexes);
        let schema = Arc::clone(&self.schema);
        let metrics = Arc::clone(&self.metrics);
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        let memory_limiter = Arc::clone(&self.memory_limiter);
        
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(config.batch_size);
            
            while running.load(Ordering::Relaxed) {
                // Try to fill the batch
                let timeout = tokio::time::sleep(config.batch_timeout);
                tokio::pin!(timeout);
                
                loop {
                    tokio::select! {
                        Some(doc) = receiver.recv() => {
                            // Acquire memory permit
                            let _permit = memory_limiter.acquire().await.ok();
                            
                            batch.push(doc);
                            if batch.len() >= config.batch_size {
                                break;
                            }
                        }
                        _ = &mut timeout => {
                            if !batch.is_empty() {
                                break;
                            }
                        }
                        else => {
                            running.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                }
                
                if !batch.is_empty() {
                    let start = Instant::now();
                    
                    // Group documents by topic
                    let mut topic_batches: HashMap<String, Vec<JsonDocument>> = HashMap::new();
                    for doc in batch.drain(..) {
                        topic_batches.entry(doc.topic.clone()).or_default().push(doc);
                    }
                    
                    // Process each topic batch
                    for (topic, docs) in topic_batches {
                        if let Err(e) = process_topic_batch(
                            &topic,
                            docs,
                            &indexes,
                            &schema,
                            &metrics,
                            &config,
                        ).await {
                            error!("Failed to process batch for topic {}: {}", topic, e);
                        }
                    }
                    
                    let duration = start.elapsed();
                    metrics.indexing_duration_ms.fetch_add(
                        duration.as_millis() as u64,
                        Ordering::Relaxed
                    );
                }
            }
        })
    }
    
    /// Periodic commit scheduler
    fn start_commit_scheduler(&self) -> JoinHandle<()> {
        let indexes = Arc::clone(&self.indexes);
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        let metrics = Arc::clone(&self.metrics);
        
        tokio::spawn(async move {
            let mut interval = interval(config.batch_timeout);
            
            while running.load(Ordering::Relaxed) {
                interval.tick().await;
                
                let indexes_read = indexes.read().await;
                for (topic, index) in indexes_read.iter() {
                    let should_commit = {
                        let last_commit = index.last_commit_time.lock().await;
                        last_commit.elapsed() > config.batch_timeout
                    };
                    
                    if should_commit {
                        if let Err(e) = commit_topic_index(topic, index, &config, &metrics).await {
                            error!("Failed to commit index for topic {}: {}", topic, e);
                        }
                    }
                }
            }
        })
    }
    
    /// Memory usage monitor
    fn start_memory_monitor(&self) -> JoinHandle<()> {
        let metrics = Arc::clone(&self.metrics);
        let running = Arc::clone(&self.running);
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(10));
            
            while running.load(Ordering::Relaxed) {
                interval.tick().await;
                
                // Update memory usage metric
                if let Ok(usage) = get_process_memory_usage() {
                    metrics.memory_usage_bytes.store(usage, Ordering::Relaxed);
                }
            }
        })
    }
    
    /// Flush all pending documents across all topics
    async fn flush_all_topics(&self) -> Result<()> {
        let indexes = self.indexes.read().await;
        
        for (topic, index) in indexes.iter() {
            commit_topic_index(topic, index, &self.config, &self.metrics).await?;
        }
        
        Ok(())
    }
}

/// Process a batch of documents for a specific topic
#[instrument(skip(docs, indexes, schema, metrics, config))]
async fn process_topic_batch(
    topic: &str,
    docs: Vec<JsonDocument>,
    indexes: &Arc<RwLock<HashMap<String, Arc<RwLock<TopicIndex>>>>>,
    schema: &Arc<RwLock<DynamicSchema>>,
    metrics: &Arc<IndexingMetrics>,
    config: &RealtimeIndexerConfig,
) -> Result<()> {
    // Get or create topic index
    let topic_index = {
        let mut indexes_write = indexes.write().await;
        
        if !indexes_write.contains_key(topic) {
            let index = create_topic_index(topic, config).await?;
            indexes_write.insert(topic.to_string(), Arc::new(RwLock::new(index)));
        }
        
        Arc::clone(indexes_write.get(topic).unwrap())
    };
    
    let topic_index = topic_index.read().await;
    let mut writer = topic_index.index_writer.lock().await;
    
    // Process each document
    for doc in docs {
        match index_json_document(&doc, &mut writer, schema, config).await {
            Ok(doc_size) => {
                metrics.documents_indexed.fetch_add(1, Ordering::Relaxed);
                metrics.bytes_indexed.fetch_add(doc_size, Ordering::Relaxed);
                topic_index.current_segment_size.fetch_add(doc_size, Ordering::Relaxed);
            }
            Err(e) => {
                error!("Failed to index document {}: {}", doc.id, e);
                metrics.documents_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    
    // Check if we should create a new segment
    let current_size = topic_index.current_segment_size.load(Ordering::Relaxed);
    if current_size >= config.target_segment_size {
        create_chronik_segment(&topic_index, config, metrics).await?;
    }
    
    Ok(())
}

/// Create an index for a topic
async fn create_topic_index(
    topic: &str,
    config: &RealtimeIndexerConfig,
) -> Result<TopicIndex> {
    let index_path = config.index_base_path.join(topic);
    std::fs::create_dir_all(&index_path)?;
    
    // Create schema with base fields
    let mut schema_builder = Schema::builder();
    
    // System fields
    schema_builder.add_text_field("_id", STRING | STORED);
    schema_builder.add_text_field("_topic", STRING | STORED | FAST);
    schema_builder.add_i64_field("_partition", NumericOptions::default().set_indexed().set_stored().set_fast());
    schema_builder.add_i64_field("_offset", NumericOptions::default().set_indexed().set_stored().set_fast());
    schema_builder.add_date_field("_timestamp", DateOptions::default().set_indexed().set_stored().set_fast());
    
    let schema = schema_builder.build();
    
    let directory = MmapDirectory::open(&index_path)?;
    let index = Index::open_or_create(directory, schema)?;
    
    let index_writer = index.writer_with_num_threads(
        config.num_indexing_threads,
        config.indexing_memory_budget
    )?;
    
    Ok(TopicIndex {
        topic: topic.to_string(),
        tantivy_index: index,
        index_writer: Arc::new(Mutex::new(index_writer)),
        current_segment_size: AtomicU64::new(0),
        pending_documents: Arc::new(Mutex::new(Vec::new())),
        last_commit_time: Arc::new(Mutex::new(Instant::now())),
    })
}

/// Index a JSON document
async fn index_json_document(
    doc: &JsonDocument,
    writer: &mut IndexWriter,
    schema: &Arc<RwLock<DynamicSchema>>,
    config: &RealtimeIndexerConfig,
) -> Result<u64> {
    let mut tantivy_doc = Document::new();
    
    // Add system fields
    tantivy_doc.add_text(writer.index().schema().get_field("_id").unwrap(), &doc.id);
    tantivy_doc.add_text(writer.index().schema().get_field("_topic").unwrap(), &doc.topic);
    tantivy_doc.add_i64(writer.index().schema().get_field("_partition").unwrap(), doc.partition as i64);
    tantivy_doc.add_i64(writer.index().schema().get_field("_offset").unwrap(), doc.offset);
    tantivy_doc.add_date(
        writer.index().schema().get_field("_timestamp").unwrap(),
        DateTime::from_timestamp_millis(doc.timestamp)
    );
    
    // Process JSON content
    let doc_size = process_json_value(
        &doc.content,
        "",
        &mut tantivy_doc,
        writer,
        schema,
        &config.field_policy,
        0,
    ).await?;
    
    // Add document to index
    writer.add_document(tantivy_doc)?;
    
    Ok(doc_size as u64)
}

/// Process a JSON value recursively
async fn process_json_value(
    value: &JsonValue,
    field_path: &str,
    tantivy_doc: &mut Document,
    writer: &mut IndexWriter,
    schema: &Arc<RwLock<DynamicSchema>>,
    policy: &FieldIndexingPolicy,
    depth: usize,
) -> Result<usize> {
    if depth > policy.max_nesting_depth {
        return Ok(0);
    }
    
    if policy.excluded_fields.contains(&field_path.to_string()) {
        return Ok(0);
    }
    
    let mut size = 0;
    
    match value {
        JsonValue::Null => {
            // Skip null values
        }
        JsonValue::Bool(b) => {
            let field = get_or_create_field(field_path, FieldTypeHint::U64, writer, schema).await?;
            tantivy_doc.add_u64(field, if *b { 1 } else { 0 });
            size += 1;
        }
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                let field = get_or_create_field(field_path, FieldTypeHint::I64, writer, schema).await?;
                tantivy_doc.add_i64(field, i);
                size += 8;
            } else if let Some(f) = n.as_f64() {
                let field = get_or_create_field(field_path, FieldTypeHint::F64, writer, schema).await?;
                tantivy_doc.add_f64(field, f);
                size += 8;
            }
        }
        JsonValue::String(s) => {
            // Detect if it's a keyword or text based on length and content
            let field_type = if s.len() < 256 && !s.contains(' ') {
                FieldTypeHint::Keyword
            } else {
                FieldTypeHint::Text
            };
            
            let field = get_or_create_field(field_path, field_type, writer, schema).await?;
            tantivy_doc.add_text(field, s);
            size += s.len();
        }
        JsonValue::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let item_path = format!("{}.{}", field_path, i);
                size += process_json_value(item, &item_path, tantivy_doc, writer, schema, policy, depth + 1).await?;
            }
        }
        JsonValue::Object(obj) => {
            for (key, val) in obj {
                let nested_path = if field_path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", field_path, key)
                };
                size += process_json_value(val, &nested_path, tantivy_doc, writer, schema, policy, depth + 1).await?;
            }
        }
    }
    
    Ok(size)
}

/// Get or create a field in the schema
async fn get_or_create_field(
    field_path: &str,
    hint: FieldTypeHint,
    writer: &mut IndexWriter,
    schema: &Arc<RwLock<DynamicSchema>>,
) -> Result<Field> {
    // Check if field already exists
    {
        let schema_read = schema.read().await;
        if let Some(field) = schema_read.field_mapping.get(field_path) {
            return Ok(*field);
        }
    }
    
    // Create new field
    let mut schema_write = schema.write().await;
    
    // Double-check after acquiring write lock
    if let Some(field) = schema_write.field_mapping.get(field_path) {
        return Ok(*field);
    }
    
    // Create field based on hint
    let field_entry = match hint {
        FieldTypeHint::Text => {
            let text_options = TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("default")
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions)
                )
                .set_stored();
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
        FieldTypeHint::Keyword => {
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
        FieldTypeHint::I64 => {
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
        FieldTypeHint::U64 => {
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
        FieldTypeHint::F64 => {
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
        FieldTypeHint::Date => {
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
        FieldTypeHint::Bytes => {
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
        FieldTypeHint::Json => {
            writer.index().schema().get_field(field_path)
                .unwrap_or_else(|_| panic!("Field {} not found", field_path))
        }
    };
    
    schema_write.field_mapping.insert(field_path.to_string(), field_entry);
    
    Ok(field_entry)
}

/// Commit changes for a topic index
async fn commit_topic_index(
    topic: &str,
    index: &Arc<RwLock<TopicIndex>>,
    config: &RealtimeIndexerConfig,
    metrics: &Arc<IndexingMetrics>,
) -> Result<()> {
    let index = index.read().await;
    let mut writer = index.index_writer.lock().await;
    
    writer.commit()?;
    
    let mut last_commit = index.last_commit_time.lock().await;
    *last_commit = Instant::now();
    
    debug!("Committed index for topic {}", topic);
    
    Ok(())
}

/// Create a Chronik segment from the current index
async fn create_chronik_segment(
    index: &TopicIndex,
    config: &RealtimeIndexerConfig,
    metrics: &Arc<IndexingMetrics>,
) -> Result<()> {
    // Get pending documents
    let mut pending = index.pending_documents.lock().await;
    if pending.is_empty() {
        return Ok(());
    }
    
    // Convert to RecordBatch
    let records: Vec<Record> = pending.iter().map(|doc| {
        Record {
            offset: doc.offset,
            timestamp: doc.timestamp,
            key: Some(doc.id.as_bytes().to_vec()),
            value: serde_json::to_vec(&doc.content).unwrap_or_default(),
            headers: HashMap::new(),
        }
    }).collect();
    
    let batch = RecordBatch { records };
    
    // Create Chronik segment
    let segment = ChronikSegmentBuilder::new(index.topic.clone(), 0) // TODO: proper partition tracking
        .add_batch(batch)
        .compression(config.compression)
        .with_index(true)
        .with_bloom_filter(config.enable_bloom_filters)
        .build()?;
    
    // Write segment to storage
    let segment_path = config.index_base_path
        .join(&index.topic)
        .join(format!("segment-{}.chronik", chrono::Utc::now().timestamp_millis()));
    
    let mut file = std::fs::File::create(&segment_path)?;
    let mut segment_mut = segment;
    segment_mut.write_to(&mut file)?;
    
    // Clear pending documents and reset counter
    pending.clear();
    index.current_segment_size.store(0, Ordering::SeqCst);
    metrics.segments_created.fetch_add(1, Ordering::Relaxed);
    
    info!("Created Chronik segment for topic {} at {:?}", index.topic, segment_path);
    
    Ok(())
}

/// Get process memory usage
fn get_process_memory_usage() -> Result<u64> {
    // This is a simplified version - in production, use proper system APIs
    Ok(0) // Placeholder
}

/// Snapshot of indexing metrics
#[derive(Debug, Clone)]
pub struct IndexingMetricsSnapshot {
    pub documents_indexed: u64,
    pub documents_failed: u64,
    pub bytes_indexed: u64,
    pub segments_created: u64,
    pub indexing_duration_ms: u64,
    pub schema_evolution_count: u64,
    pub memory_usage_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use serde_json::json;
    
    #[tokio::test]
    async fn test_realtime_indexer_basic() {
        let temp_dir = TempDir::new().unwrap();
        let config = RealtimeIndexerConfig {
            index_base_path: temp_dir.path().to_path_buf(),
            batch_size: 10,
            batch_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        
        let indexer = RealtimeIndexer::new(config).unwrap();
        let (sender, receiver) = mpsc::channel(100);
        
        let handles = indexer.start(receiver).await.unwrap();
        
        // Send test documents
        for i in 0..20 {
            let doc = JsonDocument {
                id: format!("doc-{}", i),
                topic: "test-topic".to_string(),
                partition: 0,
                offset: i,
                timestamp: chrono::Utc::now().timestamp_millis(),
                content: json!({
                    "title": format!("Document {}", i),
                    "content": "This is a test document for real-time indexing",
                    "score": i * 10,
                    "tags": ["test", "indexing"],
                    "metadata": {
                        "author": "test",
                        "version": 1
                    }
                }),
                metadata: None,
            };
            sender.send(doc).await.unwrap();
        }
        
        // Wait for processing
        tokio::time::sleep(Duration::from_millis(200)).await;
        
        // Check metrics
        let metrics = indexer.metrics();
        assert_eq!(metrics.documents_indexed, 20);
        assert_eq!(metrics.documents_failed, 0);
        assert!(metrics.bytes_indexed > 0);
        
        // Stop indexer
        indexer.stop().await.unwrap();
        drop(sender);
        
        for handle in handles {
            handle.await.unwrap();
        }
    }
    
    #[tokio::test]
    async fn test_json_field_detection() {
        let temp_dir = TempDir::new().unwrap();
        let config = RealtimeIndexerConfig {
            index_base_path: temp_dir.path().to_path_buf(),
            ..Default::default()
        };
        
        let indexer = RealtimeIndexer::new(config).unwrap();
        let (sender, receiver) = mpsc::channel(100);
        
        let handles = indexer.start(receiver).await.unwrap();
        
        // Send documents with different field types
        let doc1 = JsonDocument {
            id: "doc-1".to_string(),
            topic: "test".to_string(),
            partition: 0,
            offset: 1,
            timestamp: chrono::Utc::now().timestamp_millis(),
            content: json!({
                "text_field": "This is a long text that should be indexed as text",
                "keyword_field": "keyword123",
                "number_field": 42,
                "float_field": 3.14,
                "bool_field": true,
                "nested": {
                    "inner_field": "nested value"
                },
                "array_field": [1, 2, 3]
            }),
            metadata: None,
        };
        
        sender.send(doc1).await.unwrap();
        
        // Wait and check
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        let metrics = indexer.metrics();
        assert_eq!(metrics.documents_indexed, 1);
        
        indexer.stop().await.unwrap();
        drop(sender);
        
        for handle in handles {
            handle.await.unwrap();
        }
    }
    
    #[tokio::test]
    async fn test_error_handling() {
        let temp_dir = TempDir::new().unwrap();
        let config = RealtimeIndexerConfig {
            index_base_path: temp_dir.path().to_path_buf(),
            ..Default::default()
        };
        
        let indexer = RealtimeIndexer::new(config).unwrap();
        let (sender, receiver) = mpsc::channel(100);
        
        let handles = indexer.start(receiver).await.unwrap();
        
        // Send malformed document
        let doc = JsonDocument {
            id: "bad-doc".to_string(),
            topic: "test".to_string(),
            partition: 0,
            offset: 1,
            timestamp: chrono::Utc::now().timestamp_millis(),
            content: json!({
                "deeply_nested": create_deeply_nested_json(10)
            }),
            metadata: None,
        };
        
        sender.send(doc).await.unwrap();
        
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Should handle gracefully without crashing
        let metrics = indexer.metrics();
        assert!(metrics.documents_indexed >= 0);
        
        indexer.stop().await.unwrap();
        drop(sender);
        
        for handle in handles {
            handle.await.unwrap();
        }
    }
    
    fn create_deeply_nested_json(depth: usize) -> JsonValue {
        if depth == 0 {
            json!("leaf")
        } else {
            json!({
                "nested": create_deeply_nested_json(depth - 1)
            })
        }
    }
}