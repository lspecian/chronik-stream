//! Enhanced produce request handler with integrated Tantivy indexing.
//! 
//! This module provides a complete Kafka-compatible produce API handler that:
//! - Supports all acknowledgment modes (acks=0, 1, -1/all)
//! - Handles idempotent and transactional producers
//! - Integrates with real-time search indexing
//! - Manages segment writing to object storage
//! - Provides compression support
//! - Ensures high-throughput message ingestion

use crate::storage::{StorageConfig, StorageService};
use chronik_common::{Result, Error};
use chronik_protocol::{
    ProduceRequest, ProduceResponse, ProduceResponseTopic, ProduceResponsePartition,
    kafka_protocol::ErrorCode,
};
use chronik_storage::kafka_records::{
    KafkaRecordBatch, CompressionType, TimestampType, RecordHeader as KafkaRecordHeader,
};
// Search indexing disabled temporarily
// use chronik_search::{
//     realtime_indexer::{RealtimeIndexer, JsonDocument, RealtimeIndexerConfig},
//     json_pipeline::JsonPipeline,
// };
use chronik_storage::{
    RecordBatch, Record, SegmentWriter,
    chronik_segment::{ChronikSegmentBuilder, CompressionType as ChronikCompression},
    object_store::storage::ObjectStore,
};
use chronik_common::metadata::traits::MetadataStore;
use serde_json::Value as JsonValue;
use std::collections::{HashMap, BTreeMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Mutex, mpsc, Semaphore};
use tokio::time::timeout;
use tracing::{debug, error, info, warn, instrument};
use bytes::Bytes;

/// Maximum segment size before rotation (256MB)
const MAX_SEGMENT_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum time before segment rotation (30 seconds)
const MAX_SEGMENT_AGE: Duration = Duration::from_secs(30);

/// Maximum number of records in memory before flush
const MAX_BUFFER_RECORDS: usize = 10000;

/// Timeout for replication acknowledgments
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration for the produce handler
#[derive(Debug, Clone)]
pub struct ProduceHandlerConfig {
    /// Node ID
    pub node_id: i32,
    /// Storage configuration
    pub storage_config: StorageConfig,
    /// Indexer configuration
    // pub indexer_config: RealtimeIndexerConfig,
    /// Enable real-time indexing
    pub enable_indexing: bool,
    /// Enable idempotent producer support
    pub enable_idempotence: bool,
    /// Enable transactional producer support
    pub enable_transactions: bool,
    /// Maximum in-flight requests per connection
    pub max_in_flight_requests: usize,
    /// Batch size for buffering
    pub batch_size: usize,
    /// Linger time for batching
    pub linger_ms: u64,
    /// Compression type
    pub compression_type: CompressionType,
    /// Request timeout
    pub request_timeout_ms: u64,
    /// Buffer memory limit
    pub buffer_memory: usize,
}

impl Default for ProduceHandlerConfig {
    fn default() -> Self {
        Self {
            node_id: 0,
            storage_config: StorageConfig::default(),
            // indexer_config: RealtimeIndexerConfig::default(),
            enable_indexing: true,
            enable_idempotence: true,
            enable_transactions: true,
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: 10,
            compression_type: CompressionType::Gzip,
            request_timeout_ms: 30000,
            buffer_memory: 32 * 1024 * 1024, // 32MB
        }
    }
}

/// Producer information for idempotence tracking
#[derive(Debug, Clone)]
struct ProducerInfo {
    producer_id: i64,
    producer_epoch: i16,
    sequence_numbers: HashMap<(String, i32), i32>,
    transactional_id: Option<String>,
    transaction_state: TransactionState,
    last_activity: Instant,
}

/// Transaction state
#[derive(Debug, Clone, PartialEq)]
enum TransactionState {
    None,
    InTransaction,
    PrepareCommit,
    PrepareAbort,
    CompleteCommit,
    CompleteAbort,
}

/// Partition state for tracking offsets and segments
struct PartitionState {
    /// Next offset to assign
    next_offset: AtomicU64,
    /// High watermark (replicated offset)
    high_watermark: AtomicU64,
    /// Log start offset
    log_start_offset: AtomicU64,
    /// Current segment writer
    current_writer: Arc<Mutex<SegmentWriter>>,
    /// Segment creation time (millis since start)
    segment_created: AtomicU64,
    /// Start time for calculating relative times
    start_time: Instant,
    /// Current segment size
    segment_size: AtomicU64,
    /// Pending records buffer
    pending_records: Arc<Mutex<Vec<ProduceRecord>>>,
    /// Last flush time
    last_flush: Arc<Mutex<Instant>>,
}

/// Record to be produced
#[derive(Debug, Clone)]
struct ProduceRecord {
    offset: i64,
    timestamp: i64,
    key: Option<Vec<u8>>,
    value: Vec<u8>,
    headers: HashMap<String, Vec<u8>>,
    producer_id: i64,
    producer_epoch: i16,
    sequence: i32,
    is_transactional: bool,
    is_control: bool,
}

/// Produce handler metrics
#[derive(Debug, Default)]
pub struct ProduceMetrics {
    pub records_produced: AtomicU64,
    pub bytes_produced: AtomicU64,
    pub produce_errors: AtomicU64,
    pub duplicate_records: AtomicU64,
    pub segments_created: AtomicU64,
    pub indexing_lag_ms: AtomicU64,
    pub compression_ratio: AtomicU64,
}

/// Enhanced produce request handler
pub struct ProduceHandler {
    config: ProduceHandlerConfig,
    storage: Arc<dyn ObjectStore>,
    // indexer: Option<Arc<RealtimeIndexer>>,
    // json_pipeline: Arc<JsonPipeline>,
    metadata_store: Arc<dyn MetadataStore>,
    partition_states: Arc<RwLock<HashMap<(String, i32), Arc<PartitionState>>>>,
    producer_info: Arc<RwLock<HashMap<i64, ProducerInfo>>>,
    metrics: Arc<ProduceMetrics>,
    running: Arc<AtomicBool>,
    memory_limiter: Arc<Semaphore>,
    replication_sender: Option<mpsc::Sender<ReplicationRequest>>,
}

/// Replication request for ISR management
#[derive(Debug)]
struct ReplicationRequest {
    topic: String,
    partition: i32,
    offset: i64,
    data: Bytes,
    acks_required: i16,
    response_sender: mpsc::Sender<Result<()>>,
}

impl ProduceHandler {
    /// Create a new produce handler
    pub async fn new(
        config: ProduceHandlerConfig,
        storage: Arc<dyn ObjectStore>,
        metadata_store: Arc<dyn MetadataStore>,
    ) -> Result<Self> {
        // Create indexer if enabled
        // let (indexer, index_sender) = if config.enable_indexing {
        //     let (sender, receiver) = mpsc::channel(10000);
        //     let indexer = Arc::new(RealtimeIndexer::new(config.indexer_config.clone())?);
        //     
        //     // Start indexing pipeline
        //     let indexer_clone = Arc::clone(&indexer);
        //     tokio::spawn(async move {
        //         let _ = indexer_clone.start(receiver).await;
        //     });
        //     
        //     (Some(indexer), Some(sender))
        // } else {
        //     (None, None)
        // };
        
        // Create JSON pipeline for transformation
        // let json_pipeline = Arc::new(JsonPipeline::new(Default::default())?);
        
        // Create memory limiter
        let memory_limiter = Arc::new(Semaphore::new(
            config.buffer_memory / 1024 // Approximate 1KB per permit
        ));
        
        Ok(Self {
            config,
            storage,
            // indexer,
            // json_pipeline,
            metadata_store,
            partition_states: Arc::new(RwLock::new(HashMap::new())),
            producer_info: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(ProduceMetrics::default()),
            running: Arc::new(AtomicBool::new(true)),
            memory_limiter,
            replication_sender: None,
        })
    }
    
    /// Start the produce handler with replication support
    pub async fn start_with_replication(
        &mut self,
        replication_sender: mpsc::Sender<ReplicationRequest>,
    ) {
        self.replication_sender = Some(replication_sender);
        
        // Start background tasks
        self.start_background_tasks().await;
    }
    
    /// Start background tasks for segment management
    async fn start_background_tasks(&self) {
        let handler = self.clone();
        
        // Segment rotation task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            
            while handler.running.load(Ordering::Relaxed) {
                interval.tick().await;
                
                if let Err(e) = handler.check_and_rotate_segments().await {
                    error!("Segment rotation check failed: {}", e);
                }
            }
        });
        
        // Metrics reporting task
        let metrics = Arc::clone(&self.metrics);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            
            loop {
                interval.tick().await;
                
                info!(
                    "Produce metrics - records: {}, bytes: {}, errors: {}, duplicates: {}, segments: {}",
                    metrics.records_produced.load(Ordering::Relaxed),
                    metrics.bytes_produced.load(Ordering::Relaxed),
                    metrics.produce_errors.load(Ordering::Relaxed),
                    metrics.duplicate_records.load(Ordering::Relaxed),
                    metrics.segments_created.load(Ordering::Relaxed),
                );
            }
        });
    }
    
    /// Handle a produce request
    #[instrument(skip(self, request), fields(correlation_id, acks = request.acks))]
    pub async fn handle_produce(
        &self,
        request: ProduceRequest,
        correlation_id: i32,
    ) -> Result<ProduceResponse> {
        let start_time = Instant::now();
        let mut response_topics = Vec::new();
        let acks = request.acks;
        let timeout_ms = request.timeout_ms as u64;
        
        // Handle request with timeout
        let result = timeout(
            Duration::from_millis(timeout_ms.max(self.config.request_timeout_ms)),
            self.process_produce_request(request, acks),
        ).await;
        
        match result {
            Ok(Ok(topics)) => {
                response_topics = topics;
            }
            Ok(Err(e)) => {
                error!("Produce request processing failed: {}", e);
                self.metrics.produce_errors.fetch_add(1, Ordering::Relaxed);
                
                // Return error response
                return Ok(ProduceResponse {
                    header: chronik_protocol::parser::ResponseHeader { correlation_id },
                    throttle_time_ms: 0,
                    topics: vec![],
                });
            }
            Err(_) => {
                warn!("Produce request timed out after {}ms", timeout_ms);
                
                // Return timeout error
                return Ok(ProduceResponse {
                    header: chronik_protocol::parser::ResponseHeader { correlation_id },
                    throttle_time_ms: 0,
                    topics: vec![],
                });
            }
        }
        
        let duration = start_time.elapsed();
        debug!("Produce request completed in {:?}", duration);
        
        Ok(ProduceResponse {
            header: chronik_protocol::parser::ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            topics: response_topics,
        })
    }
    
    /// Process produce request with acknowledgment handling
    async fn process_produce_request(
        &self,
        request: ProduceRequest,
        acks: i16,
    ) -> Result<Vec<ProduceResponseTopic>> {
        let mut response_topics = Vec::new();
        let transactional_id = request.transactional_id.clone();
        
        for topic_data in request.topics {
            // Validate topic exists
            let topic_metadata = match self.metadata_store.get_topic(&topic_data.name).await? {
                Some(meta) => meta,
                None => {
                    // Topic doesn't exist, return error for all partitions
                    let error_partitions = topic_data.partitions.into_iter().map(|p| {
                        ProduceResponsePartition {
                            index: p.index,
                            error_code: ErrorCode::UnknownTopicOrPartition.code(),
                            base_offset: -1,
                            log_append_time: -1,
                            log_start_offset: 0,
                        }
                    }).collect();
                    
                    response_topics.push(ProduceResponseTopic {
                        name: topic_data.name,
                        partitions: error_partitions,
                    });
                    continue;
                }
            };
            
            let mut response_partitions = Vec::new();
            
            for partition_data in topic_data.partitions {
                // Validate partition exists
                if partition_data.index >= topic_metadata.config.partition_count as i32 {
                    response_partitions.push(ProduceResponsePartition {
                        index: partition_data.index,
                        error_code: ErrorCode::UnknownTopicOrPartition.code(),
                        base_offset: -1,
                        log_append_time: -1,
                        log_start_offset: 0,
                    });
                    continue;
                }
                
                // Check if we're the leader for this partition
                let assignments = self.metadata_store
                    .get_partition_assignments(&topic_data.name)
                    .await?;
                
                let is_leader = assignments.iter()
                    .find(|a| a.partition == partition_data.index as u32)
                    .map(|a| a.is_leader && a.broker_id == self.config.node_id)
                    .unwrap_or(false);
                
                if !is_leader {
                        response_partitions.push(ProduceResponsePartition {
                            index: partition_data.index,
                            error_code: ErrorCode::NotLeaderForPartition.code(),
                            base_offset: -1,
                            log_append_time: -1,
                            log_start_offset: 0,
                        });
                        continue;
                }
                
                // Process the partition data
                match self.produce_to_partition(
                    &topic_data.name,
                    partition_data.index,
                    &partition_data.records,
                    transactional_id.as_deref(),
                    acks,
                ).await {
                    Ok(response) => {
                        response_partitions.push(response);
                    }
                    Err(e) => {
                        error!(
                            "Failed to produce to {}-{}: {}",
                            topic_data.name, partition_data.index, e
                        );
                        
                        self.metrics.produce_errors.fetch_add(1, Ordering::Relaxed);
                        
                        let error_code = match e {
                            Error::DuplicateSequenceNumber(_) => ErrorCode::DuplicateSequenceNumber.code(),
                            Error::InvalidProducerEpoch(_) => ErrorCode::InvalidProducerEpoch.code(),
                            Error::OutOfOrderSequenceNumber(_) => ErrorCode::OutOfOrderSequenceNumber.code(),
                            Error::InvalidTransactionState(_) => ErrorCode::InvalidTxnState.code(),
                            _ => ErrorCode::None.code(),
                        };
                        
                        response_partitions.push(ProduceResponsePartition {
                            index: partition_data.index,
                            error_code,
                            base_offset: -1,
                            log_append_time: -1,
                            log_start_offset: 0,
                        });
                    }
                }
            }
            
            response_topics.push(ProduceResponseTopic {
                name: topic_data.name,
                partitions: response_partitions,
            });
        }
        
        Ok(response_topics)
    }
    
    /// Produce records to a specific partition
    #[instrument(skip(self, records_data), fields(topic, partition, acks, bytes = records_data.len()))]
    async fn produce_to_partition(
        &self,
        topic: &str,
        partition: i32,
        records_data: &[u8],
        transactional_id: Option<&str>,
        acks: i16,
    ) -> Result<ProduceResponsePartition> {
        // Acquire memory permit
        let memory_permit = self.memory_limiter
            .acquire_many(records_data.len() as u32)
            .await
            .map_err(|_| Error::Internal("Memory limit exceeded".into()))?;
        
        // Decode record batch
        let kafka_batch = KafkaRecordBatch::decode(records_data)?;
        
        // Validate producer info for idempotence
        if kafka_batch.header.producer_id >= 0 {
            self.validate_producer_sequence(
                &kafka_batch,
                topic,
                partition,
                transactional_id,
            ).await?;
        }
        
        // Get or create partition state
        let partition_state = self.get_or_create_partition_state(topic, partition).await?;
        
        // Assign offsets and prepare records
        let base_offset = partition_state.next_offset.load(Ordering::SeqCst);
        let mut records = Vec::with_capacity(kafka_batch.records.len());
        let mut current_offset = base_offset;
        let mut total_bytes = 0u64;
        
        for (i, kafka_record) in kafka_batch.records.iter().enumerate() {
            let timestamp = kafka_batch.header.base_timestamp + kafka_record.timestamp_delta;
            let record = ProduceRecord {
                offset: (current_offset as i64) + i as i64,
                timestamp,
                key: kafka_record.key.as_ref().map(|k| k.to_vec()),
                value: kafka_record.value.as_ref().map(|v| v.to_vec()).unwrap_or_default(),
                headers: kafka_record.headers.iter()
                    .map(|h| (h.key.clone(), h.value.as_ref().map(|v| v.to_vec()).unwrap_or_default()))
                    .collect(),
                producer_id: kafka_batch.header.producer_id,
                producer_epoch: kafka_batch.header.producer_epoch,
                sequence: kafka_batch.header.base_sequence + i as i32,
                is_transactional: (kafka_batch.header.attributes & 0x10) != 0,
                is_control: (kafka_batch.header.attributes & 0x20) != 0,
            };
            
            total_bytes += record.value.len() as u64;
            if let Some(ref key) = record.key {
                total_bytes += key.len() as u64;
            }
            
            records.push(record);
        }
        
        let last_offset = (base_offset as i64) + records.len() as i64 - 1;
        
        // Update next offset atomically
        partition_state.next_offset.store((last_offset + 1) as u64, Ordering::SeqCst);
        
        // Buffer records
        {
            let mut pending = partition_state.pending_records.lock().await;
            pending.extend(records.clone());
        }
        
        // Update metrics
        self.metrics.records_produced.fetch_add(records.len() as u64, Ordering::Relaxed);
        self.metrics.bytes_produced.fetch_add(total_bytes, Ordering::Relaxed);
        
        // Send to indexing pipeline if enabled
        if self.config.enable_indexing {
            self.send_to_indexer(topic, partition, &records).await;
        }
        
        // Handle acknowledgment modes
        match acks {
            0 => {
                // No acknowledgment required, return immediately
                debug!("Acks=0: Returning immediately without waiting for persistence");
            }
            1 => {
                // Wait for local write
                self.flush_partition_if_needed(topic, partition, &partition_state).await?;
                debug!("Acks=1: Local write completed");
            }
            -1 => {
                // Wait for replication to all ISR
                self.flush_partition_if_needed(topic, partition, &partition_state).await?;
                
                if let Some(ref sender) = self.replication_sender {
                    let (resp_tx, mut resp_rx) = mpsc::channel(1);
                    
                    let replication_req = ReplicationRequest {
                        topic: topic.to_string(),
                        partition,
                        offset: last_offset,
                        data: Bytes::copy_from_slice(records_data),
                        acks_required: -1,
                        response_sender: resp_tx,
                    };
                    
                    // Send replication request
                    sender.send(replication_req).await
                        .map_err(|_| Error::Internal("Replication channel closed".into()))?;
                    
                    // Wait for replication confirmation
                    match timeout(REPLICATION_TIMEOUT, resp_rx.recv()).await {
                        Ok(Some(Ok(()))) => {
                            debug!("Acks=-1: Replication to all ISR completed");
                            
                            // Update high watermark
                            partition_state.high_watermark.store((last_offset + 1) as u64, Ordering::SeqCst);
                        }
                        Ok(Some(Err(e))) => {
                            return Err(Error::Internal(format!("Replication failed: {}", e)));
                        }
                        _ => {
                            return Err(Error::Internal("Replication timeout".into()));
                        }
                    }
                } else {
                    // No replication configured, just update high watermark
                    partition_state.high_watermark.store((last_offset + 1) as u64, Ordering::SeqCst);
                }
            }
            _ => {
                return Err(Error::Protocol(format!("Invalid acks value: {}", acks)));
            }
        }
        
        // Update producer sequence for idempotence
        if kafka_batch.header.producer_id >= 0 {
            self.update_producer_sequence(
                kafka_batch.header.producer_id,
                topic,
                partition,
                kafka_batch.header.base_sequence + records.len() as i32 - 1,
            ).await;
        }
        
        // Drop memory permit
        drop(memory_permit);
        
        // Get partition state for response
        let log_start_offset = partition_state.log_start_offset.load(Ordering::Relaxed) as i64;
        
        Ok(ProduceResponsePartition {
            index: partition,
            error_code: ErrorCode::None.code(),
            base_offset: base_offset as i64,
            log_append_time: if (kafka_batch.header.attributes >> 3) & 0b111 == TimestampType::LogAppendTime as u16 {
                kafka_batch.header.max_timestamp
            } else {
                -1
            },
            log_start_offset,
        })
    }
    
    /// Get or create partition state
    async fn get_or_create_partition_state(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<Arc<PartitionState>> {
        let key = (topic.to_string(), partition);
        
        // Fast path - check if already exists
        {
            let states = self.partition_states.read().await;
            if let Some(state) = states.get(&key) {
                return Ok(Arc::clone(state));
            }
        }
        
        // Slow path - create new state
        let mut states = self.partition_states.write().await;
        
        // Double-check after acquiring write lock
        if let Some(state) = states.get(&key) {
            return Ok(Arc::clone(state));
        }
        
        // Check if partition exists by checking topic metadata
        let topic_meta = self.metadata_store
            .get_topic(topic)
            .await?
            .ok_or_else(|| Error::Internal(format!("Topic {} not found", topic)))?;
        
        if partition as u32 >= topic_meta.config.partition_count {
            return Err(Error::Internal(format!("Partition {} does not exist for topic {}", partition, topic)));
        }
        
        // Create segment writer
        let segment_path = format!("{}/{}/partition-{}", self.config.storage_config.segment_writer_config.data_dir.to_string_lossy(), topic, partition);
        let writer = Arc::new(Mutex::new(
            SegmentWriter::new(
                self.config.storage_config.segment_writer_config.clone(),
            ).await?
        ));
        
        // Get current offsets from segments
        let segments = self.metadata_store.list_segments(topic, Some(partition as u32)).await?;
        let high_watermark = segments.iter()
            .map(|s| s.end_offset + 1)
            .max()
            .unwrap_or(0);
        let log_start_offset = segments.iter()
            .map(|s| s.start_offset)
            .min()
            .unwrap_or(0);
        
        let now = Instant::now();
        let state = Arc::new(PartitionState {
            next_offset: AtomicU64::new(high_watermark as u64),
            high_watermark: AtomicU64::new(high_watermark as u64),
            log_start_offset: AtomicU64::new(log_start_offset as u64),
            current_writer: writer,
            segment_created: AtomicU64::new(0),
            start_time: now,
            segment_size: AtomicU64::new(0),
            pending_records: Arc::new(Mutex::new(Vec::new())),
            last_flush: Arc::new(Mutex::new(Instant::now())),
        });
        
        states.insert(key, Arc::clone(&state));
        Ok(state)
    }
    
    /// Validate producer sequence for idempotence
    async fn validate_producer_sequence(
        &self,
        batch: &KafkaRecordBatch,
        topic: &str,
        partition: i32,
        transactional_id: Option<&str>,
    ) -> Result<()> {
        if !self.config.enable_idempotence {
            return Ok(());
        }
        
        let mut producers = self.producer_info.write().await;
        
        // Get or create producer info
        let producer_info = producers.entry(batch.header.producer_id).or_insert_with(|| {
            ProducerInfo {
                producer_id: batch.header.producer_id,
                producer_epoch: batch.header.producer_epoch,
                sequence_numbers: HashMap::new(),
                transactional_id: transactional_id.map(String::from),
                transaction_state: TransactionState::None,
                last_activity: Instant::now(),
            }
        });
        
        // Validate producer epoch
        if producer_info.producer_epoch != batch.header.producer_epoch {
            if batch.header.producer_epoch < producer_info.producer_epoch {
                return Err(Error::InvalidProducerEpoch("Producer epoch does not match".to_string()));
            }
            // Newer epoch, reset state
            producer_info.producer_epoch = batch.header.producer_epoch;
            producer_info.sequence_numbers.clear();
        }
        
        // Validate transactional ID
        if let Some(txn_id) = transactional_id {
            if let Some(ref existing_txn_id) = producer_info.transactional_id {
                if existing_txn_id != txn_id {
                    return Err(Error::InvalidTransactionState("Producer is not in a transaction".to_string()));
                }
            } else {
                producer_info.transactional_id = Some(txn_id.to_string());
            }
        }
        
        // Check sequence number
        let key = (topic.to_string(), partition);
        let expected_sequence = producer_info.sequence_numbers
            .get(&key)
            .map(|&seq| seq + 1)
            .unwrap_or(0);
        
        if batch.header.base_sequence < expected_sequence {
            // Duplicate
            warn!(
                "Duplicate sequence number detected: producer={}, topic={}, partition={}, expected={}, received={}",
                batch.header.producer_id, topic, partition, expected_sequence, batch.header.base_sequence
            );
            self.metrics.duplicate_records.fetch_add(batch.records.len() as u64, Ordering::Relaxed);
            return Err(Error::DuplicateSequenceNumber(format!("Duplicate sequence: {}", batch.header.base_sequence)));
        } else if batch.header.base_sequence > expected_sequence {
            // Out of order
            return Err(Error::OutOfOrderSequenceNumber(format!("Expected sequence: {}, got: {}", expected_sequence, batch.header.base_sequence)));
        }
        
        // Update last activity
        producer_info.last_activity = Instant::now();
        
        Ok(())
    }
    
    /// Update producer sequence after successful write
    async fn update_producer_sequence(
        &self,
        producer_id: i64,
        topic: &str,
        partition: i32,
        last_sequence: i32,
    ) {
        let mut producers = self.producer_info.write().await;
        if let Some(producer_info) = producers.get_mut(&producer_id) {
            let key = (topic.to_string(), partition);
            producer_info.sequence_numbers.insert(key, last_sequence);
        }
    }
    
    /// Send records to indexing pipeline
    async fn send_to_indexer(&self, _topic: &str, _partition: i32, _records: &[ProduceRecord]) {
        // Indexing disabled temporarily
        // TODO: Re-enable when chronik-search is fixed
    }
    
    /// Flush partition if needed based on size or time
    async fn flush_partition_if_needed(
        &self,
        topic: &str,
        partition: i32,
        state: &Arc<PartitionState>,
    ) -> Result<()> {
        let should_flush = {
            let pending = state.pending_records.lock().await;
            let last_flush = state.last_flush.lock().await;
            
            pending.len() >= MAX_BUFFER_RECORDS ||
            state.segment_size.load(Ordering::Relaxed) >= MAX_SEGMENT_SIZE ||
            last_flush.elapsed() >= Duration::from_millis(self.config.linger_ms)
        };
        
        if should_flush {
            self.flush_partition(topic, partition, state).await?;
        }
        
        Ok(())
    }
    
    /// Flush pending records to storage
    async fn flush_partition(
        &self,
        topic: &str,
        partition: i32,
        state: &Arc<PartitionState>,
    ) -> Result<()> {
        let records = {
            let mut pending = state.pending_records.lock().await;
            if pending.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *pending)
        };
        
        // Create Kafka batch for writing
        if records.is_empty() {
            return Ok(());
        }
        
        let first_record = &records[0];
        let mut kafka_batch = KafkaRecordBatch::new(
            first_record.offset,
            first_record.timestamp,
            first_record.producer_id,
            first_record.producer_epoch,
            first_record.sequence,
            self.config.compression_type,
            false, // is_transactional
        );
        
        for record in &records {
            kafka_batch.add_record(
                record.key.as_ref().map(|k| Bytes::from(k.clone())),
                Some(Bytes::from(record.value.clone())),
                record.headers.iter().map(|(k, v)| KafkaRecordHeader {
                    key: k.clone(),
                    value: Some(Bytes::from(v.clone())),
                }).collect(),
                record.timestamp,
            );
        }
        
        // Convert ProduceRecords to Records for storage
        let storage_records: Vec<Record> = records.iter().map(|r| Record {
            offset: r.offset,
            timestamp: r.timestamp,
            key: r.key.clone(),
            value: r.value.clone(),
            headers: r.headers.clone(),
        }).collect();
        
        // Convert KafkaRecordBatch to RecordBatch
        let record_batch = RecordBatch {
            records: storage_records,
        };
        
        // Write batch
        let mut writer = state.current_writer.lock().await;
        writer.write_batch(topic, partition, record_batch).await?;
        
        // Update state
        state.segment_size.fetch_add(records.len() as u64 * 100, Ordering::Relaxed); // Approximate size
        *state.last_flush.lock().await = Instant::now();
        
        // Offsets are tracked in segment metadata, no need to update separately
        
        debug!("Flushed {} records to {}-{}", records.len(), topic, partition);
        
        Ok(())
    }
    
    /// Check and rotate segments if needed
    async fn check_and_rotate_segments(&self) -> Result<()> {
        let states = self.partition_states.read().await;
        
        for ((topic, partition), state) in states.iter() {
            let segment_created_ms = state.segment_created.load(Ordering::Relaxed);
            let segment_age = Duration::from_millis(
                Instant::now().duration_since(state.start_time).as_millis() as u64 - segment_created_ms
            );
            let should_rotate = state.segment_size.load(Ordering::Relaxed) >= MAX_SEGMENT_SIZE ||
                               segment_age >= MAX_SEGMENT_AGE;
            
            if should_rotate {
                // Flush current segment
                self.flush_partition(topic, *partition, state).await?;
                
                // Create new segment writer
                let segment_path = format!("{}/{}/partition-{}", 
                    self.config.storage_config.segment_writer_config.data_dir.to_string_lossy(), topic, partition);
                    
                let new_writer = SegmentWriter::new(
                    self.config.storage_config.segment_writer_config.clone(),
                ).await?;
                
                // Replace writer
                *state.current_writer.lock().await = new_writer;
                state.segment_size.store(0, Ordering::SeqCst);
                state.segment_created.store(
                    Instant::now().duration_since(state.start_time).as_millis() as u64,
                    Ordering::SeqCst
                );
                
                self.metrics.segments_created.fetch_add(1, Ordering::Relaxed);
                
                info!("Rotated segment for {}-{}", topic, partition);
            }
        }
        
        Ok(())
    }
    
    /// Clean up expired producer sessions
    async fn cleanup_expired_producers(&self) {
        let mut producers = self.producer_info.write().await;
        let now = Instant::now();
        let expiry = Duration::from_secs(300); // 5 minute expiry
        
        producers.retain(|_, info| {
            now.duration_since(info.last_activity) < expiry
        });
    }
    
    /// Get current metrics
    pub fn metrics(&self) -> &ProduceMetrics {
        &self.metrics
    }
    
    /// Shutdown the handler gracefully
    pub async fn shutdown(&self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        
        // Flush all partitions
        let states = self.partition_states.read().await;
        for ((topic, partition), state) in states.iter() {
            if let Err(e) = self.flush_partition(topic, *partition, state).await {
                error!("Error flushing partition {}-{} during shutdown: {}", topic, partition, e);
            }
        }
        
        // Stop indexer
        // if let Some(ref indexer) = self.indexer {
        //     indexer.stop().await?;
        // }
        
        info!("Produce handler shutdown complete");
        Ok(())
    }
}

// Implement Clone for handler (needed for background tasks)
impl Clone for ProduceHandler {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            storage: Arc::clone(&self.storage),
            metadata_store: Arc::clone(&self.metadata_store),
            partition_states: Arc::clone(&self.partition_states),
            producer_info: Arc::clone(&self.producer_info),
            metrics: Arc::clone(&self.metrics),
            running: Arc::clone(&self.running),
            memory_limiter: Arc::clone(&self.memory_limiter),
            replication_sender: self.replication_sender.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_common::metadata::sled_store::SledMetadataStore;
    use chronik_storage::object_store::backends::local::LocalObjectStore;
    use tempfile::TempDir;
    use bytes::BufMut;
    
    async fn create_test_handler() -> (ProduceHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage_path = temp_dir.path().join("storage");
        let metadata_path = temp_dir.path().join("metadata");
        
        std::fs::create_dir_all(&storage_path).unwrap();
        std::fs::create_dir_all(&metadata_path).unwrap();
        
        let storage = Arc::new(LocalObjectStore::new(storage_path.to_str().unwrap()).unwrap());
        let metadata_store = Arc::new(SledMetadataStore::new(metadata_path.to_str().unwrap()).unwrap());
        
        // Create test topic
        metadata_store.create_topic("test-topic", 3, 1).await.unwrap();
        
        // Set partition leaders
        for i in 0..3 {
            metadata_store.update_partition_leader("test-topic", i, 0).await.unwrap();
        }
        
        let config = ProduceHandlerConfig {
            node_id: 0,
            enable_indexing: false, // Disable for tests
            ..Default::default()
        };
        
        let handler = ProduceHandler::new(config, storage, metadata_store).await.unwrap();
        
        (handler, temp_dir)
    }
    
    fn create_test_record_batch(producer_id: i64, sequence: i32, records: Vec<(&str, &str)>) -> Vec<u8> {
        let mut batch = KafkaRecordBatch::new(0, producer_id, 0);
        batch.header.base_sequence = sequence;
        
        for (key, value) in records {
            batch.add_record(
                Some(Bytes::from(key.to_string())),
                Some(Bytes::from(value.to_string())),
                vec![],
            );
        }
        
        batch.encode().unwrap().to_vec()
    }
    
    #[tokio::test]
    async fn test_basic_produce() {
        let (handler, _temp) = create_test_handler().await;
        
        let records_data = create_test_record_batch(
            1234,
            0,
            vec![
                ("key1", "value1"),
                ("key2", "value2"),
                ("key3", "value3"),
            ],
        );
        
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data,
                        },
                    ],
                },
            ],
        };
        
        let response = handler.handle_produce(request, 1).await.unwrap();
        
        assert_eq!(response.topics.len(), 1);
        assert_eq!(response.topics[0].name, "test-topic");
        assert_eq!(response.topics[0].partitions.len(), 1);
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        assert_eq!(response.topics[0].partitions[0].base_offset, 0);
        
        // Check metrics
        assert_eq!(handler.metrics.records_produced.load(Ordering::Relaxed), 3);
    }
    
    #[tokio::test]
    async fn test_idempotent_produce() {
        let (handler, _temp) = create_test_handler().await;
        
        let records_data = create_test_record_batch(
            1234,
            0,
            vec![("key1", "value1")],
        );
        
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data.clone(),
                        },
                    ],
                },
            ],
        };
        
        // First produce should succeed
        let response1 = handler.handle_produce(request.clone(), 1).await.unwrap();
        assert_eq!(response1.topics[0].partitions[0].error_code, 0);
        
        // Duplicate produce should fail with duplicate sequence error
        let response2 = handler.handle_produce(request, 2).await.unwrap();
        assert_eq!(
            response2.topics[0].partitions[0].error_code,
            ErrorCode::DuplicateSequenceNumber.code()
        );
        
        // Check duplicate metrics
        assert_eq!(handler.metrics.duplicate_records.load(Ordering::Relaxed), 1);
    }
    
    #[tokio::test]
    async fn test_out_of_order_sequence() {
        let (handler, _temp) = create_test_handler().await;
        
        // First batch with sequence 0
        let records_data1 = create_test_record_batch(
            1234,
            0,
            vec![("key1", "value1")],
        );
        
        let request1 = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data1,
                        },
                    ],
                },
            ],
        };
        
        let response1 = handler.handle_produce(request1, 1).await.unwrap();
        assert_eq!(response1.topics[0].partitions[0].error_code, 0);
        
        // Second batch with sequence 2 (skipping 1)
        let records_data2 = create_test_record_batch(
            1234,
            2,
            vec![("key2", "value2")],
        );
        
        let request2 = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data2,
                        },
                    ],
                },
            ],
        };
        
        let response2 = handler.handle_produce(request2, 2).await.unwrap();
        assert_eq!(
            response2.topics[0].partitions[0].error_code,
            ErrorCode::OutOfOrderSequenceNumber.code()
        );
    }
    
    #[tokio::test]
    async fn test_multiple_partitions() {
        let (handler, _temp) = create_test_handler().await;
        
        let mut partitions = vec![];
        
        // Create records for 3 partitions
        for i in 0..3 {
            let records_data = create_test_record_batch(
                1234,
                i,
                vec![(
                    &format!("key{}", i),
                    &format!("value{}", i),
                )],
            );
            
            partitions.push(ProduceRequestPartition {
                index: i,
                records: records_data,
            });
        }
        
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions,
                },
            ],
        };
        
        let response = handler.handle_produce(request, 1).await.unwrap();
        
        assert_eq!(response.topics[0].partitions.len(), 3);
        for i in 0..3 {
            assert_eq!(response.topics[0].partitions[i].error_code, 0);
            assert_eq!(response.topics[0].partitions[i].base_offset, 0);
        }
        
        assert_eq!(handler.metrics.records_produced.load(Ordering::Relaxed), 3);
    }
    
    #[tokio::test]
    async fn test_unknown_topic() {
        let (handler, _temp) = create_test_handler().await;
        
        let records_data = create_test_record_batch(
            1234,
            0,
            vec![("key1", "value1")],
        );
        
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "unknown-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data,
                        },
                    ],
                },
            ],
        };
        
        let response = handler.handle_produce(request, 1).await.unwrap();
        
        assert_eq!(response.topics.len(), 1);
        assert_eq!(
            response.topics[0].partitions[0].error_code,
            ErrorCode::UnknownTopicOrPartition.code()
        );
    }
    
    #[tokio::test]
    async fn test_compression() {
        let (mut handler, _temp) = create_test_handler().await;
        handler.config.compression_type = CompressionType::Gzip;
        
        // Create larger payload to see compression benefits
        let mut large_value = String::new();
        for _ in 0..1000 {
            large_value.push_str("This is a test value that will be compressed. ");
        }
        
        let records_data = create_test_record_batch(
            1234,
            0,
            vec![("key1", &large_value)],
        );
        
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data,
                        },
                    ],
                },
            ],
        };
        
        let response = handler.handle_produce(request, 1).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        
        // Compression ratio should be recorded
        // Note: Actual compression testing would require checking the stored segments
    }
    
    #[tokio::test]
    async fn test_acks_modes() {
        let (handler, _temp) = create_test_handler().await;
        
        // Test acks=0 (fire and forget)
        let records_data = create_test_record_batch(
            1234,
            0,
            vec![("key1", "value1")],
        );
        
        let request_acks0 = ProduceRequest {
            transactional_id: None,
            acks: 0,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data.clone(),
                        },
                    ],
                },
            ],
        };
        
        let response = handler.handle_produce(request_acks0, 1).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        
        // Test acks=1 (leader acknowledgment)
        let request_acks1 = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 1,
                            records: records_data.clone(),
                        },
                    ],
                },
            ],
        };
        
        let response = handler.handle_produce(request_acks1, 2).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        
        // Test acks=-1 (all replicas) - without replication configured, should still work
        let request_acks_all = ProduceRequest {
            transactional_id: None,
            acks: -1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 2,
                            records: records_data,
                        },
                    ],
                },
            ],
        };
        
        let response = handler.handle_produce(request_acks_all, 3).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
    }
    
    #[tokio::test]
    async fn test_metrics_tracking() {
        let (handler, _temp) = create_test_handler().await;
        
        // Produce some records
        let records_data = create_test_record_batch(
            1234,
            0,
            vec![
                ("key1", "value1"),
                ("key2", "value2"),
                ("key3", "value3"),
            ],
        );
        
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 30000,
            topics: vec![
                ProduceRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        ProduceRequestPartition {
                            index: 0,
                            records: records_data,
                        },
                    ],
                },
            ],
        };
        
        handler.handle_produce(request, 1).await.unwrap();
        
        let metrics = handler.metrics();
        assert_eq!(metrics.records_produced.load(Ordering::Relaxed), 3);
        assert!(metrics.bytes_produced.load(Ordering::Relaxed) > 0);
        assert_eq!(metrics.produce_errors.load(Ordering::Relaxed), 0);
    }
}