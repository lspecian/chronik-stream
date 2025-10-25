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
use crate::fetch_handler::FetchHandler;
use chronik_common::{Result, Error};
use chronik_protocol::{
    ProduceRequest, ProduceResponse, ProduceResponseTopic, ProduceResponsePartition,
    kafka_protocol::ErrorCode,
};
use chronik_storage::kafka_records::{
    KafkaRecordBatch, CompressionType, TimestampType, RecordHeader as KafkaRecordHeader,
};
use chronik_search::{
    realtime_indexer::{RealtimeIndexer, JsonDocument, RealtimeIndexerConfig},
    json_pipeline::JsonPipeline,
};
use serde_json::Map as JsonMap;
use chronik_storage::{
    RecordBatch, Record, SegmentWriter,
    object_store::storage::ObjectStore,
};
use chronik_common::metadata::traits::MetadataStore;
use chronik_wal::WalManager;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Mutex, mpsc, Semaphore};
use tokio::time::timeout;
use tracing::{debug, error, info, warn, trace, instrument};
use bytes::Bytes;

/// Maximum segment size before rotation (256MB)
const MAX_SEGMENT_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum time before segment rotation (30 seconds - reasonable interval)
/// We flush after each produce for immediate availability, rotation is for segment management
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
    pub indexer_config: RealtimeIndexerConfig,
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
    /// Enable automatic topic creation
    pub auto_create_topics_enable: bool,
    /// Default number of partitions for auto-created topics
    pub num_partitions: u32,
    /// Default replication factor for auto-created topics
    pub default_replication_factor: u32,
    /// Flush profile for pending_batches management
    pub flush_profile: ProduceFlushProfile,
}

impl Default for ProduceHandlerConfig {
    fn default() -> Self {
        let profile = ProduceFlushProfile::auto_select();
        info!("ProduceHandler using flush profile: {} (min_batches={}, linger_ms={}, buffer_memory={}MB)",
              profile.name(), profile.min_batches(), profile.linger_ms(), profile.buffer_memory() / (1024 * 1024));

        Self {
            node_id: 0,
            storage_config: StorageConfig::default(),
            indexer_config: RealtimeIndexerConfig::default(),
            enable_indexing: true,
            enable_idempotence: true,
            enable_transactions: true,
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: profile.linger_ms(),
            compression_type: CompressionType::Gzip,
            request_timeout_ms: 120000,  // 120 seconds (increased from 30s to handle slow topic auto-creation)
            buffer_memory: profile.buffer_memory(),
            auto_create_topics_enable: true,
            num_partitions: 3,
            default_replication_factor: 1,
            flush_profile: profile,
        }
    }
}

/// ProduceHandler flush performance profiles
///
/// Controls when buffered messages in `pending_batches` become visible to consumers.
/// Similar to WAL profiles, but optimizes the in-memory flush layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProduceFlushProfile {
    /// Low-latency profile: Optimize for minimal latency (<20ms p99)
    /// Use case: Real-time analytics, instant messaging, live dashboards
    LowLatency,

    /// Balanced profile: Good throughput with reasonable latency (100-150ms p99)
    /// Use case: General-purpose streaming, typical microservices (DEFAULT)
    Balanced,

    /// High-throughput profile: Maximum throughput at cost of higher latency
    /// Use case: Log aggregation, data pipelines, ETL, batch processing
    HighThroughput,
}

impl Default for ProduceFlushProfile {
    fn default() -> Self {
        Self::Balanced
    }
}

impl ProduceFlushProfile {
    /// Auto-select profile based on environment variable or default to Balanced
    pub fn auto_select() -> Self {
        if let Ok(profile) = std::env::var("CHRONIK_PRODUCE_PROFILE") {
            match profile.to_lowercase().as_str() {
                "low" | "low-latency" | "realtime" => Self::LowLatency,
                "balanced" | "medium" | "default" => Self::Balanced,
                "high" | "high-throughput" | "bulk" => Self::HighThroughput,
                _ => {
                    warn!("Unknown CHRONIK_PRODUCE_PROFILE '{}', using Balanced", profile);
                    Self::Balanced
                }
            }
        } else {
            Self::Balanced
        }
    }

    /// Get minimum batches before flush
    pub fn min_batches(&self) -> usize {
        match self {
            Self::LowLatency => 1,      // Flush immediately
            Self::Balanced => 10,        // Wait for 10 batches
            Self::HighThroughput => 100, // Wait for 100 batches
        }
    }

    /// Get linger time (max time before forced flush)
    pub fn linger_ms(&self) -> u64 {
        match self {
            Self::LowLatency => 10,      // 10ms max wait
            Self::Balanced => 100,       // 100ms max wait
            Self::HighThroughput => 500, // 500ms max wait
        }
    }

    /// Get buffer memory size
    pub fn buffer_memory(&self) -> usize {
        match self {
            Self::LowLatency => 16 * 1024 * 1024,  // 16MB
            Self::Balanced => 32 * 1024 * 1024,     // 32MB
            Self::HighThroughput => 128 * 1024 * 1024, // 128MB
        }
    }

    /// Get profile name for logging
    pub fn name(&self) -> &'static str {
        match self {
            Self::LowLatency => "LowLatency",
            Self::Balanced => "Balanced",
            Self::HighThroughput => "HighThroughput",
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

/// Batch of records with original wire format preserved
struct BufferedBatch {
    /// Original wire-format bytes from Produce request (with correct CRC)
    raw_bytes: Vec<u8>,
    /// Parsed records for indexing
    records: Vec<ProduceRecord>,
    /// Base offset for this batch
    base_offset: i64,
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
    /// Pending batches buffer (preserves original wire format)
    pending_batches: Arc<Mutex<Vec<BufferedBatch>>>,
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
    pub storage_write_errors: AtomicU64,
    pub storage_write_retries: AtomicU64,
    // Topic auto-creation metrics
    pub topics_auto_created: AtomicU64,
    pub topic_creation_errors: AtomicU64,
    pub topic_creation_invalid_names: AtomicU64,
    pub topic_creation_concurrent_attempts: AtomicU64,
}

/// Topic creation statistics
#[derive(Debug, Clone)]
pub struct TopicCreationStats {
    pub topics_created: u64,
    pub creation_errors: u64,
    pub invalid_name_attempts: u64,
    pub concurrent_attempts: u64,
}

/// Enhanced produce request handler
pub struct ProduceHandler {
    config: ProduceHandlerConfig,
    storage: Arc<dyn ObjectStore>,
    indexer: Option<Arc<RealtimeIndexer>>,
    json_pipeline: Arc<JsonPipeline>,
    index_sender: Option<mpsc::Sender<JsonDocument>>,
    metadata_store: Arc<dyn MetadataStore>,
    partition_states: Arc<RwLock<HashMap<(String, i32), Arc<PartitionState>>>>,
    producer_info: Arc<RwLock<HashMap<i64, ProducerInfo>>>,
    metrics: Arc<ProduceMetrics>,
    running: Arc<AtomicBool>,
    memory_limiter: Arc<Semaphore>,
    replication_sender: Option<mpsc::Sender<ReplicationRequest>>,
    fetch_handler: Option<Arc<FetchHandler>>,
    /// Track in-flight topic creation requests to prevent duplicates
    topic_creation_cache: Arc<RwLock<HashMap<String, Arc<Mutex<Option<chronik_common::metadata::TopicMetadata>>>>>>,
    /// WAL manager for inline durability writes (v1.3.47+)
    /// Uses Arc<WalManager> directly - no RwLock needed since WalManager uses DashMap internally
    wal_manager: Option<Arc<WalManager>>,
    /// Raft replica manager for multi-partition replication (optional, for Raft-enabled partitions)
    /// If Some, produce requests will use Raft consensus for those partitions
    /// If None, all produce requests use existing direct-write path (backward compatible)
    #[cfg(feature = "raft")]
    raft_manager: Option<Arc<crate::raft_integration::RaftReplicaManager>>,
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
    /// Get reference to metadata store
    pub fn get_metadata_store(&self) -> &Arc<dyn MetadataStore> {
        &self.metadata_store
    }

    /// Get request timeout in milliseconds
    pub fn get_request_timeout_ms(&self) -> u64 {
        self.config.request_timeout_ms
    }

    /// Ensure a partition exists with the specified starting offset (for WAL recovery)
    pub async fn ensure_partition_exists(
        &self,
        topic: &str,
        partition: i32,
        next_offset: i64,
    ) -> Result<()> {
        let key = (topic.to_string(), partition);

        // Check if partition already exists
        {
            let states = self.partition_states.read().await;
            if states.contains_key(&key) {
                // Update the offset if needed
                if let Some(state) = states.get(&key) {
                    let current_offset = state.next_offset.load(Ordering::SeqCst);
                    if next_offset > current_offset as i64 {
                        state.next_offset.store(next_offset as u64, Ordering::SeqCst);
                        info!(
                            "Updated partition {}-{} next offset from {} to {}",
                            topic, partition, current_offset, next_offset
                        );
                    }
                }
                return Ok(());
            }
        }

        // Create new partition state
        let mut states = self.partition_states.write().await;
        if !states.contains_key(&key) {
            let state = self.create_partition_state_with_offset(topic, partition, next_offset).await?;
            states.insert(key, Arc::new(state));
            info!(
                "Created partition {}-{} with starting offset {}",
                topic, partition, next_offset
            );
        }

        Ok(())
    }

    /// Apply recovered batch to partition buffers (for WAL recovery)
    pub async fn apply_recovered_batch(
        &self,
        topic: &str,
        partition: i32,
        batch_data: Bytes,
    ) -> Result<()> {
        let key = (topic.to_string(), partition);

        let state = {
            let states = self.partition_states.read().await;
            states.get(&key).cloned()
        };

        if let Some(state) = state {
            // Add to pending batches
            let mut pending = state.pending_batches.lock().await;
            pending.push(BufferedBatch {
                raw_bytes: batch_data.to_vec(),
                records: Vec::new(), // Will be parsed when needed
                base_offset: 0, // Will be assigned when written
            });

            info!(
                "Applied recovered batch to partition {}-{} buffers",
                topic, partition
            );
        } else {
            warn!(
                "Partition {}-{} not found when applying recovered batch",
                topic, partition
            );
        }

        Ok(())
    }

    /// Get high watermark for a partition (NEW architecture v1.3.39+)
    ///
    /// Returns the next offset that will be assigned = the high watermark.
    /// This is the SOURCE OF TRUTH for FetchHandler - no need to query metadata store.
    pub async fn get_high_watermark(&self, topic: &str, partition: i32) -> Result<i64> {
        let key = (topic.to_string(), partition);
        let states = self.partition_states.read().await;

        if let Some(state) = states.get(&key) {
            let high_watermark = state.next_offset.load(Ordering::SeqCst) as i64;
            Ok(high_watermark)
        } else {
            // Partition doesn't exist yet, high watermark is 0
            Ok(0)
        }
    }

    /// Update high watermark in metadata store (for WAL recovery)
    ///
    /// NOTE: This is only used during WAL recovery to update the metadata store.
    /// In normal operation, FetchHandler should call get_high_watermark() instead.
    pub async fn update_high_watermark(
        &self,
        topic: &str,
        partition: i32,
        high_watermark: i64,
    ) -> Result<()> {
        // Get current log_start_offset or default to 0
        let log_start_offset = self.metadata_store
            .get_partition_offset(topic, partition as u32)
            .await?
            .map(|(_, lso)| lso)
            .unwrap_or(0);

        self.metadata_store.update_partition_offset(topic, partition as u32, high_watermark, log_start_offset).await
            .map_err(|e| Error::Internal(format!("Failed to update partition offset: {}", e)))
    }

    /// Extract pending batches for WAL writing (v1.3.37)
    ///
    /// This method retrieves the batches that were just written to the partition buffer
    /// so they can be persisted to the WAL. The batches are NOT cleared, as they're still
    /// needed for serving fetch requests.
    pub async fn get_pending_batches(&self, topic: &str, partition: i32) -> Result<Vec<Vec<u8>>> {
        let key = (topic.to_string(), partition);

        let state = {
            let states = self.partition_states.read().await;
            states.get(&key).cloned()
        };

        if let Some(state) = state {
            let pending = state.pending_batches.lock().await;
            // Return cloned raw_bytes from all pending batches
            Ok(pending.iter().map(|b| b.raw_bytes.clone()).collect())
        } else {
            Ok(Vec::new())
        }
    }

    /// Clear pending batches after they've been written to WAL (v1.3.43)
    pub async fn clear_pending_batches(&self, topic: &str, partition: i32) -> Result<()> {
        let key = (topic.to_string(), partition);

        let state = {
            let states = self.partition_states.read().await;
            states.get(&key).cloned()
        };

        if let Some(state) = state {
            let mut pending = state.pending_batches.lock().await;
            let count = pending.len();
            pending.clear();
            debug!("Cleared {} pending batches for {}-{} after WAL write", count, topic, partition);
        }

        Ok(())
    }

    /// Clear all partition buffers (used before WAL recovery to prevent duplicates - v1.3.52+)
    ///
    /// This is called during server startup BEFORE WAL recovery to ensure we start with
    /// a clean slate. Without this, WAL replay would add recovered data on top of any
    /// existing in-memory state, causing duplicate messages.
    pub async fn clear_all_buffers(&self) -> Result<()> {
        let states = self.partition_states.read().await;
        let mut total_cleared = 0;

        for (key, state) in states.iter() {
            let mut pending = state.pending_batches.lock().await;
            let count = pending.len();
            total_cleared += count;
            pending.clear();
            if count > 0 {
                debug!("Cleared {} pending batches for {}-{}", count, key.0, key.1);
            }
        }

        info!("Cleared all partition buffers before WAL recovery: {} total batches", total_cleared);
        Ok(())
    }

    /// Restore partition state from WAL recovery (v1.3.48)
    ///
    /// Called during server startup to restore partition state after crash.
    /// Sets the next_offset based on recovered high watermark from WAL.
    pub async fn restore_partition_state(
        &self,
        topic: &str,
        partition: i32,
        high_watermark: u64,
    ) -> Result<()> {
        let key = (topic.to_string(), partition);
        let mut states = self.partition_states.write().await;

        if !states.contains_key(&key) {
            // Create new partition state with recovered offset
            let state = self.create_partition_state_with_offset(topic, partition, high_watermark as i64).await?;
            states.insert(key, Arc::new(state));
            info!("Restored partition state: {}-{} with high watermark {}", topic, partition, high_watermark);
        }

        Ok(())
    }

    /// Create partition state with specific starting offset
    async fn create_partition_state_with_offset(
        &self,
        topic: &str,
        partition: i32,
        next_offset: i64,
    ) -> Result<PartitionState> {
        // Create segment path
        let segment_path = self.config.storage_config.segment_writer_config.data_dir
            .join(topic)
            .join(format!("{:010}", partition))
            .join(format!("{:020}.log", next_offset));

        // Ensure directory exists
        tokio::fs::create_dir_all(segment_path.parent().unwrap()).await?;

        // Configure the writer with the segment directory
        let mut writer_config = self.config.storage_config.segment_writer_config.clone();
        writer_config.data_dir = segment_path.parent().unwrap().to_path_buf();

        let writer = SegmentWriter::new(writer_config).await?;

        Ok(PartitionState {
            next_offset: AtomicU64::new(next_offset as u64),
            high_watermark: AtomicU64::new(next_offset as u64),
            log_start_offset: AtomicU64::new(0),
            current_writer: Arc::new(Mutex::new(writer)),
            segment_created: AtomicU64::new(0),
            start_time: Instant::now(),
            segment_size: AtomicU64::new(0),
            pending_batches: Arc::new(Mutex::new(Vec::new())),
            last_flush: Arc::new(Mutex::new(Instant::now())),
        })
    }
    /// Create a new produce handler
    pub async fn new(
        config: ProduceHandlerConfig,
        storage: Arc<dyn ObjectStore>,
        metadata_store: Arc<dyn MetadataStore>,
    ) -> Result<Self> {
        // Create indexer if enabled
        let (indexer, index_sender) = if config.enable_indexing {
            let (sender, receiver) = mpsc::channel(10000);
            let indexer = Arc::new(RealtimeIndexer::new(config.indexer_config.clone())?);
            
            // Start indexing pipeline
            let indexer_clone = Arc::clone(&indexer);
            tokio::spawn(async move {
                let _ = indexer_clone.start(receiver).await;
            });
            
            (Some(indexer), Some(sender))
        } else {
            (None, None)
        };
        
        // Create JSON pipeline for transformation
        let json_pipeline = Arc::new(JsonPipeline::new(Default::default(), config.indexer_config.clone()).await?);
        
        // Create memory limiter
        let memory_limiter = Arc::new(Semaphore::new(
            config.buffer_memory / 1024 // Approximate 1KB per permit
        ));
        
        Ok(Self {
            config,
            storage,
            indexer,
            json_pipeline,
            index_sender,
            metadata_store,
            partition_states: Arc::new(RwLock::new(HashMap::new())),
            producer_info: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(ProduceMetrics::default()),
            running: Arc::new(AtomicBool::new(true)),
            memory_limiter,
            replication_sender: None,
            fetch_handler: None,
            topic_creation_cache: Arc::new(RwLock::new(HashMap::new())),
            wal_manager: None,
            #[cfg(feature = "raft")]
            raft_manager: None,
        })
    }

    /// Create a new ProduceHandler with WAL support for inline durability (v1.3.47+)
    /// WalManager uses DashMap internally so no external RwLock needed
    pub async fn new_with_wal(
        config: ProduceHandlerConfig,
        storage: Arc<dyn ObjectStore>,
        metadata_store: Arc<dyn MetadataStore>,
        wal_manager: Arc<WalManager>,
    ) -> Result<Self> {
        info!("CRITICAL_DEBUG: ProduceHandler::new_with_wal() called - setting wal_manager");
        let mut handler = Self::new(config, storage, metadata_store).await?;
        handler.wal_manager = Some(wal_manager);
        info!("CRITICAL_DEBUG: wal_manager set to Some - inline WAL writes ENABLED");
        Ok(handler)
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
    
    /// Set the fetch handler for updating buffers
    pub fn set_fetch_handler(&mut self, fetch_handler: Arc<FetchHandler>) {
        self.fetch_handler = Some(fetch_handler);
    }

    /// Set the Raft replica manager for Raft-enabled partitions (optional)
    #[cfg(feature = "raft")]
    pub fn set_raft_manager(&mut self, raft_manager: Arc<crate::raft_integration::RaftReplicaManager>) {
        info!("Setting RaftReplicaManager for ProduceHandler");
        self.raft_manager = Some(raft_manager);
    }
    
    /// Create a new produce handler with fetch handler connected
    pub async fn new_with_fetch_handler(
        config: ProduceHandlerConfig,
        storage: Arc<dyn ObjectStore>,
        metadata_store: Arc<dyn MetadataStore>,
        fetch_handler: Arc<FetchHandler>,
    ) -> Result<Self> {
        let mut handler = Self::new(config, storage, metadata_store).await?;
        handler.fetch_handler = Some(fetch_handler);
        Ok(handler)
    }
    
    /// Start background tasks for segment management
    pub async fn start_background_tasks(&self) {
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
    
    /// Check leadership using metadata store (fallback when Raft is not enabled)
    ///
    /// # Returns
    /// (is_leader, leader_hint) where leader_hint is the current leader node ID if known
    async fn check_metadata_leadership(&self, topic: &str, partition: i32) -> Result<(bool, Option<u64>)> {
        let assignments = self.metadata_store
            .get_partition_assignments(topic)
            .await?;

        // Debug partition assignments and leadership
        debug!(
            "Metadata leadership check for {}-{}: node_id={}, assignments={:?}",
            topic, partition, self.config.node_id, assignments
        );

        let (is_leader, leader_id) = assignments.iter()
            .find(|a| a.partition == partition as u32)
            .map(|a| {
                let leader = a.is_leader && a.broker_id == self.config.node_id;
                let leader_node = if a.is_leader {
                    Some(a.broker_id as u64)
                } else {
                    None
                };
                debug!(
                    "Partition {}-{}: assignment broker_id={}, is_leader={}, our_node_id={}, result={}",
                    topic, partition, a.broker_id, a.is_leader, self.config.node_id, leader
                );
                (leader, leader_node)
            })
            .unwrap_or((false, None));

        Ok((is_leader, leader_id))
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

        // Clone topic names for error handling
        let topic_names: Vec<(String, Vec<i32>)> = request.topics.iter()
            .map(|t| (t.name.clone(), t.partitions.iter().map(|p| p.index).collect()))
            .collect();
        
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
                
                // Return error response for all topics in the request
                let error_topics = topic_names.iter().map(|(topic_name, partitions)| {
                    ProduceResponseTopic {
                        name: topic_name.clone(),
                        partitions: partitions.iter().map(|&index| {
                            ProduceResponsePartition {
                                index,
                                error_code: ErrorCode::KafkaStorageError.code(),
                                base_offset: -1,
                                log_append_time: -1,
                                log_start_offset: 0,
                            }
                        }).collect(),
                    }
                }).collect();
                
                return Ok(ProduceResponse {
                    header: chronik_protocol::parser::ResponseHeader { correlation_id },
                    throttle_time_ms: 0,
                    topics: error_topics,
                });
            }
            Err(_) => {
                let actual_timeout = timeout_ms.max(self.config.request_timeout_ms);
                warn!("Produce request timed out after {}ms (client requested {}ms, server config {}ms)",
                    actual_timeout, timeout_ms, self.config.request_timeout_ms);

                // Return timeout error for all topics
                let timeout_topics = topic_names.iter().map(|(topic_name, partitions)| {
                    ProduceResponseTopic {
                        name: topic_name.clone(),
                        partitions: partitions.iter().map(|&index| {
                            ProduceResponsePartition {
                                index,
                                error_code: ErrorCode::RequestTimedOut.code(),
                                base_offset: -1,
                                log_append_time: -1,
                                log_start_offset: 0,
                            }
                        }).collect(),
                    }
                }).collect();
                
                return Ok(ProduceResponse {
                    header: chronik_protocol::parser::ResponseHeader { correlation_id },
                    throttle_time_ms: 0,
                    topics: timeout_topics,
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
            // Validate topic exists or auto-create if enabled
            let topic_metadata = match self.metadata_store.get_topic(&topic_data.name).await? {
                Some(meta) => meta,
                None => {
                    // Topic doesn't exist - check if auto-creation is enabled
                    if self.config.auto_create_topics_enable {
                        debug!("Topic '{}' not found, attempting auto-creation", topic_data.name);
                        // Attempt to auto-create the topic
                        match self.auto_create_topic(&topic_data.name).await {
                            Ok(meta) => {
                                info!("Successfully auto-created topic '{}' - produce request will now proceed", 
                                    topic_data.name);
                                meta
                            }
                            Err(e) => {
                                warn!("Failed to auto-create topic '{}': {:?} - produce request will fail", 
                                    topic_data.name, e);
                                // Return error for all partitions
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
                        }
                    } else {
                        // Auto-creation disabled, return error for all partitions
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
                
                // Check leadership: Raft takes precedence over metadata store
                #[cfg(feature = "raft")]
                let (is_leader, leader_hint) = {
                    if let Some(ref raft_manager) = self.raft_manager {
                        // Check if this partition is managed by Raft
                        if raft_manager.has_replica(&topic_data.name, partition_data.index) {
                            let is_raft_leader = raft_manager.is_leader(&topic_data.name, partition_data.index);
                            let leader_id = raft_manager.get_leader(&topic_data.name, partition_data.index);

                            debug!(
                                "Raft leadership check for {}-{}: is_leader={}, leader_id={:?}",
                                topic_data.name, partition_data.index, is_raft_leader, leader_id
                            );

                            (is_raft_leader, leader_id)
                        } else {
                            // Partition not managed by Raft, fall back to metadata store
                            debug!(
                                "Partition {}-{} not managed by Raft, using metadata store for leadership",
                                topic_data.name, partition_data.index
                            );
                            self.check_metadata_leadership(&topic_data.name, partition_data.index).await?
                        }
                    } else {
                        // No Raft manager, use metadata store
                        self.check_metadata_leadership(&topic_data.name, partition_data.index).await?
                    }
                };

                #[cfg(not(feature = "raft"))]
                let (is_leader, leader_hint) = self.check_metadata_leadership(&topic_data.name, partition_data.index).await?;

                if !is_leader {
                    debug!(
                        "Not leader for {}-{}, returning NOT_LEADER_FOR_PARTITION (leader_hint={:?})",
                        topic_data.name, partition_data.index, leader_hint
                    );
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
                debug!(
                    "PARTITION_DATA: topic={}, partition={}, records_len={} bytes",
                    topic_data.name, partition_data.index, partition_data.records.len()
                );
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
        records_data: &[u8],  // This contains the original wire-format bytes with correct CRC!
        transactional_id: Option<&str>,
        acks: i16,
    ) -> Result<ProduceResponsePartition> {
        use std::time::Instant;
        let start_time = Instant::now();

        // ENTRY POINT LOGGING (v1.3.47 debugging)
        info!("→ produce_to_partition({}-{}) bytes={} acks={}", topic, partition, records_data.len(), acks);

        // Acquire memory permit
        let memory_permit = self.memory_limiter
            .acquire_many(records_data.len() as u32)
            .await
            .map_err(|_| Error::Internal("Memory limit exceeded".into()))?;
        
        // Get or create partition state FIRST to determine base_offset
        let partition_state = self.get_or_create_partition_state(topic, partition).await?;
        let base_offset = partition_state.next_offset.load(Ordering::SeqCst);

        // CRITICAL FIX (v1.3.46): Skip modification when offsets match to preserve CRC perfectly
        // Java Kafka client requires byte-perfect CRC validation. Even header-only updates
        // can cause CRC recalculation issues. When base_offset matches, store AS-IS.
        use chronik_storage::canonical_record::CanonicalRecord;

        // Parse incoming base_offset from records_data
        let incoming_base_offset = if records_data.len() >= 8 {
            i64::from_be_bytes([
                records_data[0], records_data[1], records_data[2], records_data[3],
                records_data[4], records_data[5], records_data[6], records_data[7],
            ])
        } else {
            return Err(Error::Protocol("Invalid record batch: too short".into()));
        };

        // Check if we need to modify the batch at all
        let (re_encoded_bytes, kafka_batch) = if incoming_base_offset == base_offset as i64 {
            // Perfect match - store original bytes AS-IS (preserves CRC perfectly)
            debug!(
                "Base offset match ({}) - storing original bytes without modification (byte-perfect CRC preservation)",
                base_offset
            );
            let bytes = Bytes::copy_from_slice(records_data);

            // OPTIMIZATION: Decode once when offset matches
            let (batch, _) = KafkaRecordBatch::decode(records_data)
                .map_err(|e| Error::Protocol(format!("Failed to decode record batch: {}", e)))?;

            (bytes, batch)
        } else {
            // Offset mismatch - update ONLY base_offset field to preserve CRC
            debug!(
                "Base offset mismatch (incoming={}, assigned={}) - updating base_offset WITHOUT changing CRC",
                incoming_base_offset, base_offset
            );

            // CRITICAL FIX (v1.3.59): Kafka v2 CRC is calculated from partition_leader_epoch onwards.
            // The base_offset field (first 8 bytes) is NOT included in CRC calculation.
            // Therefore, we can update ONLY base_offset and keep everything else byte-identical.

            // Create a copy and update ONLY the first 8 bytes
            let mut updated_bytes = records_data.to_vec();
            updated_bytes[0..8].copy_from_slice(&(base_offset as i64).to_be_bytes());
            let bytes = Bytes::from(updated_bytes);

            // Decode to get batch metadata (needed for records count, etc.)
            let (kafka_batch, _) = KafkaRecordBatch::decode(&bytes)
                .map_err(|e| Error::Protocol(format!("Failed to decode updated batch: {}", e)))?;

            (bytes, kafka_batch)
        };

        // Validate producer info for idempotence
        if kafka_batch.header.producer_id >= 0 {
            self.validate_producer_sequence(
                &kafka_batch,
                topic,
                partition,
                transactional_id,
            ).await?;
        }
        
        // partition_state and base_offset already loaded above (before re-encoding)

        // Assign offsets and prepare records
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

            // Trace-level logging only (removed from hot path for performance)
            trace!(
                "Record: topic={} partition={} offset={} value_len={}",
                topic,
                partition,
                record.offset,
                record.value.len()
            );

            total_bytes += record.value.len() as u64;
            if let Some(ref key) = record.key {
                total_bytes += key.len() as u64;
            }
            
            records.push(record);
        }
        
        let last_offset = (base_offset as i64) + records.len() as i64 - 1;
        
        // Update next offset atomically
        partition_state.next_offset.store((last_offset + 1) as u64, Ordering::SeqCst);
        
        // Persist offset to metadata store
        let high_watermark = partition_state.high_watermark.load(Ordering::SeqCst) as i64;
        let log_start_offset = partition_state.log_start_offset.load(Ordering::SeqCst) as i64;
        if let Err(e) = self.metadata_store.update_partition_offset(
            topic, 
            partition as u32, 
            (last_offset + 1) as i64,  // Update high watermark to next offset
            log_start_offset
        ).await {
            warn!("Failed to persist partition offset to metadata store: {:?}", e);
        }
        
        // Log batch-level summary (performance-optimized)
        info!(
            "Batch: topic={} partition={} base_offset={} records={} bytes={}",
            topic, partition, base_offset, records.len(), total_bytes
        );

        // PHASE 3: Raft-based Produce Path
        // Check if this partition is Raft-enabled FIRST, before writing to WAL directly
        #[cfg(feature = "raft")]
        {
            if let Some(ref raft_manager) = self.raft_manager {
                if raft_manager.has_replica(topic, partition) {
                    // LEADER CHECK: Only the leader can accept produce requests
                    if !raft_manager.is_leader(topic, partition) {
                        // This node is not the leader - return error with leader info
                        let leader_id = raft_manager.get_leader(topic, partition);
                        warn!(
                            "NOT_LEADER: {}-{} leader_id={:?}, this node cannot accept produce requests",
                            topic, partition, leader_id
                        );

                        // Drop memory permit before returning error
                        drop(memory_permit);

                        // Return LEADER_NOT_AVAILABLE error (Kafka standard for non-leader)
                        return Ok(ProduceResponsePartition {
                            index: partition,
                            error_code: chronik_protocol::error_codes::LEADER_NOT_AVAILABLE,
                            base_offset: -1,
                            log_append_time: -1,
                            log_start_offset: -1,
                        });
                    }

                    info!("Raft-enabled partition {}-{}: Proposing write to Raft consensus (leader)", topic, partition);

                    // Serialize the canonical record for Raft proposal
                    use chronik_storage::canonical_record::CanonicalRecord;
                    match CanonicalRecord::from_kafka_batch(&re_encoded_bytes) {
                        Ok(mut canonical_record) => {
                            // Preserve original wire bytes for CRC validation
                            canonical_record.compressed_records_wire_bytes = Some(re_encoded_bytes.to_vec());

                            match bincode::serialize(&canonical_record) {
                                Ok(serialized) => {
                                    // CRITICAL (v2.0.0 Phase 3): Log before proposing to Raft
                                    info!(
                                        "PRODUCE: Proposing {} bytes to Raft for {}-{}, base_offset={}, num_records={}",
                                        serialized.len(), topic, partition, canonical_record.base_offset, canonical_record.records.len()
                                    );

                                    if serialized.is_empty() {
                                        error!("PRODUCE: ❌ EMPTY serialized bytes before propose! This is a BUG in {}-{}", topic, partition);
                                    }

                                    // Propose to Raft (will block until committed by quorum)
                                    // The state machine will handle WAL writes and storage
                                    match raft_manager.propose(topic, partition, serialized.clone()).await {
                                        Ok(raft_index) => {
                                            info!("Raft✓ {}-{}: Committed at index={}, offsets={}-{}",
                                                topic, partition, raft_index, base_offset, last_offset);

                                            // Update metrics
                                            self.metrics.records_produced.fetch_add(records.len() as u64, Ordering::Relaxed);
                                            self.metrics.bytes_produced.fetch_add(total_bytes, Ordering::Relaxed);

                                            // Update high watermark after successful Raft commit
                                            partition_state.high_watermark.store((last_offset + 1) as u64, Ordering::SeqCst);

                                            // Send to indexing pipeline if enabled
                                            if self.config.enable_indexing {
                                                self.send_to_indexer(topic, partition, &records).await;
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

                                            // Return success immediately - Raft has replicated the data
                                            return Ok(ProduceResponsePartition {
                                                index: partition,
                                                error_code: ErrorCode::None.code(),
                                                base_offset: base_offset as i64,
                                                log_append_time: if (kafka_batch.header.attributes >> 3) & 0b111 == TimestampType::LogAppendTime as u16 {
                                                    kafka_batch.header.max_timestamp
                                                } else {
                                                    -1
                                                },
                                                log_start_offset,
                                            });
                                        }
                                        Err(e) => {
                                            error!("Raft proposal failed for {}-{}: {}", topic, partition, e);
                                            return Err(Error::Internal(format!("Raft consensus failed: {}", e)));
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to serialize for Raft {}-{}: {}", topic, partition, e);
                                    return Err(Error::Internal(format!("Raft serialization failed: {}", e)));
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse batch for Raft {}-{}: {}", topic, partition, e);
                            return Err(Error::Protocol(format!("Invalid batch for Raft: {}", e)));
                        }
                    }
                }
            }
        }

        // NON-RAFT PATH: Direct WAL write for partitions not managed by Raft
        // This is the fallback for standalone mode or non-Raft partitions
        warn!("DEBUG_TRACE: Using non-Raft path for {}-{}", topic, partition);

        // CRITICAL (v1.3.47+): Write to WAL BEFORE updating high watermark
        // This ensures durability guarantee - data is persisted before acknowledgment
        if self.wal_manager.is_none() {
            warn!("WAL manager is None - WAL writes DISABLED! Data will NOT be durable!");
        }
        if let Some(ref wal_mgr) = self.wal_manager {
            use chronik_storage::canonical_record::CanonicalRecord;

            // Convert to CanonicalRecord and serialize
            match CanonicalRecord::from_kafka_batch(&re_encoded_bytes) {
                Ok(mut canonical_record) => {
                    // Preserve original wire bytes for byte-perfect CRC
                    canonical_record.compressed_records_wire_bytes = Some(re_encoded_bytes.to_vec());

                    match bincode::serialize(&canonical_record) {
                        Ok(serialized) => {
                            // v1.3.52+: Group commit with acks parameter
                            // - acks=0: Fire-and-forget (buffered, ~50ms flush window)
                            // - acks=1/−1: Immediate fsync (zero data loss guarantee)
                            if let Err(e) = wal_mgr.append_canonical_with_acks(
                                topic.to_string(),
                                partition,
                                serialized,
                                base_offset as i64,
                                last_offset as i64,
                                records.len() as i32,
                                acks
                            ).await {
                                error!("WAL WRITE FAILED: topic={} partition={} acks={} error={}",
                                       topic, partition, acks, e);
                                return Err(Error::Internal(format!("WAL write failed: {}", e)));
                            }
                            warn!("WAL✓ {}-{}: {} bytes, {} records (offsets {}-{}), acks={}",
                                topic, partition, re_encoded_bytes.len(), records.len(),
                                base_offset, last_offset, acks);
                        }
                        Err(e) => {
                            error!("WAL SERIALIZATION FAILED: topic={} partition={} error={}", topic, partition, e);
                            return Err(Error::Internal(format!("Serialization failed: {}", e)));
                        }
                    }
                }
                Err(e) => {
                    error!("WAL PARSE FAILED: topic={} partition={} error={}", topic, partition, e);
                    return Err(Error::Protocol(format!("Invalid Kafka batch: {}", e)));
                }
            }
        }

        // Buffer the batch for non-Raft partitions
        #[cfg(feature = "raft")]
        {
            if let Some(ref raft_manager) = self.raft_manager {
                if !raft_manager.has_replica(topic, partition) {
                    debug!("Partition {}-{} not Raft-enabled, using normal buffering", topic, partition);
                    let mut pending = partition_state.pending_batches.lock().await;
                    pending.push(BufferedBatch {
                        raw_bytes: re_encoded_bytes.to_vec(),
                        records: records.clone(),
                        base_offset: base_offset as i64,
                    });
                }
            } else {
                // No Raft manager, use normal buffering
                let mut pending = partition_state.pending_batches.lock().await;
                pending.push(BufferedBatch {
                    raw_bytes: re_encoded_bytes.to_vec(),
                    records: records.clone(),
                    base_offset: base_offset as i64,
                });
            }
        }

        #[cfg(not(feature = "raft"))]
        {
            // Buffer the batch with re-encoded wire format (correct CRC)
            let mut pending = partition_state.pending_batches.lock().await;
            pending.push(BufferedBatch {
                raw_bytes: re_encoded_bytes.to_vec(),
                records: records.clone(),
                base_offset: base_offset as i64,
            });
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
                
                // Update high watermark for acks=0
                partition_state.high_watermark.store((last_offset + 1) as u64, Ordering::SeqCst);
            }
            1 => {
                // For acks=1, we don't need to flush immediately
                // The background task will handle flushing based on time/size thresholds
                // We just need to ensure the data is buffered
                debug!("Acks=1: Data buffered, will be persisted by background task");
                
                // Update high watermark for acks=1
                partition_state.high_watermark.store((last_offset + 1) as u64, Ordering::SeqCst);
            }
            -1 => {
                // Wait for replication to all ISR - this requires immediate flush
                self.flush_partition_if_needed(topic, partition, &partition_state).await?;
                
                if let Some(ref sender) = self.replication_sender {
                    let (resp_tx, mut resp_rx) = mpsc::channel(1);
                    
                    let replication_req = ReplicationRequest {
                        topic: topic.to_string(),
                        partition,
                        offset: last_offset,
                        data: Bytes::copy_from_slice(&re_encoded_bytes),
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
        
        // Update fetch handler buffer with RAW batch bytes (v1.3.32 CRC FIX)
        // CRITICAL: Store original wire-format bytes to preserve CRC
        if let Some(ref fetch_handler) = self.fetch_handler {
            let high_watermark = partition_state.high_watermark.load(Ordering::Relaxed) as i64;
            let record_count = records.len() as i32;

            tracing::info!("PRODUCE→BUFFER: Storing raw batch for {}-{}, base_offset={}, last_offset={}, record_count={}, high_watermark={}",
                topic, partition, base_offset, last_offset, record_count, high_watermark);

            if let Err(e) = fetch_handler.update_buffer_with_raw_batch(
                topic,
                partition,
                &re_encoded_bytes,  // Re-encoded bytes with correct CRC!
                base_offset as i64,
                last_offset,
                record_count,
                high_watermark
            ).await {
                warn!("Failed to update fetch handler buffer: {:?}", e);
            } else {
                tracing::info!("PRODUCE→BUFFER: Successfully stored raw batch for {}-{}", topic, partition);
            }
        }
        
        // Flush immediately to ensure data is available for fetch
        // Note: We don't force rotation here - let the background task handle rotation
        // based on time/size thresholds to avoid infinite rotation loops
        if let Err(e) = self.flush_partition_if_needed(topic, partition, &partition_state).await {
            warn!("Failed to flush partition {}-{}: {:?}", topic, partition, e);
        }
        
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
        
        // Get current offsets from metadata store first
        let (high_watermark, log_start_offset) = match self.metadata_store
            .get_partition_offset(topic, partition as u32)
            .await? 
        {
            Some((hw, lso)) => (hw, lso),
            None => {
                // Fall back to calculating from segments
                let segments = self.metadata_store.list_segments(topic, Some(partition as u32)).await?;
                let hw = segments.iter()
                    .map(|s| s.end_offset + 1)
                    .max()
                    .unwrap_or(0);
                let lso = segments.iter()
                    .map(|s| s.start_offset)
                    .min()
                    .unwrap_or(0);
                
                // Persist the calculated offsets
                if let Err(e) = self.metadata_store.update_partition_offset(
                    topic, 
                    partition as u32, 
                    hw, 
                    lso
                ).await {
                    warn!("Failed to persist initial partition offsets: {:?}", e);
                }
                
                (hw, lso)
            }
        };
        
        let now = Instant::now();
        let state = Arc::new(PartitionState {
            next_offset: AtomicU64::new(high_watermark as u64),
            high_watermark: AtomicU64::new(high_watermark as u64),
            log_start_offset: AtomicU64::new(log_start_offset as u64),
            current_writer: writer,
            segment_created: AtomicU64::new(0),
            start_time: now,
            segment_size: AtomicU64::new(0),
            pending_batches: Arc::new(Mutex::new(Vec::new())),
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
    async fn send_to_indexer(&self, topic: &str, partition: i32, records: &[ProduceRecord]) {
        if let Some(sender) = &self.index_sender {
            // Convert ProduceRecords to JsonDocuments
            for record in records {
                // Create metadata for the document
                let mut metadata = serde_json::Map::new();
                metadata.insert("_topic".to_string(), serde_json::Value::String(topic.to_string()));
                metadata.insert("_partition".to_string(), serde_json::Value::Number(partition.into()));
                metadata.insert("_offset".to_string(), serde_json::Value::Number(record.offset.into()));
                metadata.insert("_timestamp".to_string(), serde_json::Value::Number(record.timestamp.into()));
                
                // Add headers as metadata
                for (key, value) in &record.headers {
                    if let Ok(header_str) = String::from_utf8(value.clone()) {
                        metadata.insert(format!("_header_{}", key), serde_json::Value::String(header_str));
                    }
                }
                
                // Try to parse value as JSON, fallback to string
                let document = if let Ok(json_value) = serde_json::from_slice::<serde_json::Value>(&record.value) {
                    // Merge JSON value with metadata
                    if let serde_json::Value::Object(mut obj) = json_value {
                        for (k, v) in &metadata {
                            obj.insert(k.clone(), v.clone());
                        }
                        JsonDocument {
                            id: format!("{}-{}-{}", topic, partition, record.offset),
                            topic: topic.to_string(),
                            partition,
                            offset: record.offset,
                            timestamp: record.timestamp,
                            content: serde_json::Value::Object(obj),
                            metadata: Some(metadata),
                        }
                    } else {
                        // Non-object JSON, wrap it
                        let mut obj = metadata.clone();
                        obj.insert("_value".to_string(), json_value);
                        JsonDocument {
                            id: format!("{}-{}-{}", topic, partition, record.offset),
                            topic: topic.to_string(),
                            partition,
                            offset: record.offset,
                            timestamp: record.timestamp,
                            content: serde_json::Value::Object(obj),
                            metadata: Some(metadata),
                        }
                    }
                } else {
                    // Not JSON, store as string
                    let mut obj = metadata.clone();
                    if let Ok(value_str) = String::from_utf8(record.value.clone()) {
                        obj.insert("_value".to_string(), serde_json::Value::String(value_str));
                    } else {
                        // Binary data, store as base64
                        use base64::Engine;
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&record.value);
                        obj.insert("_value_base64".to_string(), serde_json::Value::String(encoded));
                    }
                    
                    JsonDocument {
                        id: format!("{}-{}-{}", topic, partition, record.offset),
                        topic: topic.to_string(),
                        partition,
                        offset: record.offset,
                        timestamp: record.timestamp,
                        content: serde_json::Value::Object(obj),
                        metadata: Some(metadata),
                    }
                };
                
                // Send to indexer (non-blocking)
                if let Err(e) = sender.try_send(document) {
                    debug!("Failed to send document to indexer: {}", e);
                    self.metrics.indexing_lag_ms.store(1000, Ordering::Relaxed);
                }
            }
        }
    }
    
    /// Flush partition if needed based on size or time
    ///
    /// v1.3.56+: Uses ProduceFlushProfile for configurable flush behavior:
    /// - LowLatency: 1 batch / 10ms (real-time)
    /// - Balanced: 10 batches / 100ms (default)
    /// - HighThroughput: 100 batches / 500ms (bulk)
    async fn flush_partition_if_needed(
        &self,
        topic: &str,
        partition: i32,
        state: &Arc<PartitionState>,
    ) -> Result<()> {
        let should_flush = {
            let pending = state.pending_batches.lock().await;
            let last_flush = state.last_flush.lock().await;

            // Use profile settings for flush thresholds
            let min_batches = if cfg!(debug_assertions) {
                1  // Always flush immediately in debug
            } else {
                self.config.flush_profile.min_batches()
            };

            let linger_time = if cfg!(debug_assertions) {
                Duration::from_millis(10)  // Fast flush in debug
            } else {
                Duration::from_millis(self.config.flush_profile.linger_ms())
            };

            pending.len() >= min_batches ||
            state.segment_size.load(Ordering::Relaxed) >= MAX_SEGMENT_SIZE ||
            last_flush.elapsed() >= linger_time
        };

        if should_flush {
            self.flush_partition(topic, partition, state).await?;
        }

        Ok(())
    }
    
    /// Flush pending records to storage (v1.3.66 - PROPER UNDERSTANDING)
    ///
    /// POST-REFACTOR (v1.3.39+): This is intentionally a NO-OP because data persistence happens via:
    /// 1. Inline WAL writes (handled during handle_produce at line ~1208) - MANDATORY, acks-based
    /// 2. Background indexing (WalIndexer → Tantivy → Object Store)
    ///
    /// pending_batches is for in-memory serving to consumers, NOT for durability.
    /// Durability is guaranteed by inline WAL writes that happen BEFORE this method.
    ///
    /// The data flow is:
    /// - Producer sends batch
    /// - handle_produce() writes to WAL inline (line ~1208, with acks parameter)
    /// - handle_produce() buffers to pending_batches for fast consumer reads
    /// - Consumer reads from pending_batches (fast path) or WAL (slower path)
    /// - WalIndexer periodically indexes WAL to Tantivy
    ///
    /// During shutdown:
    /// - Inline WAL writes have already persisted all data
    /// - pending_batches can be safely discarded (data is in WAL)
    /// - No additional flush needed
    async fn flush_partition(
        &self,
        topic: &str,
        partition: i32,
        state: &Arc<PartitionState>,
    ) -> Result<()> {
        // ARCHITECTURAL NOTE (v1.3.64):
        // Data flow: Produce → WAL (GroupCommitWal V2) → WalIndexer → S3 raw segments
        //
        // The inline WAL writes (in WalProduceHandler) have ALREADY persisted all data.
        // pending_batches are only for in-memory serving (Phase 1 buffer fetch).
        //
        // WalIndexer runs periodically (every 30s) to:
        // 1. Read sealed WAL segments
        // 2. Upload bincode Vec<CanonicalRecord> to S3 as raw segments
        // 3. Create Tantivy indexes for searchability
        //
        // Therefore, flush_partition() should simply clear pending_batches since:
        // - Data is already durable (in WAL)
        // - Segment creation is WalIndexer's responsibility
        // - No need to write to SegmentWriter (wrong format anyway)

        // Take all pending batches (discard them, data is in WAL)
        let batches = {
            let mut pending = state.pending_batches.lock().await;
            std::mem::take(&mut *pending)
        };

        if batches.is_empty() {
            trace!("No pending batches to flush for {}-{}", topic, partition);
            return Ok(());
        }

        info!(
            "Flushed {} batches from memory for {}-{} (data already in WAL, WalIndexer will upload to S3)",
            batches.len(), topic, partition
        );

        // Update last flush time
        {
            let mut last_flush = state.last_flush.lock().await;
            *last_flush = Instant::now();
        }

        Ok(())
    }
    
    /// Force flush all partitions (useful for testing)
    pub async fn flush_all_partitions(&self) -> Result<()> {
        let states = self.partition_states.read().await;
        
        for ((topic, partition), state) in states.iter() {
            self.flush_partition(topic, *partition, state).await?;
        }
        
        Ok(())
    }
    
    /// Force immediate rotation of a partition's segment
    async fn force_rotate_partition(
        &self,
        topic: &str,
        partition: i32,
        state: &Arc<PartitionState>,
    ) -> Result<()> {
        // Flush current segment immediately
        self.flush_partition(topic, partition, state).await?;
        
        // Force rotation by creating a new segment writer
        let segment_path = format!("{}/{}/partition-{}", 
            self.config.storage_config.segment_writer_config.data_dir.to_string_lossy(), 
            topic, 
            partition
        );
        
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
        
        debug!("Force rotated segment for {}-{}", topic, partition);
        
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
    
    /// Auto-create a topic with default settings
    ///
    /// This method creates a topic and automatically creates Raft replicas if Raft is enabled.
    /// Should be called by both ProduceHandler internally and by KafkaProtocolHandler.
    pub async fn auto_create_topic(&self, topic_name: &str) -> Result<chronik_common::metadata::TopicMetadata> {
        let start_time = Instant::now();
        
        // Validate topic name according to Kafka rules
        if !Self::is_valid_topic_name(topic_name) {
            self.metrics.topic_creation_invalid_names.fetch_add(1, Ordering::Relaxed);
            warn!("Rejected auto-creation of topic with invalid name: '{}'", topic_name);
            return Err(Error::Protocol(format!("Invalid topic name: '{}'. Topic names must be 1-249 characters, containing only letters, numbers, dots, hyphens, and underscores", topic_name)));
        }
        
        // Check topic creation policy (reserved names)
        if Self::is_reserved_topic_name(topic_name) {
            self.metrics.topic_creation_invalid_names.fetch_add(1, Ordering::Relaxed);
            warn!("Rejected auto-creation of reserved topic: '{}'", topic_name);
            return Err(Error::Protocol(format!("Cannot auto-create reserved topic: '{}'", topic_name)));
        }
        
        // Check if there's already an in-flight creation request for this topic
        let creation_lock = {
            let cache = self.topic_creation_cache.read().await;
            if let Some(existing_lock) = cache.get(topic_name) {
                Arc::clone(existing_lock)
            } else {
                drop(cache);
                // Need to create a new lock
                let mut cache = self.topic_creation_cache.write().await;
                // Double-check after acquiring write lock
                if let Some(existing_lock) = cache.get(topic_name) {
                    Arc::clone(existing_lock)
                } else {
                    let new_lock = Arc::new(Mutex::new(None));
                    cache.insert(topic_name.to_string(), Arc::clone(&new_lock));
                    new_lock
                }
            }
        };
        
        // Try to acquire the creation lock for this topic
        let mut creation_result = creation_lock.lock().await;
        
        // If another thread already completed the creation, return that result
        if let Some(ref metadata) = *creation_result {
            self.metrics.topic_creation_concurrent_attempts.fetch_add(1, Ordering::Relaxed);
            info!("Topic '{}' creation already completed by another thread", topic_name);
            return Ok(metadata.clone());
        }
        
        // We are the first to attempt creation for this topic
        info!("Starting auto-creation of topic '{}' with {} partitions and replication factor {}", 
            topic_name, self.config.num_partitions, self.config.default_replication_factor);
        
        // Create topic configuration with defaults
        let topic_config = chronik_common::metadata::TopicConfig {
            partition_count: self.config.num_partitions,
            replication_factor: self.config.default_replication_factor,
            retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
            segment_bytes: 1024 * 1024 * 1024, // 1GB
            config: std::collections::HashMap::new(),
        };
        
        // Attempt to create the topic
        let result = match self.metadata_store.create_topic(topic_name, topic_config).await {
            Ok(metadata) => {
                // Create partition assignments
                // For clustered mode, use round-robin assignment across nodes
                // For standalone mode, assign all partitions to this node
                #[cfg(feature = "raft")]
                if let Some(ref raft_manager) = self.raft_manager {
                    if raft_manager.is_enabled() {
                        // Clustered mode: Use round-robin assignment
                        use chronik_common::partition_assignment::PartitionAssignment as AssignmentManager;

                        let peers = raft_manager.get_peers().await;
                        info!("Auto-creating topic '{}' in cluster mode with {} peers", topic_name, peers.len());

                        // Convert peer IDs from u64 to u32 for partition_assignment module
                        let peer_ids: Vec<u32> = peers.iter().map(|&id| id as u32).collect();

                        let mut assignment_mgr = AssignmentManager::new();
                        if let Err(e) = assignment_mgr.add_topic(
                            topic_name,
                            self.config.num_partitions as i32,
                            self.config.default_replication_factor.min(peers.len() as u32) as i32,
                            &peer_ids,
                        ) {
                            warn!("Failed to create partition assignment plan for topic '{}': {:?}", topic_name, e);
                        } else {
                            // Persist assignments via metadata store
                            let topic_assignments = assignment_mgr.get_topic_assignments(topic_name);
                            for (partition_id, partition_info) in topic_assignments {
                                for (replica_idx, &node_id) in partition_info.replicas.iter().enumerate() {
                                    let is_leader = node_id == partition_info.leader;
                                    let assignment = chronik_common::metadata::PartitionAssignment {
                                        topic: topic_name.to_string(),
                                        partition: partition_id as u32,
                                        broker_id: node_id as i32,
                                        is_leader,
                                    };

                                    if let Err(e) = self.metadata_store.assign_partition(assignment).await {
                                        warn!("Failed to assign partition {} for topic '{}' to node {}: {:?}",
                                            partition_id, topic_name, node_id, e);
                                    } else if is_leader {
                                        info!("Assigned partition {}/{} to node {} (leader)",
                                            topic_name, partition_id, node_id);
                                    }
                                }
                            }
                        }
                    } else {
                        // Standalone mode with Raft disabled
                        for partition in 0..self.config.num_partitions {
                            let assignment = chronik_common::metadata::PartitionAssignment {
                                topic: topic_name.to_string(),
                                partition,
                                broker_id: self.config.node_id,
                                is_leader: true,
                            };

                            if let Err(e) = self.metadata_store.assign_partition(assignment).await {
                                warn!("Failed to assign partition {} for topic '{}': {:?}",
                                    partition, topic_name, e);
                            }
                        }
                    }
                } else {
                    // No Raft feature or no manager: standalone mode
                    for partition in 0..self.config.num_partitions {
                        let assignment = chronik_common::metadata::PartitionAssignment {
                            topic: topic_name.to_string(),
                            partition,
                            broker_id: self.config.node_id,
                            is_leader: true,
                        };

                        if let Err(e) = self.metadata_store.assign_partition(assignment).await {
                            warn!("Failed to assign partition {} for topic '{}': {:?}",
                                partition, topic_name, e);
                        }
                    }
                }

                // Non-Raft builds always use standalone assignment
                #[cfg(not(feature = "raft"))]
                {
                    for partition in 0..self.config.num_partitions {
                        let assignment = chronik_common::metadata::PartitionAssignment {
                            topic: topic_name.to_string(),
                            partition,
                            broker_id: self.config.node_id,
                            is_leader: true,
                        };

                        if let Err(e) = self.metadata_store.assign_partition(assignment).await {
                            warn!("Failed to assign partition {} for topic '{}': {:?}",
                                partition, topic_name, e);
                        }
                    }
                }

                // Create Raft replicas for each partition if Raft is enabled
                #[cfg(feature = "raft")]
                if let Some(ref raft_manager) = self.raft_manager {
                    if raft_manager.is_enabled() {
                        use chronik_storage::SegmentWriterConfig;

                        // Get peer list from RaftReplicaManager
                        let peers = raft_manager.get_peers().await;
                        info!("Creating Raft replicas for topic '{}' with {} partitions, peer list: {:?}",
                            topic_name, self.config.num_partitions, peers);

                        for partition in 0..self.config.num_partitions {
                            // Create Raft log storage for this partition
                            let log_storage = match crate::raft_integration::create_raft_log_storage(
                                &self.config.storage_config.segment_writer_config.data_dir,
                                topic_name,
                                partition as i32,
                            ).await {
                                Ok(storage) => storage,
                                Err(e) => {
                                    warn!("Failed to create Raft log storage for {}-{}: {:?}",
                                        topic_name, partition, e);
                                    continue;
                                }
                            };

                            // Create the Raft replica with cluster peers
                            // Note: ChronikStateMachine now writes directly to WAL (shared with ProduceHandler)
                            if let Err(e) = raft_manager.create_replica(
                                topic_name.to_string(),
                                partition as i32,
                                log_storage,
                                peers.clone(),
                            ).await {
                                warn!("Failed to create Raft replica for {}-{}: {:?}",
                                    topic_name, partition, e);
                            } else {
                                info!("✅ Created Raft replica for {}-{} with {} peers",
                                    topic_name, partition, peers.len());

                                // CRITICAL: Add each peer via ConfChange to initialize Raft's Progress tracker
                                // Wait a bit for the replica to become leader first
                                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                                // Include all peers (including self) - Raft will handle the cluster setup
                                for &peer_id in &peers {
                                    match raft_manager.add_peer_to_replica(
                                        topic_name,
                                        partition as i32,
                                        peer_id,
                                    ).await {
                                        Ok(_) => {
                                            info!("✅ Added peer {} to replica {}-{} via ConfChange",
                                                peer_id, topic_name, partition);
                                        }
                                        Err(e) => {
                                            // Log as debug if not leader yet (expected during startup)
                                            if e.to_string().contains("not leader") {
                                                debug!("Not leader for {}-{} yet, will retry peer addition later: {:?}",
                                                    topic_name, partition, e);
                                            } else {
                                                warn!("Failed to add peer {} to replica {}-{}: {:?}",
                                                    peer_id, topic_name, partition, e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        debug!("Raft is disabled, skipping replica creation for topic '{}'", topic_name);
                    }
                }

                let elapsed = start_time.elapsed();
                self.metrics.topics_auto_created.fetch_add(1, Ordering::Relaxed);
                
                info!("Successfully auto-created topic '{}' with {} partitions and replication factor {} in {:?}", 
                    topic_name, 
                    self.config.num_partitions, 
                    self.config.default_replication_factor,
                    elapsed);
                
                // Store successful result for other waiting threads
                *creation_result = Some(metadata.clone());
                
                // Clean up the cache entry after a delay to prevent memory growth
                let cache = Arc::clone(&self.topic_creation_cache);
                let topic_name_owned = topic_name.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let mut cache = cache.write().await;
                    cache.remove(&topic_name_owned);
                });
                
                Ok(metadata)
            }
            Err(chronik_common::metadata::MetadataError::AlreadyExists(_)) => {
                // Topic was created concurrently by another server/process, try to fetch it
                self.metrics.topic_creation_concurrent_attempts.fetch_add(1, Ordering::Relaxed);
                info!("Topic '{}' already exists (created by another process), fetching metadata", topic_name);
                
                match self.metadata_store.get_topic(topic_name).await? {
                    Some(metadata) => {
                        let elapsed = start_time.elapsed();
                        info!("Successfully fetched existing topic '{}' metadata in {:?}", topic_name, elapsed);
                        
                        // Store successful result for other waiting threads
                        *creation_result = Some(metadata.clone());
                        
                        // Clean up the cache entry after a delay
                        let cache = Arc::clone(&self.topic_creation_cache);
                        let topic_name_owned = topic_name.to_string();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            let mut cache = cache.write().await;
                            cache.remove(&topic_name_owned);
                        });
                        
                        Ok(metadata)
                    }
                    None => {
                        self.metrics.topic_creation_errors.fetch_add(1, Ordering::Relaxed);
                        error!("Topic '{}' reported as existing but cannot be fetched from metadata store", topic_name);
                        Err(Error::Internal(format!("Topic '{}' exists but cannot be fetched", topic_name)))
                    }
                }
            }
            Err(e) => {
                let elapsed = start_time.elapsed();
                self.metrics.topic_creation_errors.fetch_add(1, Ordering::Relaxed);
                error!("Failed to auto-create topic '{}' after {:?}: {:?}", topic_name, elapsed, e);
                
                // On error, we don't store anything - let the next thread retry
                // But we should still clean up the cache entry
                let cache = Arc::clone(&self.topic_creation_cache);
                let topic_name_owned = topic_name.to_string();
                tokio::spawn(async move {
                    // Shorter timeout for errors to allow retries sooner
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let mut cache = cache.write().await;
                    cache.remove(&topic_name_owned);
                });
                
                Err(Error::Internal(format!("Topic auto-creation failed: {}", e)))
            }
        };
        
        result
    }
    
    /// Validate topic name according to Kafka rules
    fn is_valid_topic_name(name: &str) -> bool {
        // Kafka topic naming rules:
        // - Length between 1 and 249 characters
        // - Contain only alphanumeric, '.', '_', and '-'
        // - Cannot be "." or ".."
        
        if name.is_empty() || name.len() > 249 {
            return false;
        }
        
        if name == "." || name == ".." {
            return false;
        }
        
        name.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '-')
    }
    
    /// Check if topic name is reserved
    fn is_reserved_topic_name(name: &str) -> bool {
        // Reserved topic names that should not be auto-created
        matches!(name, 
            "__consumer_offsets" | 
            "__transaction_state" |
            "__cluster_metadata" |
            "__telemetry" |
            "__schemas"
        ) || name.starts_with("__")  // All topics starting with __ are considered internal
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
    
    /// Get topic auto-creation statistics
    pub fn get_topic_creation_stats(&self) -> TopicCreationStats {
        TopicCreationStats {
            topics_created: self.metrics.topics_auto_created.load(Ordering::Relaxed),
            creation_errors: self.metrics.topic_creation_errors.load(Ordering::Relaxed),
            invalid_name_attempts: self.metrics.topic_creation_invalid_names.load(Ordering::Relaxed),
            concurrent_attempts: self.metrics.topic_creation_concurrent_attempts.load(Ordering::Relaxed),
        }
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
    
    /// Get the high watermark for a partition (includes in-memory messages)
    pub async fn get_partition_high_watermark(&self, topic: &str, partition: i32) -> i64 {
        let states = self.partition_states.read().await;
        if let Some(state) = states.get(&(topic.to_string(), partition)) {
            state.high_watermark.load(Ordering::SeqCst) as i64
        } else {
            // If no state exists yet, query metadata store for persisted offsets
            if let Ok(Some((hw, _))) = self.metadata_store.get_partition_offset(topic, partition as u32).await {
                hw
            } else {
                0
            }
        }
    }
}

// Implement Clone for handler (needed for background tasks)
impl Clone for ProduceHandler {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            storage: Arc::clone(&self.storage),
            indexer: self.indexer.clone(),
            json_pipeline: Arc::clone(&self.json_pipeline),
            index_sender: self.index_sender.clone(),
            metadata_store: Arc::clone(&self.metadata_store),
            partition_states: Arc::clone(&self.partition_states),
            producer_info: Arc::clone(&self.producer_info),
            metrics: Arc::clone(&self.metrics),
            running: Arc::clone(&self.running),
            memory_limiter: Arc::clone(&self.memory_limiter),
            replication_sender: self.replication_sender.clone(),
            fetch_handler: self.fetch_handler.clone(),
            topic_creation_cache: Arc::clone(&self.topic_creation_cache),
            wal_manager: self.wal_manager.clone(),
            #[cfg(feature = "raft")]
            raft_manager: self.raft_manager.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_common::metadata::FileMetadataStore;
    use chronik_storage::object_store::backends::local::LocalBackend;
    use chronik_protocol::types::{ProduceRequestTopic, ProduceRequestPartition};
    use tempfile::TempDir;
    use bytes::BufMut;
    
    async fn create_test_handler() -> (ProduceHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage_path = temp_dir.path().join("storage");
        let metadata_path = temp_dir.path().join("metadata");
        
        std::fs::create_dir_all(&storage_path).unwrap();
        std::fs::create_dir_all(&metadata_path).unwrap();
        
        let storage_config = chronik_storage::object_store::ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: storage_path.to_string_lossy().to_string(),
            },
            retry: Default::default(),
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
        
        // Create test topic
        let topic_config = chronik_common::metadata::TopicConfig {
            partition_count: 3,
            replication_factor: 1,
            retention_ms: Some(7 * 24 * 60 * 60 * 1000),
            segment_bytes: 1024 * 1024 * 1024,
            config: std::collections::HashMap::new(),
        };
        metadata_store.create_topic("test-topic", topic_config).await.unwrap();
        
        let config = ProduceHandlerConfig {
            node_id: 0,
            enable_indexing: false, // Disable for tests
            ..Default::default()
        };
        
        let handler = ProduceHandler::new(config, storage, metadata_store).await.unwrap();
        
        (handler, temp_dir)
    }
    
    fn create_test_record_batch(producer_id: i64, sequence: i32, records: Vec<(&str, &str)>) -> Vec<u8> {
        let mut batch = KafkaRecordBatch::new(
            0,                                  // base_offset
            chrono::Utc::now().timestamp_millis(), // base_timestamp
            producer_id,                        // producer_id
            0,                                  // producer_epoch
            sequence,                           // base_sequence
            CompressionType::None,              // compression
            false,                              // is_transactional
        );
        
        for (key, value) in records {
            batch.add_record(
                Some(Bytes::from(key.to_string())),
                Some(Bytes::from(value.to_string())),
                vec![],
                chrono::Utc::now().timestamp_millis(),
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
    
    #[tokio::test]
    async fn test_topic_auto_creation_enabled() {
        let temp_dir = TempDir::new().unwrap();
        let storage_path = temp_dir.path().join("storage");
        std::fs::create_dir_all(&storage_path).unwrap();
        
        let storage_config = chronik_storage::object_store::ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: storage_path.to_string_lossy().to_string(),
            },
            retry: Default::default(),
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
        
        let config = ProduceHandlerConfig {
            node_id: 0,
            enable_indexing: false,
            auto_create_topics_enable: true,
            num_partitions: 5,
            default_replication_factor: 3,
            ..Default::default()
        };
        
        let handler = ProduceHandler::new(config, storage, metadata_store.clone()).await.unwrap();
        
        // Produce to non-existent topic
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
                    name: "auto-created-topic".to_string(),
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
        
        // Should succeed with auto-creation
        assert_eq!(response.topics.len(), 1);
        assert_eq!(response.topics[0].name, "auto-created-topic");
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        
        // Verify topic was created with correct config
        let topic_meta = metadata_store.get_topic("auto-created-topic").await.unwrap().unwrap();
        assert_eq!(topic_meta.config.partition_count, 5);
        assert_eq!(topic_meta.config.replication_factor, 3);
    }
    
    #[tokio::test]
    async fn test_topic_auto_creation_disabled() {
        let temp_dir = TempDir::new().unwrap();
        let storage_path = temp_dir.path().join("storage");
        std::fs::create_dir_all(&storage_path).unwrap();
        
        let storage_config = chronik_storage::object_store::ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: storage_path.to_string_lossy().to_string(),
            },
            retry: Default::default(),
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
        
        let config = ProduceHandlerConfig {
            node_id: 0,
            enable_indexing: false,
            auto_create_topics_enable: false, // Disabled
            ..Default::default()
        };
        
        let handler = ProduceHandler::new(config, storage, metadata_store).await.unwrap();
        
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
                    name: "non-existent-topic".to_string(),
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
        
        // Should fail with UNKNOWN_TOPIC_OR_PARTITION
        assert_eq!(response.topics[0].partitions[0].error_code, ErrorCode::UnknownTopicOrPartition.code());
    }
    
    #[tokio::test]
    async fn test_concurrent_topic_auto_creation() {
        let temp_dir = TempDir::new().unwrap();
        let storage_path = temp_dir.path().join("storage");
        std::fs::create_dir_all(&storage_path).unwrap();
        
        let storage_config = chronik_storage::object_store::ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: storage_path.to_string_lossy().to_string(),
            },
            retry: Default::default(),
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
        
        let config = ProduceHandlerConfig {
            node_id: 0,
            enable_indexing: false,
            auto_create_topics_enable: true,
            num_partitions: 3,
            default_replication_factor: 1,
            ..Default::default()
        };
        
        let handler = Arc::new(ProduceHandler::new(config, storage, metadata_store).await.unwrap());
        
        // Spawn multiple concurrent produce requests to the same non-existent topic
        let mut handles = vec![];
        
        for i in 0..10 {
            let handler_clone = Arc::clone(&handler);
            let handle = tokio::spawn(async move {
                let records_data = create_test_record_batch(
                    1234 + i as i64,
                    0,
                    vec![(format!("key{}", i).as_str(), format!("value{}", i).as_str())],
                );
                
                let request = ProduceRequest {
                    transactional_id: None,
                    acks: 1,
                    timeout_ms: 30000,
                    topics: vec![
                        ProduceRequestTopic {
                            name: "concurrent-topic".to_string(),
                            partitions: vec![
                                ProduceRequestPartition {
                                    index: 0,
                                    records: records_data,
                                },
                            ],
                        },
                    ],
                };
                
                handler_clone.handle_produce(request, i).await
            });
            handles.push(handle);
        }
        
        // Wait for all requests to complete
        let mut success_count = 0;
        let mut error_count = 0;
        
        for handle in handles {
            match handle.await.unwrap() {
                Ok(response) => {
                    if response.topics[0].partitions[0].error_code == 0 {
                        success_count += 1;
                    } else {
                        error_count += 1;
                    }
                }
                Err(_) => {
                    error_count += 1;
                }
            }
        }
        
        // All requests should succeed
        assert_eq!(success_count, 10);
        assert_eq!(error_count, 0);
        
        // Topic should have been created only once
        // Check the cache is properly cleaned up by waiting
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    
    #[tokio::test]
    async fn test_invalid_topic_name_auto_creation() {
        let temp_dir = TempDir::new().unwrap();
        let storage_path = temp_dir.path().join("storage");
        std::fs::create_dir_all(&storage_path).unwrap();
        
        let storage_config = chronik_storage::object_store::ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: storage_path.to_string_lossy().to_string(),
            },
            retry: Default::default(),
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(FileMetadataStore::new(temp_dir.path().join("metadata")).await.unwrap());
        
        let config = ProduceHandlerConfig {
            node_id: 0,
            enable_indexing: false,
            auto_create_topics_enable: true,
            ..Default::default()
        };
        
        let handler = ProduceHandler::new(config, storage, metadata_store).await.unwrap();
        
        // Test various invalid topic names
        let invalid_names = vec![
            "",                    // Empty
            "a".repeat(250),      // Too long
            "topic with spaces",  // Contains spaces
            "topic@name",         // Invalid character
            ".",                  // Reserved
            "..",                 // Reserved
            "__consumer_offsets", // Reserved internal topic
        ];
        
        for invalid_name in invalid_names {
            let records_data = create_test_record_batch(
                1234,
                0,
                vec![("key", "value")],
            );
            
            let request = ProduceRequest {
                transactional_id: None,
                acks: 1,
                timeout_ms: 30000,
                topics: vec![
                    ProduceRequestTopic {
                        name: invalid_name.to_string(),
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
            
            // Should fail with error
            assert_ne!(
                response.topics[0].partitions[0].error_code, 
                0,
                "Topic '{}' should have failed auto-creation",
                invalid_name
            );
        }
    }
    
    #[test]
    fn test_topic_name_validation() {
        // Valid names
        assert!(ProduceHandler::is_valid_topic_name("valid-topic"));
        assert!(ProduceHandler::is_valid_topic_name("valid_topic"));
        assert!(ProduceHandler::is_valid_topic_name("valid.topic"));
        assert!(ProduceHandler::is_valid_topic_name("123"));
        assert!(ProduceHandler::is_valid_topic_name("a"));
        assert!(ProduceHandler::is_valid_topic_name("a".repeat(249).as_str()));
        
        // Invalid names
        assert!(!ProduceHandler::is_valid_topic_name(""));
        assert!(!ProduceHandler::is_valid_topic_name("a".repeat(250).as_str()));
        assert!(!ProduceHandler::is_valid_topic_name("."));
        assert!(!ProduceHandler::is_valid_topic_name(".."));
        assert!(!ProduceHandler::is_valid_topic_name("topic with spaces"));
        assert!(!ProduceHandler::is_valid_topic_name("topic@name"));
        assert!(!ProduceHandler::is_valid_topic_name("topic#name"));
        assert!(!ProduceHandler::is_valid_topic_name("topic/name"));
    }
    
    #[test]
    fn test_reserved_topic_names() {
        assert!(ProduceHandler::is_reserved_topic_name("__consumer_offsets"));
        assert!(ProduceHandler::is_reserved_topic_name("__transaction_state"));
        assert!(ProduceHandler::is_reserved_topic_name("__cluster_metadata"));
        assert!(ProduceHandler::is_reserved_topic_name("__telemetry"));
        assert!(ProduceHandler::is_reserved_topic_name("__schemas"));
        assert!(ProduceHandler::is_reserved_topic_name("__anything"));
        
        assert!(!ProduceHandler::is_reserved_topic_name("_single_underscore"));
        assert!(!ProduceHandler::is_reserved_topic_name("normal_topic"));
        assert!(!ProduceHandler::is_reserved_topic_name("consumer_offsets"));
    }

    #[tokio::test]
    async fn test_high_watermark_update_acks_0() {
        use tempfile::TempDir;
        use std::sync::atomic::Ordering;
        
        let temp_dir = TempDir::new().unwrap();
        let config = ProduceConfig {
            data_dir: temp_dir.path().to_path_buf(),
            flush_interval_ms: 1000,
            flush_bytes: 1024 * 1024,
            ..Default::default()
        };
        
        let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
            temp_dir.path().join("metadata")
        ).await.unwrap());
        
        let object_store = Arc::new(chronik_storage::LocalObjectStore::new(
            temp_dir.path().join("segments")
        ).await.unwrap());
        
        let mut handler = ProduceHandler::new(
            config,
            object_store,
            metadata_store.clone(),
        ).await.unwrap();
        
        // Create topic
        metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
        
        // Produce a message with acks=0
        let request = ProduceRequest {
            transactional_id: None,
            acks: 0, // acks=0
            timeout_ms: 1000,
            topics: vec![ProduceRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![ProduceRequestPartition {
                    index: 0,
                    records: Some(create_test_record_batch(0, vec!["msg1"])),
                }],
            }],
        };
        
        let response = handler.handle_produce(request).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        
        // Check that high watermark was updated for acks=0
        let state = handler.partition_states.get("test-topic").unwrap();
        let partition_state = state.get(&0).unwrap();
        let high_watermark = partition_state.high_watermark.load(Ordering::Relaxed);
        assert_eq!(high_watermark, 1, "High watermark should be updated to 1 for acks=0");
    }

    #[tokio::test]
    async fn test_high_watermark_update_acks_1() {
        use tempfile::TempDir;
        use std::sync::atomic::Ordering;
        
        let temp_dir = TempDir::new().unwrap();
        let config = ProduceConfig {
            data_dir: temp_dir.path().to_path_buf(),
            flush_interval_ms: 1000,
            flush_bytes: 1024 * 1024,
            ..Default::default()
        };
        
        let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
            temp_dir.path().join("metadata")
        ).await.unwrap());
        
        let object_store = Arc::new(chronik_storage::LocalObjectStore::new(
            temp_dir.path().join("segments")
        ).await.unwrap());
        
        let mut handler = ProduceHandler::new(
            config,
            object_store,
            metadata_store.clone(),
        ).await.unwrap();
        
        // Create topic
        metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
        
        // Produce multiple messages with acks=1
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1, // acks=1
            timeout_ms: 1000,
            topics: vec![ProduceRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![ProduceRequestPartition {
                    index: 0,
                    records: Some(create_test_record_batch(0, vec!["msg1", "msg2", "msg3"])),
                }],
            }],
        };
        
        let response = handler.handle_produce(request).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        
        // Check that high watermark was updated for acks=1
        let state = handler.partition_states.get("test-topic").unwrap();
        let partition_state = state.get(&0).unwrap();
        let high_watermark = partition_state.high_watermark.load(Ordering::Relaxed);
        assert_eq!(high_watermark, 3, "High watermark should be updated to 3 for acks=1 with 3 messages");
    }

    #[tokio::test]
    async fn test_high_watermark_with_fetch_handler_integration() {
        use tempfile::TempDir;
        use std::sync::atomic::Ordering;
        
        let temp_dir = TempDir::new().unwrap();
        let config = ProduceConfig {
            data_dir: temp_dir.path().to_path_buf(),
            flush_interval_ms: 1000,
            flush_bytes: 1024 * 1024,
            ..Default::default()
        };
        
        let metadata_store = Arc::new(chronik_common::metadata::file_store::FileMetadataStore::new(
            temp_dir.path().join("metadata")
        ).await.unwrap());
        
        let object_store = Arc::new(chronik_storage::LocalObjectStore::new(
            temp_dir.path().join("segments")
        ).await.unwrap());
        
        // Create FetchHandler
        let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
            object_store.clone()
        ));
        
        let fetch_handler = Arc::new(crate::fetch_handler::FetchHandler::new(
            segment_reader,
            metadata_store.clone(),
            object_store.clone(),
        ));
        
        let mut handler = ProduceHandler::new(
            config,
            object_store.clone(),
            metadata_store.clone(),
        ).await.unwrap();
        
        // Connect fetch handler
        handler.set_fetch_handler(fetch_handler.clone());
        
        // Create topic
        metadata_store.create_topic("test-topic", 1, HashMap::new()).await.unwrap();
        
        // Produce messages
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 1000,
            topics: vec![ProduceRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![ProduceRequestPartition {
                    index: 0,
                    records: Some(create_test_record_batch(0, vec!["msg1", "msg2"])),
                }],
            }],
        };
        
        let response = handler.handle_produce(request).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        
        // Verify high watermark is updated
        let state = handler.partition_states.get("test-topic").unwrap();
        let partition_state = state.get(&0).unwrap();
        let high_watermark = partition_state.high_watermark.load(Ordering::Relaxed);
        assert_eq!(high_watermark, 2);
        
        // Verify fetch handler buffer was updated via fetch_partition
        let fetch_response = fetch_handler.fetch_partition(
            "test-topic",
            0,
            0,     // fetch_offset
            1024,  // max_bytes
            0,     // max_wait_ms
            0,     // min_bytes
        ).await.unwrap();
        
        assert_eq!(fetch_response.high_watermark, 2, "Fetch handler should report correct high watermark");
        assert!(!fetch_response.records.is_empty(), "Fetch handler should return buffered records");
    }

    // Helper function to create test record batches
    fn create_test_record_batch(base_offset: i64, messages: Vec<&str>) -> Vec<u8> {
        use chronik_protocol::kafka_protocol::KafkaRecordBatch;
        use bytes::Bytes;
        
        let mut batch = KafkaRecordBatch::new(
            base_offset,
            0,  // partition_leader_epoch
            2,  // magic
            0,  // compression_type
            chrono::Utc::now().timestamp_millis(),
            -1, // producer_id
            -1, // producer_epoch
            -1, // base_sequence
        );
        
        for (i, msg) in messages.iter().enumerate() {
            batch.add_record(
                (i as i64) * 1000,  // timestamp_delta
                None,               // key
                Some(Bytes::from(msg.to_string())),
                vec![],            // headers
            );
        }
        
        batch.encode()
    }
}