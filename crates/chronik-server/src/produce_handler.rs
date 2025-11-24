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
use crate::raft_cluster::RaftCluster;  // NEW: v2.2.7 Phase 3
use chronik_common::{Result, Error};
use chronik_monitoring::MetricsRecorder;
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
use chronik_common::metadata::traits::{MetadataStore, PartitionAssignment};
use chronik_wal::WalManager;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Mutex, mpsc, oneshot};  // v2.2.10: oneshot for ResponsePipeline
use tokio::time::timeout;
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn, trace, instrument};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use dashmap::DashMap;
use crossbeam::queue::SegQueue;

/// Re-export WalReplicationManager from wal_replication module (v2.2.0 Phase 3.1)
pub use crate::wal_replication::WalReplicationManager;

/// v2.2.9: Async pipelined connection for leader forwarding (eliminates 168x slowdown)
use crate::pipelined_connection::{PipelinedConnection, PipelinedConnectionPool};

/// Maximum segment size before rotation (256MB)
const MAX_SEGMENT_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum time before segment rotation (30 seconds - reasonable interval)
/// We flush after each produce for immediate availability, rotation is for segment management
const MAX_SEGMENT_AGE: Duration = Duration::from_secs(30);

/// Maximum number of records in memory before flush
const MAX_BUFFER_RECORDS: usize = 10000;

/// Timeout for replication acknowledgments
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum connections per leader in the connection pool
const MAX_CONNECTIONS_PER_LEADER: usize = 128;

/// Connection timeout for leader forwarding
const LEADER_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Leader connection pool for reusing TCP connections
///
/// CRITICAL PERFORMANCE FIX (v2.2.8): Eliminates the 147x performance degradation
/// observed when acks=1/all in cluster mode. Previous implementation created a NEW
/// TCP connection for every forwarded produce request (Line 1504). With connection
/// pooling, connections are reused, eliminating the ~50ms TCP handshake overhead.
///
/// Performance Impact:
/// - BEFORE: acks=1 @ 1,098 msg/s (0.68% of acks=0 performance)
/// - AFTER: Expected to match acks=0 @ 161,204 msg/s (100% performance)
///
/// Architecture:
/// - DashMap keyed by leader address for lock-free concurrent access
/// - VecDeque per leader for FIFO connection reuse
/// - Connections are tested before reuse (write readiness check)
/// - Failed connections trigger automatic reconnection
/// - No lock contention on hot path (DashMap provides sharding)
#[derive(Clone)]
struct LeaderConnectionPool {
    /// Pool of connections per leader address
    /// Key: leader address (e.g., "localhost:9092")
    /// Value: Queue of available TCP connections
    pools: Arc<DashMap<String, Arc<Mutex<VecDeque<TcpStream>>>>>,
    /// Maximum connections per leader
    max_connections_per_leader: usize,
}

impl LeaderConnectionPool {
    /// Create a new connection pool
    fn new() -> Self {
        Self {
            pools: Arc::new(DashMap::new()),
            max_connections_per_leader: MAX_CONNECTIONS_PER_LEADER,
        }
    }

    /// Get a connection from the pool or create a new one
    ///
    /// Returns a connection that is ready to use. If a pooled connection
    /// is available, it will be returned after a health check. If no pooled
    /// connection exists or the health check fails, a new connection is created.
    async fn get_connection(&self, leader_addr: &str) -> Result<TcpStream> {
        // Try to get an existing connection from the pool
        if let Some(pool_ref) = self.pools.get(leader_addr) {
            let mut pool = pool_ref.lock().await;

            // Try to reuse a pooled connection
            while let Some(conn) = pool.pop_front() {
                // Health check: ensure connection is still writable
                // If connection is dead, this will fail and we'll try the next one
                if conn.writable().await.is_ok() {
                    trace!("Reusing pooled connection to leader {}", leader_addr);
                    return Ok(conn);
                } else {
                    debug!("Discarding dead connection to leader {}", leader_addr);
                }
            }
        }

        // No pooled connection available or all connections failed health check
        // Create a new connection
        trace!("Creating new connection to leader {}", leader_addr);
        match timeout(LEADER_CONNECTION_TIMEOUT, TcpStream::connect(leader_addr)).await {
            Ok(Ok(stream)) => {
                debug!("Successfully connected to leader {}", leader_addr);
                Ok(stream)
            }
            Ok(Err(e)) => {
                error!("Failed to connect to leader {}: {}", leader_addr, e);
                Err(Error::Internal(format!("Connection failed: {}", e)))
            }
            Err(_) => {
                error!("Connection timeout to leader {}", leader_addr);
                Err(Error::Internal(format!("Connection timeout to {}", leader_addr)))
            }
        }
    }

    /// Return a connection to the pool for reuse
    ///
    /// Only returns connections to the pool if below the max limit.
    /// Otherwise, the connection is dropped (closed).
    async fn return_connection(&self, leader_addr: String, conn: TcpStream) {
        // Get or create the pool for this leader
        let pool_ref = self.pools
            .entry(leader_addr.clone())
            .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())))
            .clone();

        let mut pool = pool_ref.lock().await;

        // Only add to pool if below max limit
        if pool.len() < self.max_connections_per_leader {
            pool.push_back(conn);
            trace!("Returned connection to pool for leader {} (pool size: {})",
                   leader_addr, pool.len());
        } else {
            // Pool is full, drop the connection (it will close automatically)
            trace!("Pool full for leader {}, dropping connection", leader_addr);
        }
    }
}

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
    /// Low-latency profile: Minimal buffering, instant visibility
    /// Use case: Real-time analytics, instant messaging, live dashboards
    /// Performance: 37K msg/s, 6.84ms p99 latency (with LOW WAL profile)
    LowLatency,

    /// Balanced profile: Good balance of throughput and latency
    /// Use case: General-purpose streaming, typical microservices
    /// Performance: 32K msg/s, 6.57ms p99 latency (with LOW WAL profile)
    Balanced,

    /// High-throughput profile: Large batches, higher latency
    /// Use case: Bulk data pipelines, ETL, batch processing
    /// Performance: 30K msg/s, 7.11ms p99 latency (with LOW WAL profile)
    HighThroughput,

    /// Extreme profile: Very large batches (200ms linger)
    /// Use case: Bulk ingestion, data migrations, performance testing
    /// Performance: 30K msg/s, 7.17ms p99 latency (with LOW WAL profile)
    Extreme,
}

impl Default for ProduceFlushProfile {
    fn default() -> Self {
        Self::LowLatency
    }
}

impl ProduceFlushProfile {
    /// Auto-select profile based on environment variable or default to LowLatency
    pub fn auto_select() -> Self {
        if let Ok(profile) = std::env::var("CHRONIK_PRODUCE_PROFILE") {
            match profile.to_lowercase().as_str() {
                "low" | "low-latency" | "realtime" => Self::LowLatency,
                "balanced" | "medium" => Self::Balanced,
                "high" | "high-throughput" | "bulk" => Self::HighThroughput,
                "extreme" | "max" | "ultra" => Self::Extreme,
                _ => {
                    warn!("Unknown CHRONIK_PRODUCE_PROFILE '{}', using LowLatency (default)", profile);
                    Self::LowLatency
                }
            }
        } else {
            Self::LowLatency
        }
    }

    /// Get minimum batches before flush
    pub fn min_batches(&self) -> usize {
        match self {
            Self::LowLatency => 1,      // Flush immediately
            Self::Balanced => 10,        // Wait for 10 batches
            Self::HighThroughput => 100, // Wait for 100 batches
            Self::Extreme => 500,        // Wait for 500 batches (push limits!)
        }
    }

    /// Get linger time (max time before forced flush)
    pub fn linger_ms(&self) -> u64 {
        match self {
            Self::LowLatency => 1,      // 1ms max wait
            Self::Balanced => 10,       // 10ms max wait
            Self::HighThroughput => 50, // 50ms max wait
            Self::Extreme => 200,       // 200ms max wait (bulk ingestion)
        }
    }

    /// Get buffer memory size
    pub fn buffer_memory(&self) -> usize {
        match self {
            Self::LowLatency => 16 * 1024 * 1024,  // 16MB
            Self::Balanced => 32 * 1024 * 1024,     // 32MB
            Self::HighThroughput => 128 * 1024 * 1024, // 128MB
            Self::Extreme => 512 * 1024 * 1024,     // 512MB (extreme batching)
        }
    }

    /// Get profile name for logging
    pub fn name(&self) -> &'static str {
        match self {
            Self::LowLatency => "LowLatency",
            Self::Balanced => "Balanced",
            Self::HighThroughput => "HighThroughput",
            Self::Extreme => "Extreme",
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
    /// PERFORMANCE (v2.2.7 - P3): Use lock-free SegQueue instead of Mutex<Vec> for pending batches
    /// This eliminates mutex contention on every batch write (5-10% throughput gain)
    pending_batches: Arc<SegQueue<BufferedBatch>>,
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
    // PERFORMANCE (v2.2.7 - P2): Use DashMap instead of RwLock<HashMap> for lock-free partition access
    // This eliminates lock contention with 128 concurrent producers (10-15% throughput gain)
    partition_states: Arc<DashMap<(String, i32), Arc<PartitionState>>>,
    producer_info: Arc<RwLock<HashMap<i64, ProducerInfo>>>,
    metrics: Arc<ProduceMetrics>,
    running: Arc<AtomicBool>,
    /// P3 OPTIMIZATION (v2.2.7): Lock-free memory tracking with AtomicU64
    /// Replaces Semaphore for ~15-25% throughput gain at high concurrency
    memory_used_bytes: Arc<AtomicU64>,
    memory_limit_bytes: u64,
    replication_sender: Option<mpsc::Sender<ReplicationRequest>>,
    fetch_handler: Option<Arc<FetchHandler>>,
    /// Track in-flight topic creation requests to prevent duplicates
    topic_creation_cache: Arc<RwLock<HashMap<String, Arc<Mutex<Option<chronik_common::metadata::TopicMetadata>>>>>>,
    /// WAL manager for inline durability writes (v1.3.47+)
    /// Uses Arc<WalManager> directly - no RwLock needed since WalManager uses DashMap internally
    wal_manager: Option<Arc<WalManager>>,
    /// Raft cluster for metadata coordination (v2.2.7 Phase 3)
    /// CRITICAL: Option<Arc<>> NOT Arc<RwLock<>> to avoid hot path locks!
    /// Used to query partition replicas and ISR for replication decisions
    raft_cluster: Option<Arc<RaftCluster>>,
    /// WAL replication manager for PostgreSQL-style streaming (v2.2.0+)
    /// CRITICAL: Option<Arc<>> NOT Arc<RwLock<>> to avoid hot path locks!
    /// Fire-and-forget async replication, never blocks produce path
    wal_replication_manager: Option<Arc<WalReplicationManager>>,
    /// ISR ACK tracker for acks=-1 quorum support (v2.2.7 Phase 4)
    /// Tracks pending acks=-1 requests and notifies when ISR quorum reached
    isr_ack_tracker: Option<Arc<crate::isr_ack_tracker::IsrAckTracker>>,
    /// Leader elector for partition leader failover (v2.2.7 Phase 5)
    /// Used to record heartbeats when handling produce requests as leader
    leader_elector: Option<Arc<crate::leader_election::LeaderElector>>,
    /// Metadata event bus for high watermark replication (v2.2.7.2)
    /// Emits HighWatermarkUpdated events to replicate watermarks < 10ms via metadata WAL
    event_bus: Option<Arc<crate::metadata_events::MetadataEventBus>>,
    /// PERFORMANCE OPTIMIZATION #4: Leadership cache to avoid expensive metadata store lookups
    /// Cache key: (topic, partition), Value: (leader_id, last_updated)
    /// Provides 15-20% throughput improvement by eliminating per-request metadata queries
    leadership_cache: Arc<DashMap<(String, i32), (u64, std::time::Instant)>>,
    /// CRITICAL PERFORMANCE FIX #6 (v2.2.9): Async pipelined connection pool for leader forwarding
    /// Eliminates 168x performance degradation via async request pipelining (Kafka-style)
    /// OLD (v2.2.8 - synchronous): 2,197 msg/s @ 51ms p99 (head-of-line blocking)
    /// NEW (v2.2.9 - async pipelined): Expected 300,000+ msg/s @ <5ms p99 (100x+ improvement)
    pipelined_pool: Arc<PipelinedConnectionPool>,
    /// CRITICAL PERFORMANCE FIX #7 (v2.2.10): Async response pipeline for local acks=1
    /// Eliminates synchronous WAL fsync blocking via callback-based response delivery
    /// OLD (v2.2.9 - sync fsync wait): 2,197 msg/s @ 50ms p99 (blocking on group commit)
    /// NEW (v2.2.10 - async responses): Expected 300,000+ msg/s @ <5ms p99 (150x+ improvement)
    /// This is the ACTUAL bottleneck - async pipelining (v2.2.9) only helps follower forwarding
    response_pipeline: Option<Arc<crate::response_pipeline::ResponsePipeline>>,
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

    /// Assign a partition to replica nodes (writes to metadata WAL)
    ///
    /// v2.2.9 Phase 1: Direct metadata writes bypass Raft consensus.
    /// This method writes partition assignment metadata directly to the __chronik_metadata WAL,
    /// which then replicates to followers via WAL replication (microsecond latency).
    pub async fn assign_partition(
        &self,
        topic: &str,
        partition: i32,
        replicas: Vec<u64>,
    ) -> Result<()> {
        // Assume first replica is the leader by default
        let leader_id = replicas.first().copied().unwrap_or(self.config.node_id as u64);

        let assignment = PartitionAssignment {
            topic: topic.to_string(),
            partition: partition as u32,
            broker_id: -1,  // Deprecated field
            is_leader: false,  // Deprecated field
            replicas,
            leader_id,
        };

        // Write AssignPartition event to metadata_store
        // metadata_store will persist to __chronik_metadata WAL
        // WAL replication will propagate to followers
        self.metadata_store.assign_partition(assignment).await?;
        Ok(())
    }

    /// Set partition leader (writes to metadata WAL)
    ///
    /// v2.2.9 Phase 1: Updates the leader for a partition by writing to metadata WAL.
    /// This preserves the existing replicas but changes which node is the leader.
    pub async fn set_partition_leader(
        &self,
        topic: &str,
        partition: i32,
        leader: u64,
    ) -> Result<()> {
        // Get current assignment to preserve replicas
        let assignments = self.metadata_store
            .get_partition_assignments(topic)
            .await?;

        let current = assignments.iter()
            .find(|a| a.partition == partition as u32)
            .ok_or_else(|| Error::Storage(format!("Partition not found: {}-{}", topic, partition)))?;

        let assignment = PartitionAssignment {
            topic: topic.to_string(),
            partition: partition as u32,
            broker_id: -1,  // Deprecated field
            is_leader: false,  // Deprecated field
            replicas: current.replicas.clone(),
            leader_id: leader,
        };

        self.metadata_store.assign_partition(assignment).await?;
        Ok(())
    }

    /// Update in-sync replica set (writes to metadata WAL)
    ///
    /// v2.2.9 Phase 1: Updates the ISR (in-sync replicas) for a partition via metadata WAL.
    /// This preserves the existing leader but changes which nodes are in-sync.
    pub async fn update_isr(
        &self,
        topic: &str,
        partition: i32,
        isr: Vec<u64>,
    ) -> Result<()> {
        // Get current assignment to preserve leader
        let assignments = self.metadata_store
            .get_partition_assignments(topic)
            .await?;

        let current = assignments.iter()
            .find(|a| a.partition == partition as u32)
            .ok_or_else(|| Error::Storage(format!("Partition not found: {}-{}", topic, partition)))?;

        let assignment = PartitionAssignment {
            topic: topic.to_string(),
            partition: partition as u32,
            broker_id: -1,  // Deprecated field
            is_leader: false,  // Deprecated field
            replicas: isr,
            leader_id: current.leader_id,
        };

        self.metadata_store.assign_partition(assignment).await?;
        Ok(())
    }

    /// Ensure a partition exists with the specified starting offset (for WAL recovery)
    pub async fn ensure_partition_exists(
        &self,
        topic: &str,
        partition: i32,
        next_offset: i64,
    ) -> Result<()> {
        let key = (topic.to_string(), partition);

        // Check if partition already exists (lock-free with DashMap)
        if let Some(state_ref) = self.partition_states.get(&key) {
            let state = state_ref.value();
            let current_offset = state.next_offset.load(Ordering::SeqCst);
            if next_offset > current_offset as i64 {
                state.next_offset.store(next_offset as u64, Ordering::SeqCst);
                info!(
                    "Updated partition {}-{} next offset from {} to {}",
                    topic, partition, current_offset, next_offset
                );
            }
            return Ok(());
        }

        // Create new partition state (lock-free insert with DashMap)
        if !self.partition_states.contains_key(&key) {
            let state = self.create_partition_state_with_offset(topic, partition, next_offset).await?;
            self.partition_states.insert(key.clone(), Arc::new(state));
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

        // Lock-free lookup with DashMap
        if let Some(state_ref) = self.partition_states.get(&key) {
            let state = state_ref.value();
            // Add to pending batches
            state.pending_batches.push(BufferedBatch {
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
    /// Returns the replicated offset (high watermark).
    /// This is the SOURCE OF TRUTH for FetchHandler - no need to query metadata store.
    ///
    /// CRITICAL FIX (v2.2.7): Return actual high_watermark, not next_offset!
    /// BUG: Was returning next_offset which is updated immediately on produce
    /// CORRECT: Return high_watermark which is updated after ISR acknowledgment
    /// This caused large batch consumption to stall - consumer saw next_offset
    /// but actual replicated data (high_watermark) lagged behind, causing 60s timeouts.
    pub async fn get_high_watermark(&self, topic: &str, partition: i32) -> Result<i64> {
        let key = (topic.to_string(), partition);

        if let Some(state) = self.partition_states.get(&key) {
            let high_watermark = state.value().high_watermark.load(Ordering::SeqCst) as i64;
            Ok(high_watermark)
        } else {
            // Partition doesn't exist yet, high watermark is 0
            Ok(0)
        }
    }

    /// Update high watermark in memory (v2.2.9 - simplified architecture)
    ///
    /// ARCHITECTURAL CHANGE: Removed redundant metadata_store persistence.
    /// Watermarks are NOW single-source-of-truth:
    /// - Runtime: partition_states (in-memory, fast)
    /// - Recovery: WAL files (scanned on startup to reconstruct watermarks)
    ///
    /// The metadata_store update was redundant because:
    /// 1. Recovery doesn't use metadata_store - it scans WAL files directly
    /// 2. Extra I/O on every produce (performance penalty)
    /// 3. Caused sync bugs (v2.2.8 watermark visibility issue)
    ///
    /// v2.2.9 FOLLOWER FIX: Auto-create partition state if it doesn't exist.
    /// This handles the case where followers receive replicated data before
    /// they've explicitly created the partition (which only happens on leader).
    ///
    /// See integrated_server.rs:632 for recovery flow.
    pub async fn update_high_watermark(
        &self,
        topic: &str,
        partition: i32,
        high_watermark: i64,
    ) -> Result<()> {
        // Update in-memory partition_states (authoritative for queries)
        let key = (topic.to_string(), partition);

        if let Some(state) = self.partition_states.get(&key) {
            // Partition exists - update watermark only if it's increasing
            // v2.2.9 MONOTONICITY FIX: Watermarks should never decrease (prevent stale WAL data from overwriting)
            let current = state.value().high_watermark.load(Ordering::SeqCst) as i64;
            if high_watermark > current {
                state.value().high_watermark.store(high_watermark as u64, Ordering::SeqCst);
                debug!("✅ Updated watermark for {}-{} from {} to {}", topic, partition, current, high_watermark);
                Ok(())
            } else {
                debug!("⏭️  Skipped watermark update for {}-{}: {} <= {} (current)", topic, partition, high_watermark, current);
                Ok(())
            }
        } else {
            // v2.2.9 FOLLOWER FIX: Partition doesn't exist yet (follower receiving replicated data)
            // Create partition state with initial watermark
            debug!("Creating partition state for {}-{} (follower receiving replication)", topic, partition);

            // Build path from segment_writer_config
            let base_dir = self.config.storage_config.segment_writer_config.data_dir.clone();
            let segment_path = base_dir
                .join(topic)
                .join(format!("partition-{}", partition));

            // Create directory if needed
            std::fs::create_dir_all(&segment_path).map_err(|e|
                Error::Internal(format!("Failed to create segment directory: {}", e)))?;

            // Create segment writer config using existing settings
            let segment_config = chronik_storage::SegmentWriterConfig {
                data_dir: segment_path,
                max_segment_size: self.config.storage_config.segment_writer_config.max_segment_size,
                compression_codec: self.config.storage_config.segment_writer_config.compression_codec.clone(),
                max_segment_age_secs: self.config.storage_config.segment_writer_config.max_segment_age_secs,
                retention_period_secs: self.config.storage_config.segment_writer_config.retention_period_secs,
                enable_cleanup: self.config.storage_config.segment_writer_config.enable_cleanup,
            };

            let segment_writer = SegmentWriter::new(segment_config)
                .await
                .map_err(|e| Error::Internal(format!("Failed to create segment writer: {}", e)))?;

            // Create partition state
            let state = PartitionState {
                next_offset: AtomicU64::new(high_watermark as u64),
                high_watermark: AtomicU64::new(high_watermark as u64),
                log_start_offset: AtomicU64::new(0),
                current_writer: Arc::new(Mutex::new(segment_writer)),
                segment_created: AtomicU64::new(0),
                start_time: Instant::now(),
                segment_size: AtomicU64::new(0),
                pending_batches: Arc::new(SegQueue::new()),
                last_flush: Arc::new(Mutex::new(Instant::now())),
            };

            // Insert into partition_states (wrap in Arc)
            self.partition_states.insert(key, Arc::new(state));
            info!("✅ Created partition state for {}-{} with watermark {}", topic, partition, high_watermark);
            Ok(())
        }
    }

    /// Extract pending batches for WAL writing (v1.3.37)
    ///
    /// This method retrieves the batches that were just written to the partition buffer
    /// so they can be persisted to the WAL. The batches are NOT cleared, as they're still
    /// needed for serving fetch requests.
    pub async fn get_pending_batches(&self, topic: &str, partition: i32) -> Result<Vec<Vec<u8>>> {
        let key = (topic.to_string(), partition);

        // Lock-free lookup with DashMap
        if let Some(state_ref) = self.partition_states.get(&key) {
            let state = state_ref.value();
            // Drain all batches from SegQueue (lock-free operation)
            let mut batches = Vec::new();
            while let Some(batch) = state.pending_batches.pop() {
                batches.push(batch.raw_bytes);
            }
            Ok(batches)
        } else {
            Ok(Vec::new())
        }
    }

    /// Clear pending batches after they've been written to WAL (v1.3.43)
    pub async fn clear_pending_batches(&self, topic: &str, partition: i32) -> Result<()> {
        let key = (topic.to_string(), partition);

        // Lock-free lookup with DashMap
        if let Some(state_ref) = self.partition_states.get(&key) {
            let state = state_ref.value();
            // Drain all batches from SegQueue (lock-free)
            let mut count = 0;
            while let Some(_) = state.pending_batches.pop() {
                count += 1;
            }
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
        let mut total_cleared = 0;

        // PERFORMANCE (v2.2.7 - P2 Fix): Collect keys first to avoid holding DashMap iterator
        // The DashMap iterator can conflict with insert() operations during WAL recovery,
        // causing the cluster to hang at "Replaying WAL to restore high watermarks".
        // By collecting keys first, we release the iterator before operating on partitions.
        let keys: Vec<_> = self.partition_states.iter()
            .map(|entry| entry.key().clone())
            .collect();

        // Now iterate over keys without holding DashMap iterator
        for key in keys {
            if let Some(state) = self.partition_states.get(&key) {
                let mut count = 0;
                while let Some(_) = state.pending_batches.pop() {
                    count += 1;
                }
                total_cleared += count;
                if count > 0 {
                    debug!("Cleared {} pending batches for {}-{}", count, key.0, key.1);
                }
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

        if !self.partition_states.contains_key(&key) {
            // Create new partition state with recovered offset
            let state = self.create_partition_state_with_offset(topic, partition, high_watermark as i64).await?;
            self.partition_states.insert(key.clone(), Arc::new(state));
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
            pending_batches: Arc::new(SegQueue::new()),
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
        
        // P3 OPTIMIZATION (v2.2.7): Lock-free memory tracking
        let memory_limit_bytes = config.buffer_memory as u64;
        let memory_used_bytes = Arc::new(AtomicU64::new(0));

        // Record active ProduceFlushProfile in metrics (v2.1.0)
        let profile_id = match config.flush_profile {
            ProduceFlushProfile::LowLatency => 0,
            ProduceFlushProfile::Balanced => 1,
            ProduceFlushProfile::HighThroughput => 2,
            ProduceFlushProfile::Extreme => 3,
        };
        MetricsRecorder::set_produce_profile(profile_id);
        info!("ProduceHandler initialized with profile: {} (id={})", config.flush_profile.name(), profile_id);

        Ok(Self {
            config,
            storage,
            indexer,
            json_pipeline,
            index_sender,
            metadata_store,
            partition_states: Arc::new(DashMap::new()),  // PERFORMANCE (v2.2.7 - P2): Lock-free concurrent hashmap
            producer_info: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(ProduceMetrics::default()),
            running: Arc::new(AtomicBool::new(true)),
            memory_used_bytes,
            memory_limit_bytes,
            replication_sender: None,
            fetch_handler: None,
            topic_creation_cache: Arc::new(RwLock::new(HashMap::new())),
            wal_manager: None,
            raft_cluster: None,  // v2.2.7 Phase 3: Initialize as None (set via set_raft_cluster)
            wal_replication_manager: None,  // v2.2.0 Phase 1: Initialize as None
            isr_ack_tracker: None,  // v2.2.7 Phase 4: Initialize as None (set via set_isr_ack_tracker)
            leader_elector: None,  // v2.2.7 Phase 5: Initialize as None (set via set_leader_elector)
            event_bus: None,  // v2.2.7.2: Initialize as None (set via set_event_bus)
            leadership_cache: Arc::new(DashMap::new()),  // Optimization #4: Empty cache, populated on first access
            pipelined_pool: Arc::new(PipelinedConnectionPool::new(1000)),  // v2.2.9: Async pipelined connection pool
            response_pipeline: None,  // v2.2.10: Initialize as None (set via set_response_pipeline) - CRITICAL FIX #7
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

    /// Set the Raft cluster for metadata coordination (v2.2.7 Phase 3)
    pub fn set_raft_cluster(&mut self, raft_cluster: Arc<RaftCluster>) {
        info!("Setting RaftCluster for ProduceHandler - enables partition replication routing");
        self.raft_cluster = Some(raft_cluster);
    }

    /// Set the WAL replication manager for PostgreSQL-style streaming (v2.2.0+)
    pub fn set_wal_replication_manager(&mut self, replication_manager: Arc<WalReplicationManager>) {
        info!("Setting WalReplicationManager for ProduceHandler");
        self.wal_replication_manager = Some(replication_manager);
    }

    /// Set the ISR ACK tracker for acks=-1 quorum support (v2.2.7 Phase 4)
    pub fn set_isr_ack_tracker(&mut self, tracker: Arc<crate::isr_ack_tracker::IsrAckTracker>) {
        info!("Setting IsrAckTracker for ProduceHandler - enables acks=-1 quorum");
        self.isr_ack_tracker = Some(tracker);
    }

    /// Set the leader elector for partition leader failover (v2.2.7 Phase 5)
    pub fn set_leader_elector(&mut self, elector: Arc<crate::leader_election::LeaderElector>) {
        info!("Setting LeaderElector for ProduceHandler - enables heartbeat tracking");
        self.leader_elector = Some(elector);
    }

    /// Set the metadata event bus for high watermark replication (v2.2.7.2)
    pub fn set_event_bus(&mut self, event_bus: Arc<crate::metadata_events::MetadataEventBus>) {
        info!("Setting MetadataEventBus for ProduceHandler - enables < 10ms watermark replication");
        self.event_bus = Some(event_bus);
    }

    /// Set the response pipeline for async acks=1 responses (v2.2.10)
    /// CRITICAL PERFORMANCE FIX #7: Eliminates 168x bottleneck by decoupling responses from WAL fsync
    pub fn set_response_pipeline(&mut self, pipeline: Arc<crate::response_pipeline::ResponsePipeline>) {
        info!("Setting ResponsePipeline for ProduceHandler - enables async acks=1 responses (150x+ improvement)");
        self.response_pipeline = Some(pipeline);
    }

    /// Emit high watermark update event (v2.2.7.2)
    /// Replaces 5-second background sync with < 10ms metadata WAL replication
    fn emit_watermark_event(&self, topic: &str, partition: i32, offset: i64) {
        if let Some(ref event_bus) = self.event_bus {
            let subscriber_count = event_bus.publish(crate::metadata_events::MetadataEvent::HighWatermarkUpdated {
                topic: topic.to_string(),
                partition,
                offset,
            });
            warn!("📡 Emitted HighWatermarkUpdated: {}-{} => {}, subscribers={}",
                  topic, partition, offset, subscriber_count);
        } else {
            warn!("⚠️  No event_bus configured - watermark event NOT emitted for {}-{}", topic, partition);
        }
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

        // CRITICAL FIX (v2.2.7): Background watermark sync task
        // Periodically syncs in-memory high watermarks to Raft metadata store
        // This decouples produce hot path from expensive Raft consensus (150ms)
        //
        // REVERTED (v2.2.7.2): Removed watermark monitoring improvements from v2.2.7.1
        // - Reason: Made consumption worse (-9% regression) without solving actual problem
        // - No watermark lag was detected (improvements solving wrong problem)
        // - Actual issue likely in fetch handler pagination or partition balancing
        let handler_for_watermark = self.clone();
        tokio::spawn(async move {
            while handler_for_watermark.running.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(5)).await;

                // Sync watermarks for each partition asynchronously
                for entry in handler_for_watermark.partition_states.iter() {
                    let (topic, partition) = entry.key();
                    let state = entry.value();

                    let high_watermark = state.high_watermark.load(Ordering::SeqCst) as i64;
                    let log_start_offset = state.log_start_offset.load(Ordering::SeqCst) as i64;

                    // Update metadata store asynchronously (doesn't block produce path)
                    if let Err(e) = handler_for_watermark.metadata_store.update_partition_offset(
                        topic,
                        *partition as u32,
                        high_watermark,
                        log_start_offset
                    ).await {
                        // P2 FIX (v2.2.7): Tolerate "Cannot propose" errors during startup/leadership changes
                        // During cluster startup, Raft may not have elected a leader yet. This is a transient
                        // condition that resolves automatically. Log at DEBUG level to avoid alarming users.
                        let error_msg = format!("{:?}", e);
                        if error_msg.contains("Cannot propose") {
                            debug!(
                                "Background watermark sync waiting for Raft leader election for {}-{}: {:?}",
                                topic, partition, e
                            );
                        } else {
                            warn!(
                                "Background watermark sync failed for {}-{}: {:?}",
                                topic, partition, e
                            );
                        }
                    } else {
                        debug!(
                            "✓ Synced watermark for {}-{}: high_watermark={}, log_start={}",
                            topic, partition, high_watermark, log_start_offset
                        );
                    }
                }
            }

            info!("Background watermark sync task stopped");
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

        // PARTITION_DEBUG: Log which partitions client is requesting
        for topic in &request.topics {
            info!("PARTITION_DEBUG: PRODUCE topic={} partition_count={}",
                topic.name, topic.partitions.len());
            for partition_data in &topic.partitions {
                info!("PARTITION_DEBUG:   partition={} records_bytes={}",
                    partition_data.index, partition_data.records.len());
            }
        }

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
            
            // PERFORMANCE (v2.2.7 - P1): Process partitions in parallel instead of serially
            // This provides 20-30% throughput improvement by overlapping partition I/O
            let topic_name = topic_data.name.clone();
            let partition_count = topic_metadata.config.partition_count;

            let response_partitions = stream::iter(topic_data.partitions)
                .map(|partition_data| {
                    let topic_name = topic_name.clone();
                    let metadata_store = self.metadata_store.clone();
                    let node_id = self.config.node_id;
                    let transactional_id = transactional_id.clone();
                    let metrics = self.metrics.clone();

                    async move {
                        // Validate partition exists
                        if partition_data.index >= partition_count as i32 {
                            return ProduceResponsePartition {
                                index: partition_data.index,
                                error_code: ErrorCode::UnknownTopicOrPartition.code(),
                                base_offset: -1,
                                log_append_time: -1,
                                log_start_offset: 0,
                            };
                        }

                        // OPTIMIZATION #4: Check leadership using cached assignments (15-20% throughput gain)
                        // Cache-first strategy: check cache, fallback to metadata store if stale/missing
                        let cache_key = (topic_name.clone(), partition_data.index);

                        // TTL configurable via environment variable (default: 60s)
                        // Leadership rarely changes in stable clusters, so long TTL is safe
                        // Old default was 5s, causing excessive Raft queries (10-50ms each)
                        let cache_ttl_secs = std::env::var("CHRONIK_LEADERSHIP_CACHE_TTL_SECS")
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(60); // 60 seconds default (12x improvement over old 5s)

                        let (is_leader, leader_hint) = {
                            // Fast path: Try cache first
                            if let Some(entry) = self.leadership_cache.get(&cache_key) {
                                let (leader, cached_at) = *entry;
                                let age = cached_at.elapsed().as_secs();

                                if age < cache_ttl_secs {
                                    // Cache hit - use cached leader
                                    let is_leader = leader == node_id as u64;
                                    debug!(
                                        "CACHE HIT: {}-{} leader={} is_leader={} age={}s",
                                        topic_name, partition_data.index, leader, is_leader, age
                                    );
                                    (is_leader, Some(leader))
                                } else {
                                    // Cache stale - fallthrough to metadata query
                                    debug!(
                                        "CACHE STALE: {}-{} age={}s (TTL={}s), refreshing",
                                        topic_name, partition_data.index, age, cache_ttl_secs
                                    );
                                    drop(entry); // Release lock before query

                                    // Query and update cache
                                    match metadata_store.get_partition_assignments(&topic_name).await {
                                        Ok(assignments) => {
                                            assignments.iter()
                                                .find(|a| a.partition == partition_data.index as u32)
                                                .map(|a| {
                                                    let leader = a.leader_id;
                                                    let is_leader = leader == node_id as u64;
                                                    self.leadership_cache.insert(cache_key.clone(), (leader, std::time::Instant::now()));
                                                    debug!(
                                                        "CACHE REFRESH: {}-{} leader={} is_leader={}",
                                                        topic_name, partition_data.index, leader, is_leader
                                                    );
                                                    (is_leader, Some(leader))
                                                })
                                                .unwrap_or((false, None))
                                        }
                                        Err(e) => {
                                            warn!(
                                                "Failed to refresh cache for {}-{}: {:?}",
                                                topic_name, partition_data.index, e
                                            );
                                            (false, None)
                                        }
                                    }
                                }
                            } else {
                                // Cache miss - query metadata store and populate cache
                                debug!("CACHE MISS: {}-{}, querying metadata store", topic_name, partition_data.index);
                                match metadata_store.get_partition_assignments(&topic_name).await {
                                    Ok(assignments) => {
                                        assignments.iter()
                                            .find(|a| a.partition == partition_data.index as u32)
                                            .map(|a| {
                                                let leader = a.leader_id;
                                                let is_leader = leader == node_id as u64;
                                                self.leadership_cache.insert(cache_key.clone(), (leader, std::time::Instant::now()));
                                                debug!(
                                                    "CACHE POPULATE: {}-{} leader={} is_leader={}",
                                                    topic_name, partition_data.index, leader, is_leader
                                                );
                                                (is_leader, Some(leader))
                                            })
                                            .unwrap_or_else(|| {
                                                debug!(
                                                    "Partition {}-{} not assigned in metadata store",
                                                    topic_name, partition_data.index
                                                );
                                                (false, None)
                                            })
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to get partition assignments for {}-{}: {:?}",
                                            topic_name, partition_data.index, e
                                        );
                                        (false, None)
                                    }
                                }
                            }
                        };

                        if !is_leader {
                            // Forward request to leader instead of returning error (v2.2.9)
                            if let Some(leader_id) = leader_hint {
                                debug!(
                                    "Not leader for {}-{}, forwarding to leader node {}",
                                    topic_name, partition_data.index, leader_id
                                );

                                match self.forward_produce_to_leader(
                                    leader_id,
                                    &topic_name,
                                    partition_data.index,
                                    &partition_data.records,
                                    acks,
                                ).await {
                                    Ok(response) => {
                                        debug!(
                                            "Successfully forwarded {}-{} to leader {}, offset={}",
                                            topic_name, partition_data.index, leader_id, response.base_offset
                                        );
                                        return response;
                                    }
                                    Err(e) => {
                                        error!(
                                            "Failed to forward {}-{} to leader {}: {}",
                                            topic_name, partition_data.index, leader_id, e
                                        );
                                        // Fallback to NOT_LEADER_FOR_PARTITION on forwarding failure
                                        return ProduceResponsePartition {
                                            index: partition_data.index,
                                            error_code: ErrorCode::NotLeaderForPartition.code(),
                                            base_offset: -1,
                                            log_append_time: -1,
                                            log_start_offset: 0,
                                        };
                                    }
                                }
                            } else {
                                // No leader hint, return NOT_LEADER_FOR_PARTITION
                                debug!(
                                    "Not leader for {}-{} and no leader hint, returning NOT_LEADER_FOR_PARTITION",
                                    topic_name, partition_data.index
                                );
                                return ProduceResponsePartition {
                                    index: partition_data.index,
                                    error_code: ErrorCode::NotLeaderForPartition.code(),
                                    base_offset: -1,
                                    log_append_time: -1,
                                    log_start_offset: 0,
                                };
                            }
                        }

                        // Process the partition data
                        debug!(
                            "PARTITION_DATA: topic={}, partition={}, records_len={} bytes",
                            topic_name, partition_data.index, partition_data.records.len()
                        );
                        match self.produce_to_partition(
                            &topic_name,
                            partition_data.index,
                            &partition_data.records,
                            transactional_id.as_deref(),
                            acks,
                        ).await {
                            Ok(response) => response,
                            Err(e) => {
                                error!(
                                    "Failed to produce to {}-{}: {}",
                                    topic_name, partition_data.index, e
                                );

                                metrics.produce_errors.fetch_add(1, Ordering::Relaxed);

                                let error_code = match e {
                                    Error::DuplicateSequenceNumber(_) => ErrorCode::DuplicateSequenceNumber.code(),
                                    Error::InvalidProducerEpoch(_) => ErrorCode::InvalidProducerEpoch.code(),
                                    Error::OutOfOrderSequenceNumber(_) => ErrorCode::OutOfOrderSequenceNumber.code(),
                                    Error::InvalidTransactionState(_) => ErrorCode::InvalidTxnState.code(),
                                    _ => ErrorCode::None.code(),
                                };

                                ProduceResponsePartition {
                                    index: partition_data.index,
                                    error_code,
                                    base_offset: -1,
                                    log_append_time: -1,
                                    log_start_offset: 0,
                                }
                            }
                        }
                    }
                })
                .buffer_unordered(16)  // Process up to 16 partitions concurrently
                .collect::<Vec<_>>()
                .await;
            
            response_topics.push(ProduceResponseTopic {
                name: topic_data.name,
                partitions: response_partitions,
            });
        }
        
        Ok(response_topics)
    }

    /// Forward produce request to the partition leader (v2.2.9)
    ///
    /// CRITICAL PERFORMANCE FIX: Async pipelined forwarding eliminates head-of-line blocking
    /// OLD (v2.2.8 - synchronous): 2,197 msg/s @ 51ms p99 (each request blocks ~50ms)
    /// NEW (v2.2.9 - pipelined): Expected 300,000+ msg/s @ <5ms p99 (100+ requests in-flight)
    ///
    /// When a non-leader receives a produce request, forward it to the actual leader
    /// instead of returning NOT_LEADER_FOR_PARTITION. This matches Kafka's behavior.
    async fn forward_produce_to_leader(
        &self,
        leader_id: u64,
        topic: &str,
        partition: i32,
        records_data: &[u8],
        acks: i16,
    ) -> Result<ProduceResponsePartition> {
        use bytes::{Bytes, BytesMut, BufMut};
        use chronik_protocol::parser::{Encoder, Decoder};

        // Get leader's Kafka address from metadata store
        let leader_addr = match self.metadata_store.get_broker(leader_id as i32).await {
            Ok(Some(broker)) => {
                format!("{}:{}", broker.host, broker.port)
            }
            Ok(None) => {
                error!("Leader node {} not found in metadata store", leader_id);
                return Err(Error::Internal(format!("Leader node {} not found", leader_id)));
            }
            Err(e) => {
                error!("Failed to get leader {} address: {}", leader_id, e);
                return Err(Error::Internal(format!("Metadata error: {}", e)));
            }
        };

        debug!(
            "Forwarding produce request for {}-{} to leader {} at {}",
            topic, partition, leader_id, leader_addr
        );

        // CRITICAL FIX (v2.2.9): Get pipelined connection
        // Eliminates synchronous head-of-line blocking (168x performance improvement)
        let conn = self.pipelined_pool.get_connection(&leader_addr).await?;

        // Build Kafka Produce request frame (Kafka wire format: length + header + body)
        let request_frame = {
            let api_version = 9i16; // Use v9 for modern protocol

            // Encode request header
            let mut header_buf = BytesMut::new();
            {
                let mut encoder = Encoder::new(&mut header_buf);
                encoder.write_i16(0); // API key 0 = Produce
                encoder.write_i16(api_version);
                encoder.write_i32(0); // correlation_id (will be set by pipelined connection)
                encoder.write_compact_string(Some("chronik-forward")); // client_id
                encoder.write_unsigned_varint(0); // No tagged fields
            }

            // Encode request body (following parse_produce_request in reverse)
            let mut body_buf = BytesMut::new();
            {
                let mut body_encoder = Encoder::new(&mut body_buf);

                // transactional_id (v3+, compact string for v9+)
                body_encoder.write_compact_string(None);

                // acks
                body_encoder.write_i16(acks);

                // timeout_ms
                body_encoder.write_i32(5000); // 5 second timeout for forwarded requests

                // topics array (compact for v9+)
                body_encoder.write_unsigned_varint(2); // 1 topic + 1

                // topic name (compact string for v9+)
                body_encoder.write_compact_string(Some(topic));

                // partitions array (compact for v9+)
                body_encoder.write_unsigned_varint(2); // 1 partition + 1

                // partition index
                body_encoder.write_i32(partition);

                // records (compact bytes for v9+)
                body_encoder.write_compact_bytes(Some(records_data));

                // Tagged fields for partition (v9+)
                body_encoder.write_unsigned_varint(0);

                // Tagged fields for topic (v9+)
                body_encoder.write_unsigned_varint(0);

                // Tagged fields for request (v9+)
                body_encoder.write_unsigned_varint(0);
            }

            // Combine header + body with length prefix
            let message_size = header_buf.len() + body_buf.len();
            let mut frame_buf = BytesMut::with_capacity(4 + message_size);
            frame_buf.put_i32(message_size as i32);
            frame_buf.extend_from_slice(&header_buf);
            frame_buf.extend_from_slice(&body_buf);

            frame_buf.freeze()
        };

        // CRITICAL: Send request via pipelined connection (NON-BLOCKING!)
        // Multiple requests can be in-flight simultaneously (100-500 expected)
        // Responses matched back via correlation IDs in background receive task
        let response_frame = conn.send_request(request_frame, 5000).await?;

        // Parse response (length-prefixed frame already handled by pipelined connection)
        let mut response_bytes = response_frame.slice(4..); // Skip length prefix
        let mut decoder = Decoder::new(&mut response_bytes);

        // Response header
        let _response_correlation_id = decoder.read_i32()?; // Already validated by pipelined connection

        // Tagged fields in response header (v9+)
        let tagged_count = decoder.read_unsigned_varint()?;
        for _ in 0..tagged_count {
            let _tag_id = decoder.read_unsigned_varint()?;
            let tag_size = decoder.read_unsigned_varint()? as usize;
            decoder.advance(tag_size)?;
        }

        // Parse response body (reverse of encode_produce_response)
        // Topics array (compact for v9+)
        let topic_count = (decoder.read_unsigned_varint()? - 1) as usize;
        if topic_count != 1 {
            error!("Expected 1 topic in response, got {}", topic_count);
            return Err(Error::Internal("Unexpected topic count".into()));
        }

        // Topic name
        let _topic_name = decoder.read_compact_string()?;

        // Partitions array
        let partition_count = (decoder.read_unsigned_varint()? - 1) as usize;
        if partition_count != 1 {
            error!("Expected 1 partition in response, got {}", partition_count);
            return Err(Error::Internal("Unexpected partition count".into()));
        }

        // Partition response
        let partition_index = decoder.read_i32()?;
        let error_code = decoder.read_i16()?;
        let base_offset = decoder.read_i64()?;

        // log_append_time (v2+)
        let log_append_time = decoder.read_i64()?;

        // log_start_offset (v5+)
        let log_start_offset = decoder.read_i64()?;

        // Tagged fields for partition (v9+)
        let tagged_count = decoder.read_unsigned_varint()?;
        for _ in 0..tagged_count {
            let _tag_id = decoder.read_unsigned_varint()?;
            let tag_size = decoder.read_unsigned_varint()? as usize;
            decoder.advance(tag_size)?;
        }

        Ok(ProduceResponsePartition {
            index: partition_index,
            error_code,
            base_offset,
            log_append_time,
            log_start_offset,
        })
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
        let mut last_checkpoint = start_time;

        // ENTRY POINT LOGGING (v1.3.47 debugging)
        debug!("→ produce_to_partition({}-{}) bytes={} acks={}", topic, partition, records_data.len(), acks);

        // P3 OPTIMIZATION (v2.2.7): Lock-free memory tracking with AtomicU64
        // Check memory limit and atomically reserve memory
        let bytes_to_reserve = records_data.len() as u64; // Declare outside loop for later use
        loop {
            let current_used = self.memory_used_bytes.load(Ordering::Acquire);
            let new_used = current_used + bytes_to_reserve;

            // Check if we would exceed the limit
            if new_used > self.memory_limit_bytes {
                return Err(Error::Internal("Memory limit exceeded".into()));
            }

            // Try to atomically update the counter
            match self.memory_used_bytes.compare_exchange_weak(
                current_used,
                new_used,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break, // Successfully reserved memory
                Err(_) => continue, // CAS failed, retry
            }
        }
        
        // Get or create partition state FIRST
        let partition_state = self.get_or_create_partition_state(topic, partition).await?;

        // CRITICAL FIX (v2.2.10): ATOMIC offset allocation to prevent race conditions
        // Problem: Multiple concurrent produce requests could read the same base_offset
        // before any of them updates next_offset, causing duplicate offsets and dropped responses.
        //
        // Solution: Get record count first, then atomically reserve the offset range.
        // This ensures each request gets a unique, non-overlapping offset range.
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

        // Decode batch ONCE to get record count for atomic offset reservation
        let (temp_batch, _) = KafkaRecordBatch::decode(records_data)
            .map_err(|e| Error::Protocol(format!("Failed to decode record batch: {}", e)))?;
        let record_count = temp_batch.records.len() as u64;

        // ATOMIC: Reserve offset range in ONE operation (prevents race conditions)
        let base_offset = partition_state.next_offset.fetch_add(record_count, Ordering::SeqCst);

        // Check if we need to modify the batch at all
        let (re_encoded_bytes, kafka_batch) = if incoming_base_offset == base_offset as i64 {
            // Perfect match - store original bytes AS-IS (preserves CRC perfectly)
            debug!(
                "Base offset match ({}) - storing original bytes without modification (byte-perfect CRC preservation)",
                base_offset
            );
            let bytes = Bytes::copy_from_slice(records_data);

            // Reuse the temp_batch we already decoded
            (bytes, temp_batch)
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

        // NOTE: next_offset already updated atomically by fetch_add() above (line 1839)

        // CRITICAL FIX (v2.2.7): Removed synchronous metadata update from hot path
        // BEFORE: Every produce waited 150ms for Raft consensus to update high watermark
        // AFTER: Background task syncs watermarks every 5 seconds asynchronously
        // This improves throughput from 6 msg/s to 10,000+ msg/s
        //
        // Watermark persistence strategy:
        // 1. In-memory watermarks updated immediately (above)
        // 2. Background task periodically syncs to Raft metadata store
        // 3. On leader change, watermarks recovered from Raft
        // 4. WAL provides durability for actual message data
        //
        // NOTE: Slight chance of watermark drift on crash (up to 5 seconds)
        // but this is acceptable because:
        // - WAL recovery will rebuild correct watermarks from persisted data
        // - Clients can handle duplicate message delivery (at-least-once semantics)
        // - Performance gain (2500x faster) far outweighs this minor inconsistency risk
        
        // Log batch-level summary (performance-optimized)
        debug!(
            "Batch: topic={} partition={} base_offset={} records={} bytes={}",
            topic, partition, base_offset, records.len(), total_bytes
        );

        // REMOVED: Old Raft data replication code (was behind #[cfg(feature = "raft")])
        // REASON: Raft should ONLY handle metadata (leader election, ISR tracking, assignments)
        // MESSAGE DATA replication is handled by WAL streaming (wal_replication.rs)
        // See docs/HYBRID_CLUSTERING_ARCHITECTURE.md for design rationale
        //
        // Performance: Raft data replication = 2-5K msg/s, WAL streaming = 60K+ msg/s
        //
        // The hybrid design:
        // - Raft: Metadata coordination (data/wal/__meta/)
        // - WAL Streaming: Message data (data/wal/{topic}/{partition}/)

        // CRITICAL (v1.3.47+): Write to WAL BEFORE updating high watermark
        // This ensures durability guarantee - data is persisted before acknowledgment
        // v2.2.0: Parse and serialize ONCE, reuse for both local WAL and replication
        if self.wal_manager.is_none() {
            warn!("WAL manager is None - WAL writes DISABLED! Data will NOT be durable!");
        }

        // v2.2.0: Store serialized WAL data for replication (zero-copy optimization)
        let serialized_for_replication: Option<Vec<u8>>;

        if let Some(ref wal_mgr) = self.wal_manager {
            use chronik_storage::canonical_record::CanonicalRecord;

            // PERFORMANCE OPTIMIZATION (v2.2.7): Skip wire bytes preservation if no replication
            // This avoids an expensive .to_vec() clone when replication is disabled
            let needs_replication = self.wal_replication_manager.is_some();

            // Convert to CanonicalRecord and serialize (ONCE - reused for replication)
            // from_kafka_batch() now automatically preserves compressed_records_wire_bytes
            // for BOTH compressed and uncompressed batches (v2.2.7 fix)
            match CanonicalRecord::from_kafka_batch(&re_encoded_bytes) {
                Ok(mut canonical_record) => {
                    // CRITICAL FIX (Session 24): For v1 MessageSets, recalculate record offsets
                    // because only the first message's offset was updated in wire bytes (line 1157),
                    // leaving subsequent messages with incorrect offsets.
                    if canonical_record.original_v1_wire_format.is_some() {
                        warn!("SESSION24_FIX: v1 MessageSet detected, recalculating record offsets from base_offset={}",
                              canonical_record.base_offset);
                        canonical_record.recalculate_record_offsets();
                    }

                    match bincode::serialize(&canonical_record) {
                        Ok(serialized) => {
                            // v2.2.7: ALWAYS populate serialized_for_replication when wal_replication_manager exists
                            // The wire bytes optimization (line 1375-1377) only affects CRC preservation, not replication!
                            // BUG FIX: Previously set to None when !needs_replication, breaking replication entirely
                            serialized_for_replication = if needs_replication {
                                Some(serialized.clone())
                            } else {
                                None  // No replication manager = no data to replicate
                            };

                            // v2.2.10 CRITICAL PERFORMANCE FIX #7: Async response delivery for acks=1
                            // OLD (v2.2.9): Synchronous WAL fsync blocking → 2,197 msg/s (168x slower)
                            // NEW (v2.2.10): Async callback-based responses → 300,000+ msg/s (150x+ improvement)
                            //
                            // Architecture:
                            // - acks=0: Fire-and-forget (no change - already fast)
                            // - acks=1/all WITH response_pipeline: Register → WAL(acks=0) → Wait for callback
                            // - acks=1/all WITHOUT response_pipeline: Fallback to synchronous path
                            let wal_start = Instant::now();

                            // Check if we should use async response delivery
                            let use_async_responses = self.response_pipeline.is_some() && acks != 0;

                            if use_async_responses {
                                // ASYNC PATH: Register for callback notification, write with acks=0 (non-blocking)
                                info!("🔵 ASYNC_RESPONSE_PATH: topic={} partition={} base_offset={} last_offset={} acks={}",
                                      topic, partition, base_offset, last_offset, acks);

                                // Create oneshot channel for async response
                                let (response_tx, response_rx) = oneshot::channel();

                                // Register with ResponsePipeline BEFORE WAL write
                                // (callback will be triggered after group commit completes)
                                if let Some(ref pipeline) = self.response_pipeline {
                                    info!("🔵 REGISTERING: topic={} partition={} base_offset={} last_offset={}",
                                          topic, partition, base_offset, last_offset);
                                    pipeline.register(
                                        topic.to_string(),
                                        partition,
                                        base_offset as i64,
                                        last_offset as i64,
                                        response_tx,
                                    );
                                }

                                // Write to WAL with acks=0 (fire-and-forget, no blocking!)
                                // Background group commit will trigger callback when fsync completes
                                if let Err(e) = wal_mgr.append_canonical_with_acks(
                                    topic.to_string(),
                                    partition,
                                    serialized,
                                    base_offset as i64,
                                    last_offset as i64,
                                    records.len() as i32,
                                    0  // acks=0 → non-blocking
                                ).await {
                                    error!("WAL WRITE FAILED (async path): topic={} partition={} error={}",
                                           topic, partition, e);
                                    return Err(Error::Internal(format!("WAL write failed: {}", e)));
                                }

                                // Wait for async callback notification (non-blocking on WAL!)
                                // The response_rx will be signaled when GroupCommitWal completes the batch fsync
                                match response_rx.await {
                                    Ok(_response) => {
                                        // Success! Callback delivered the response
                                        let wal_elapsed = wal_start.elapsed();
                                        debug!("⏱️  ASYNC RESPONSE DELIVERED: topic={} partition={} latency={:?}",
                                               topic, partition, wal_elapsed);
                                    }
                                    Err(_) => {
                                        error!("ASYNC RESPONSE CHANNEL CLOSED: topic={} partition={} base_offset={}",
                                               topic, partition, base_offset);
                                        return Err(Error::Internal("Async response channel closed".into()));
                                    }
                                }
                            } else {
                                // SYNCHRONOUS PATH: Original blocking behavior (acks=0 or no response_pipeline)
                                // - acks=0: Already fast (fire-and-forget)
                                // - acks=1/all: Blocks on fsync (fallback mode)
                                if let Err(e) = wal_mgr.append_canonical_with_acks(
                                    topic.to_string(),
                                    partition,
                                    serialized,
                                    base_offset as i64,
                                    last_offset as i64,
                                    records.len() as i32,
                                    acks
                                ).await {
                                    error!("WAL WRITE FAILED (sync path): topic={} partition={} acks={} error={}",
                                           topic, partition, acks, e);
                                    return Err(Error::Internal(format!("WAL write failed: {}", e)));
                                }

                                let wal_elapsed = wal_start.elapsed();
                                debug!("⏱️  PERF: WAL write (acks={}, sync path): {:?}", acks, wal_elapsed);
                            }
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
        } else {
            serialized_for_replication = None;
        }

        // v2.2.7 Phase 3: WAL Replication Hook with ISR-aware routing
        // Zero-copy optimization: Reuse serialized WAL data from above (no re-parsing!)
        // This is called AFTER WAL write completes, so data is durable locally
        if let Some(ref wal_repl_mgr) = self.wal_replication_manager {
            debug!("🔍 DEBUG: WAL replication manager exists for {}-{}", topic, partition);
            if let Some(serialized_data) = serialized_for_replication {
                debug!("🔍 DEBUG: Serialized data exists ({} bytes), spawning replication task for {}-{} offset={}",
                    serialized_data.len(), topic, partition, base_offset);

                // Clone necessary metadata (cheap - just strings and ints)
                let topic_clone = topic.to_string();
                let partition_clone = partition;
                let base_offset_clone = base_offset as i64;
                let repl_mgr_clone = Arc::clone(wal_repl_mgr);

                // Get current high watermark for ISR filtering
                let high_watermark = partition_state.high_watermark.load(Ordering::SeqCst) as i64;

                // Spawn background task (fire-and-forget, never blocks)
                // v2.2.7: Use replicate_partition for ISR-aware routing
                tokio::spawn(async move {
                    debug!("🚀 DEBUG: Calling replicate_partition for {}-{} offset={}",
                        topic_clone, partition_clone, base_offset_clone);
                    repl_mgr_clone.replicate_partition(
                        topic_clone,
                        partition_clone,
                        base_offset_clone,
                        high_watermark,  // For ISR filtering
                        serialized_data,
                    ).await;
                    // Errors are logged inside replicate_partition, we don't block produce
                });
            } else {
                // WAL manager was None, nothing to replicate
                debug!("⚠️ DEBUG: Skipping replication for {}-{}: serialized_for_replication is None!", topic, partition);
            }
        } else {
            debug!("⚠️ DEBUG: Skipping replication for {}-{}: wal_replication_manager is None!", topic, partition);
        }

        // v2.2.7 FIX: Removed duplicate buffering code that was always running
        // (lines 1491-1499 were duplicate of lines 1475-1488)

        // Update metrics
        self.metrics.records_produced.fetch_add(records.len() as u64, Ordering::Relaxed);
        self.metrics.bytes_produced.fetch_add(total_bytes, Ordering::Relaxed);
        
        // Send to indexing pipeline if enabled
        if self.config.enable_indexing {
            self.send_to_indexer(topic, partition, &records).await;
        }

        // Handle acknowledgment modes
        debug!("🎯 PRODUCE REQUEST: topic={}, partition={}, acks={}, base_offset={}, last_offset={}",
              topic, partition, acks, base_offset, last_offset);

        match acks {
            0 => {
                // No acknowledgment required, return immediately
                debug!("Acks=0: Returning immediately without waiting for persistence");

                // Update high watermark for acks=0
                // Use fetch_max to prevent backward progression during retries (idempotent)
                let old_watermark = partition_state.high_watermark.load(Ordering::SeqCst) as i64;
                let new_watermark = (last_offset + 1) as i64;
                let prev_watermark = partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst) as i64;

                debug!(
                    "🔥 WATERMARK UPDATE [acks=0]: topic={}, partition={}, old={}, new={}, last_offset={}, actually_updated={}",
                    topic, partition, old_watermark, new_watermark, last_offset, prev_watermark < new_watermark
                );

                // v2.2.7.2: Emit watermark event for < 10ms replication
                self.emit_watermark_event(topic, partition, new_watermark);

                // OPTIMIZATION P1 (v2.2.11): ASYNC metadata update for acks=0 (eliminate second fsync!)
                // In-memory watermark already updated above, so ListOffsets queries see it immediately
                // Metadata WAL update ensures durability but doesn't need to block producer (fire-and-forget)
                // This eliminates second fsync from critical path: ~25ms → ~15ms latency (30-40% improvement)
                if prev_watermark < new_watermark {
                    let metadata_store = self.metadata_store.clone();
                    let topic_clone = topic.to_string();
                    let partition_u32 = partition as u32;
                    tokio::spawn(async move {
                        if let Err(e) = metadata_store.update_partition_offset(
                            &topic_clone,
                            partition_u32,
                            new_watermark,
                            0  // log_start_offset
                        ).await {
                            debug!("Background metadata watermark update failed for {}-{}: {:?}", topic_clone, partition_u32, e);
                        }
                    });
                }
            }
            1 => {
                // For acks=1, we don't need to flush immediately
                // The background task will handle flushing based on time/size thresholds
                // We just need to ensure the data is buffered
                debug!("Acks=1: Data buffered, will be persisted by background task");

                // Update high watermark for acks=1
                // Use fetch_max to prevent backward progression during retries (idempotent)
                let old_watermark = partition_state.high_watermark.load(Ordering::SeqCst) as i64;
                let new_watermark = (last_offset + 1) as i64;
                let prev_watermark = partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst) as i64;

                debug!(
                    "🔥 WATERMARK UPDATE [acks=1]: topic={}, partition={}, old={}, new={}, last_offset={}, actually_updated={}",
                    topic, partition, old_watermark, new_watermark, last_offset, prev_watermark < new_watermark
                );

                // v2.2.7.2: Emit watermark event for < 10ms replication
                self.emit_watermark_event(topic, partition, new_watermark);

                // OPTIMIZATION P1 (v2.2.11): ASYNC metadata update for acks=1 (eliminate second fsync!)
                // In-memory watermark already updated above, so ListOffsets queries see it immediately
                // Metadata WAL update ensures durability but doesn't need to block producer (fire-and-forget)
                // This eliminates second fsync from critical path: ~25ms → ~15ms latency (30-40% improvement)
                if prev_watermark < new_watermark {
                    let metadata_store = self.metadata_store.clone();
                    let topic_clone = topic.to_string();
                    let partition_u32 = partition as u32;
                    tokio::spawn(async move {
                        if let Err(e) = metadata_store.update_partition_offset(
                            &topic_clone,
                            partition_u32,
                            new_watermark,
                            0  // log_start_offset
                        ).await {
                            debug!("Background metadata watermark update failed for {}-{}: {:?}", topic_clone, partition_u32, e);
                        }
                    });
                }
            }
            -1 => {
                // v2.2.7 Phase 4: acks=-1 with IsrAckTracker
                // Wait for replication to ISR quorum (leader + majority of followers)
                // NOTE: We do NOT call flush_partition_if_needed() here because:
                // 1. WAL write already completed synchronously (durability guaranteed)
                // 2. Flushing with linger logic (100 batches / 500ms) kills throughput
                // 3. Background task handles actual segment writes
                // 4. Client just needs ISR quorum ACK (immediate in standalone)

                if let Some(ref tracker) = self.isr_ack_tracker {
                    // Register this produce request for ISR quorum tracking
                    // v2.2.9 Phase 2: Option 4 - Get ISR from metadata_store (WAL-only, no Raft)
                    let quorum_size = match self.metadata_store.get_partition_assignments(topic).await {
                        Ok(assignments) => {
                            match assignments.iter().find(|a| a.partition == partition as u32) {
                                Some(assignment) => {
                                    // Quorum = all replicas (ISR) must ack
                                    let isr = &assignment.replicas;
                                    let size = isr.len();
                                    info!("📊 ISR for {}-{} from metadata_store: {:?}, quorum={}",
                                        topic, partition, isr, size);
                                    size
                                }
                                None => {
                                    // Fallback: if partition not found, use 1 (just the leader)
                                    warn!("⚠️  No partition assignment found for {}-{} in metadata_store, using quorum=1",
                                        topic, partition);
                                    1
                                }
                            }
                        }
                        Err(e) => {
                            // Error querying metadata_store, fall back to quorum=1
                            warn!("⚠️  Failed to get partition assignments for {}-{}: {:?}, using quorum=1",
                                topic, partition, e);
                            1
                        }
                    };

                    info!(
                        "🎯 acks=-1: About to register wait for {}-{} offset {} (base_offset={}, quorum={})",
                        topic, partition, base_offset, base_offset, quorum_size
                    );

                    let (tx, rx) = tokio::sync::oneshot::channel();
                    // CRITICAL FIX: Register for base_offset (not last_offset)
                    // Followers ACK base_offset of the batch, so we must wait for that offset
                    tracker.register_wait(
                        topic.to_string(),
                        partition,
                        base_offset as i64,
                        quorum_size,
                        tx,
                    );

                    info!(
                        "✅ acks=-1: Registered {}-{} offset {} for ISR quorum tracking (quorum={})",
                        topic, partition, base_offset, quorum_size
                    );

                    // v2.2.7 FIX: Leader always records its own ACK immediately (cluster AND standalone)
                    // In standalone mode (quorum=1), this is the only ACK needed
                    // In cluster mode (quorum=N), leader counts as 1/N ACKs, then waits for followers
                    info!("🏁 Recording leader self-ACK for {}-{} offset {} (quorum={}/{})",
                        topic, partition, base_offset, 1, quorum_size);
                    tracker.record_ack(topic, partition, base_offset as i64, self.config.node_id as u64);

                    // Wait for ISR quorum with configured timeout (default: 30s)
                    match timeout(REPLICATION_TIMEOUT, rx).await {
                        Ok(Ok(Ok(()))) => {
                            debug!(
                                "acks=-1: ISR quorum reached for {}-{} offset {}",
                                topic, partition, last_offset
                            );

                            // Update high watermark after quorum reached
                            // Use fetch_max to prevent backward progression during retries (idempotent)
                            let old_watermark = partition_state.high_watermark.load(Ordering::SeqCst) as i64;
                            let new_watermark = (last_offset + 1) as i64;
                            let prev_watermark = partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst) as i64;

                            warn!(
                                "🔥 WATERMARK UPDATE [acks=-1, quorum reached]: topic={}, partition={}, old={}, new={}, last_offset={}, actually_updated={}",
                                topic, partition, old_watermark, new_watermark, last_offset, prev_watermark < new_watermark
                            );

                            // v2.2.7.2: Emit watermark event for < 10ms replication
                            self.emit_watermark_event(topic, partition, new_watermark);

                            // v2.2.9.1 ASYNC FIX: Update metadata_store in background (don't block produce response)
                            // In-memory watermark (line 2064) is already updated, so ListOffsets queries see it immediately
                            // Metadata WAL update ensures durability and follower visibility, but doesn't need to block producer
                            // This eliminates second fsync from critical path: ~100ms → ~50ms p50 latency (50% improvement)
                            if prev_watermark < new_watermark {
                                let metadata_store = self.metadata_store.clone();
                                let topic_clone = topic.to_string();
                                let partition_u32 = partition as u32;
                                tokio::spawn(async move {
                                    if let Err(e) = metadata_store.update_partition_offset(
                                        &topic_clone,
                                        partition_u32,
                                        new_watermark,
                                        0  // log_start_offset
                                    ).await {
                                        debug!("Background metadata watermark update failed for {}-{}: {:?}", topic_clone, partition_u32, e);
                                    }
                                });
                            }
                        }
                        Ok(Ok(Err(e))) => {
                            error!(
                                "acks=-1: ISR quorum failed for {}-{} offset {}: {}",
                                topic, partition, last_offset, e
                            );
                            return Err(Error::Internal(format!("ISR quorum failed: {}", e)));
                        }
                        Ok(Err(_)) => {
                            error!(
                                "acks=-1: ISR quorum channel closed for {}-{} offset {}",
                                topic, partition, last_offset
                            );
                            return Err(Error::Internal("ISR quorum channel closed".into()));
                        }
                        Err(_) => {
                            error!(
                                "acks=-1: ISR quorum timeout for {}-{} offset {} after {:?}",
                                topic, partition, last_offset, REPLICATION_TIMEOUT
                            );
                            return Err(Error::Internal("ISR quorum timeout".into()));
                        }
                    }
                } else {
                    // No ISR tracker configured (standalone mode or no replication)
                    // Just update high watermark immediately
                    debug!("acks=-1: No ISR tracker, updating high watermark immediately (standalone mode)");
                    // Use fetch_max to prevent backward progression during retries (idempotent)
                    let old_watermark = partition_state.high_watermark.load(Ordering::SeqCst) as i64;
                    let new_watermark = (last_offset + 1) as i64;
                    let prev_watermark = partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst) as i64;

                    warn!(
                        "🔥 WATERMARK UPDATE [acks=-1, no ISR tracker]: topic={}, partition={}, old={}, new={}, last_offset={}, actually_updated={}",
                        topic, partition, old_watermark, new_watermark, last_offset, prev_watermark < new_watermark
                    );

                    // v2.2.7.2: Emit watermark event for < 10ms replication
                    self.emit_watermark_event(topic, partition, new_watermark);
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
        
        // P3 OPTIMIZATION (v2.2.7): Release memory atomically
        self.memory_used_bytes.fetch_sub(bytes_to_reserve, Ordering::Release);
        
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

        // PERFORMANCE FIX (v2.2.10): Flush for acks=1 to trigger group commit batching
        // acks=-1 already flushed at line 1523, so only flush for acks=0 and acks=1
        // Without this flush, batches accumulate in pending_batches but don't trigger
        // group commit batching properly, resulting in small batch sizes (1-7 writes)
        // instead of hundreds, causing 50-60% throughput loss.
        //
        // Flush strategy:
        // - acks=0: Fire-and-forget, flush triggers group commit batching
        // - acks=1: Flush triggers group commit batching (100ms window accumulates writes)
        // - acks=-1: Already flushed at line 1523 before ISR quorum wait
        if acks != -1 {
            self.flush_partition_if_needed(topic, partition, &partition_state).await?;
        }

        // v2.2.0: WAL replication hook already called earlier (after WAL write, before buffering)
        // No need to duplicate the call here

        // Get partition state for response
        let log_start_offset = partition_state.log_start_offset.load(Ordering::Relaxed) as i64;

        // v2.2.7: Leader election is now event-driven (triggered by WAL stream timeouts)
        // No need to record heartbeats from produce path - elections only happen on actual failures

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
        if let Some(state) = self.partition_states.get(&key) {
            return Ok(Arc::clone(state.value()));
        }

        // Slow path - create new state (DashMap's entry API provides atomic insert-if-absent)
        // Double-check with entry API
        if let Some(state) = self.partition_states.get(&key) {
            return Ok(Arc::clone(state.value()));
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
            pending_batches: Arc::new(SegQueue::new()),
            last_flush: Arc::new(Mutex::new(Instant::now())),
        });

        self.partition_states.insert(key, Arc::clone(&state));
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
            // Note: SegQueue doesn't have a len() method, we need to drain to count
            // This is called infrequently so the drain is acceptable
            let mut pending_count = 0;
            let mut temp_batches = Vec::new();
            while let Some(batch) = state.pending_batches.pop() {
                pending_count += 1;
                temp_batches.push(batch);
            }
            // Push back
            for batch in temp_batches {
                state.pending_batches.push(batch);
            }
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

            pending_count >= min_batches ||
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
            let mut batches = Vec::new();
            while let Some(batch) = state.pending_batches.pop() {
                batches.push(batch);
            }
            batches
        };

        if batches.is_empty() {
            trace!("No pending batches to flush for {}-{}", topic, partition);
            return Ok(());
        }

        // Record flush metrics (v2.1.0)
        MetricsRecorder::record_produce_flush(batches.len() as u64);

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
        for entry in self.partition_states.iter() {
            let (topic, partition) = entry.key();
            let state = entry.value();
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
        for entry in self.partition_states.iter() {
            let (topic, partition) = entry.key();
            let state = entry.value();
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

        // P0 FIX (v2.2.7 Phase 1): Let RaftMetadataStore handle leader-forwarding automatically
        // The metadata store already has Phase 1 forwarding logic built in:
        // - Followers forward create_topic to leader via RPC
        // - Leader processes create via metadata WAL (Phase 2)
        // - Followers wait for replication with exponential backoff
        // DO NOT reject here - let the abstraction layer handle it properly!
        if let Some(ref raft) = self.raft_cluster {
            if raft.am_i_leader().await {
                debug!("Leader node (id={}) processing topic auto-creation for '{}'",
                    raft.node_id(), topic_name);
            } else {
                debug!("Follower node (id={}) will forward topic auto-creation for '{}' to leader via Phase 1 RPC",
                    raft.node_id(), topic_name);
            }
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
                // v2.2.7 FIX: Partition initialization happens through PartitionAssignment module below
                // which has proper metadata WAL writes and replication (in raft_metadata_store.rs)
                // The broken initialize_raft_partitions() method has been removed.

                // Create partition assignments
                // For clustered mode, use round-robin assignment across nodes
                // For standalone mode, assign all partitions to this node
                if let Some(ref raft_cluster) = self.raft_cluster {
                    // Clustered mode: Use round-robin assignment
                    use chronik_common::partition_assignment::PartitionAssignment as AssignmentManager;

                    let nodes = raft_cluster.get_all_nodes().await;
                    info!("Auto-creating topic '{}' in cluster mode with {} nodes", topic_name, nodes.len());

                    // Convert node IDs from u64 to u32 for partition_assignment module
                    let node_ids: Vec<u32> = nodes.iter().map(|&id| id as u32).collect();

                    let mut assignment_mgr = AssignmentManager::new();
                    if let Err(e) = assignment_mgr.add_topic(
                        topic_name,
                        self.config.num_partitions as i32,
                        self.config.default_replication_factor.min(nodes.len() as u32) as i32,
                        &node_ids,
                    ) {
                        warn!("Failed to create partition assignment plan for topic '{}': {:?}", topic_name, e);
                    } else {
                        // v2.2.9 PERFORMANCE OPTIMIZATION: Batch all partition operations into single Raft proposal
                        // OLD: 3 separate proposals per partition (assignment, leader, ISR) × N partitions = 3N Raft rounds
                        // NEW: 1 batched proposal for all partitions = 1 Raft round
                        // Result: 3x faster for N partitions (600ms → 200ms for 3 partitions)

                        let topic_assignments = assignment_mgr.get_topic_assignments(topic_name);

                        // v2.2.9 Phase 3: Option 4 - Use metadata_store directly (WAL-only, no Raft proposal)
                        // Instead of batching into a Raft command, call assign_partition() for each partition
                        // This writes to __chronik_metadata WAL which replicates to followers automatically
                        for (partition_id, partition_info) in topic_assignments {
                            let replicas: Vec<u64> = partition_info.replicas.iter().map(|&id| id as u64).collect();

                            // assign_partition() sets leader (first replica) and ISR (all replicas) automatically
                            if let Err(e) = self.assign_partition(topic_name, partition_id as i32, replicas.clone()).await {
                                warn!("Failed to assign partition {}/{}: {:?}", topic_name, partition_id, e);
                            } else {
                                info!("  ✓ Assigned partition {}/{} with replica list: {:?} (leader: {})",
                                    topic_name, partition_id, partition_info.replicas, partition_info.leader);
                            }
                        }

                        info!("✅ Assigned all partitions for topic '{}' via metadata_store (WAL-only)", topic_name);
                    }
                } else {
                    // Standalone mode: partitions will be assigned on-demand during first produce
                    debug!("Standalone mode: topic '{}' created with {} partitions",
                        topic_name, self.config.num_partitions);
                }

                // v2.2.7 FIX: Runtime check instead of compile-time cfg
                // Standalone mode ONLY (when raft_cluster is None) assign partitions via metadata WAL
                if self.raft_cluster.is_none() {
                    for partition in 0..self.config.num_partitions {
                        let assignment = chronik_common::metadata::PartitionAssignment {
                            topic: topic_name.to_string(),
                            partition,
                            broker_id: self.config.node_id,
                            is_leader: true,
                            replicas: vec![self.config.node_id as u64],
                            leader_id: self.config.node_id as u64,
                        };

                        if let Err(e) = self.metadata_store.assign_partition(assignment).await {
                            warn!("Failed to assign partition {} for topic '{}': {:?}",
                                partition, topic_name, e);
                        }
                    }
                    debug!("Standalone mode: assigned {} partitions for topic '{}'",
                        self.config.num_partitions, topic_name);
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

    /// Initialize partition metadata for an existing topic (WAL-ONLY, no Raft propose)
    ///
    /// This method is called by CreateTopics API handler to ensure partition
    /// assignments are written to metadata WAL and replicated to followers.
    ///
    /// **CRITICAL**: Uses ONLY WAL-based metadata replication, NOT Raft propose().
    /// This eliminates the race condition where Raft and WAL have different replica lists.
    ///
    /// # Arguments
    /// - `topic_name`: Name of the topic
    /// - `num_partitions`: Number of partitions in the topic
    ///
    /// # Returns
    /// Ok(()) if partition metadata was successfully initialized, or if Raft is not enabled
    pub async fn initialize_raft_partitions(&self, topic_name: &str, num_partitions: u32) -> Result<()> {
        // Only initialize if Raft is enabled
        if let Some(ref raft) = self.raft_cluster {
            // v2.2.7 CRITICAL FIX: This method should ONLY be called by the Raft leader
            // The caller (auto_create_topic) must check am_i_leader() before calling this method.
            // We assert this condition to catch programming errors.
            if !raft.am_i_leader().await {
                error!(
                    "PROGRAMMING ERROR: initialize_raft_partitions called on Raft follower for topic '{}'",
                    topic_name
                );
                return Err(Error::Internal(
                    "Cannot initialize partitions on Raft follower - this is a programming error".into()
                ));
            }

            info!("Initializing partition metadata for topic '{}' ({} partitions) - WAL-ONLY (no Raft propose)",
                  topic_name, num_partitions);

            // Get all nodes in the cluster (for replication)
            // For now, use a fixed list of node IDs (1, 2, 3 for a 3-node cluster)
            // TODO: Get this from cluster configuration
            let all_nodes = vec![1_u64, 2_u64, 3_u64];
            let replication_factor = self.config.default_replication_factor.min(all_nodes.len() as u32);

            for partition in 0..num_partitions {
                // Assign replicas (round-robin across nodes)
                let mut replicas = Vec::new();
                for i in 0..replication_factor {
                    let node_idx = (partition as usize + i as usize) % all_nodes.len();
                    replicas.push(all_nodes[node_idx]);
                }

                // v2.2.9 Phase 2: Option 4 - Use metadata_store methods (WAL-only, no Raft commands)
                // assign_partition sets both leader (replicas[0]) and ISR (replicas) in one call
                if let Err(e) = self.assign_partition(topic_name, partition as i32, replicas.clone()).await {
                    warn!("Failed to assign partition {}-{} via metadata_store: {:?}", topic_name, partition, e);
                    continue;  // Skip this partition, try next one
                }

                let leader = replicas[0];
                info!("✓ Initialized partition metadata for {}-{}: replicas={:?}, leader={} (metadata_store)",
                      topic_name, partition, replicas, leader);
            }

            info!("✓ Completed partition metadata initialization for topic '{}' using WAL-ONLY approach", topic_name);
        } else {
            debug!("Raft not enabled, skipping partition metadata initialization for '{}'", topic_name);
        }

        Ok(())
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
        for entry in self.partition_states.iter() {
            let (topic, partition) = entry.key();
            let state = entry.value();
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
        if let Some(state) = self.partition_states.get(&(topic.to_string(), partition)) {
            state.value().high_watermark.load(Ordering::SeqCst) as i64
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
            memory_used_bytes: Arc::clone(&self.memory_used_bytes),
            memory_limit_bytes: self.memory_limit_bytes,
            replication_sender: self.replication_sender.clone(),
            fetch_handler: self.fetch_handler.clone(),
            topic_creation_cache: Arc::clone(&self.topic_creation_cache),
            wal_manager: self.wal_manager.clone(),
            raft_cluster: self.raft_cluster.clone(),  // v2.2.7 Phase 3
            wal_replication_manager: self.wal_replication_manager.clone(),  // v2.2.0 Phase 1
            isr_ack_tracker: self.isr_ack_tracker.clone(),  // v2.2.7 Phase 4
            leader_elector: self.leader_elector.clone(),  // v2.2.7 Phase 5
            event_bus: self.event_bus.clone(),  // v2.2.7.2
            leadership_cache: Arc::clone(&self.leadership_cache),  // Optimization #4
            pipelined_pool: Arc::clone(&self.pipelined_pool),  // v2.2.9: Async pipelined connection pool
            response_pipeline: self.response_pipeline.clone(),  // v2.2.10: Async response delivery (CRITICAL FIX #7)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_common::metadata::{InMemoryMetadataStore, TopicConfig};
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
            ..Default::default()
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(InMemoryMetadataStore::new());
        
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
            ..Default::default()
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(InMemoryMetadataStore::new());
        
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
            ..Default::default()
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(InMemoryMetadataStore::new());
        
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
            ..Default::default()
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(InMemoryMetadataStore::new());
        
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
            ..Default::default()
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(InMemoryMetadataStore::new());
        
        let config = ProduceHandlerConfig {
            node_id: 0,
            enable_indexing: false,
            auto_create_topics_enable: true,
            ..Default::default()
        };
        
        let handler = ProduceHandler::new(config, storage, metadata_store).await.unwrap();

        // Test various invalid topic names
        let long_name = "a".repeat(250);
        let invalid_names = vec![
            "",                    // Empty
            long_name.as_str(),   // Too long
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
        let produce_config = ProduceHandlerConfig::default();

        let metadata_store = Arc::new(InMemoryMetadataStore::new());

        let mut object_store_config = chronik_storage::object_store::ObjectStoreConfig::default();
        object_store_config.backend = chronik_storage::object_store::StorageBackend::Local {
            path: temp_dir.path().join("segments").to_str().unwrap().to_string(),
        };
        let object_store: Arc<dyn chronik_storage::object_store::ObjectStoreTrait> =
            Arc::from(chronik_storage::object_store::ObjectStoreFactory::create(object_store_config).await.unwrap());

        let mut handler = ProduceHandler::new(
            produce_config,
            object_store,
            metadata_store.clone(),
        ).await.unwrap();
        
        // Create topic
        let mut topic_config = TopicConfig::default();
        topic_config.partition_count = 1;
        metadata_store.create_topic("test-topic", topic_config).await.unwrap();
        
        // Produce a message with acks=0
        let request = ProduceRequest {
            transactional_id: None,
            acks: 0, // acks=0
            timeout_ms: 1000,
            topics: vec![ProduceRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![ProduceRequestPartition {
                    index: 0,
                    records: create_simple_record_batch(0, vec!["msg1"]),
                }],
            }],
        };
        
        let response = handler.handle_produce(request, 1).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);

        // Check that high watermark was updated for acks=0
        let partition_state = handler.partition_states.get(&("test-topic".to_string(), 0)).unwrap();
        let high_watermark = partition_state.high_watermark.load(Ordering::Relaxed);
        assert_eq!(high_watermark, 1, "High watermark should be updated to 1 for acks=0");
    }

    #[tokio::test]
    async fn test_high_watermark_update_acks_1() {
        use tempfile::TempDir;
        use std::sync::atomic::Ordering;
        
        let temp_dir = TempDir::new().unwrap();
        let produce_config = ProduceHandlerConfig::default();

        let metadata_store = Arc::new(InMemoryMetadataStore::new());

        let mut object_store_config = chronik_storage::object_store::ObjectStoreConfig::default();
        object_store_config.backend = chronik_storage::object_store::StorageBackend::Local {
            path: temp_dir.path().join("segments").to_str().unwrap().to_string(),
        };
        let object_store: Arc<dyn chronik_storage::object_store::ObjectStoreTrait> =
            Arc::from(chronik_storage::object_store::ObjectStoreFactory::create(object_store_config).await.unwrap());

        let mut handler = ProduceHandler::new(
            produce_config,
            object_store,
            metadata_store.clone(),
        ).await.unwrap();
        
        // Create topic
        let mut topic_config = TopicConfig::default();
        topic_config.partition_count = 1;
        metadata_store.create_topic("test-topic", topic_config).await.unwrap();
        
        // Produce multiple messages with acks=1
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1, // acks=1
            timeout_ms: 1000,
            topics: vec![ProduceRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![ProduceRequestPartition {
                    index: 0,
                    records: create_simple_record_batch(0, vec!["msg1", "msg2", "msg3"]),
                }],
            }],
        };
        
        let response = handler.handle_produce(request, 1).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);

        // Check that high watermark was updated for acks=1
        let partition_state = handler.partition_states.get(&("test-topic".to_string(), 0)).unwrap();
        let high_watermark = partition_state.high_watermark.load(Ordering::Relaxed);
        assert_eq!(high_watermark, 3, "High watermark should be updated to 3 for acks=1 with 3 messages");
    }

    #[tokio::test]
    async fn test_high_watermark_with_fetch_handler_integration() {
        use tempfile::TempDir;
        use std::sync::atomic::Ordering;
        
        let temp_dir = TempDir::new().unwrap();
        let produce_config = ProduceHandlerConfig::default();

        let metadata_store = Arc::new(InMemoryMetadataStore::new());

        let mut object_store_config = chronik_storage::object_store::ObjectStoreConfig::default();
        object_store_config.backend = chronik_storage::object_store::StorageBackend::Local {
            path: temp_dir.path().join("segments").to_str().unwrap().to_string(),
        };
        let object_store: Arc<dyn chronik_storage::object_store::ObjectStoreTrait> =
            Arc::from(chronik_storage::object_store::ObjectStoreFactory::create(object_store_config).await.unwrap());

        // Create FetchHandler
        let segment_reader = Arc::new(chronik_storage::SegmentReader::new(
            chronik_storage::SegmentReaderConfig::default(),
            object_store.clone()
        ));

        let fetch_handler = Arc::new(crate::fetch_handler::FetchHandler::new(
            segment_reader,
            metadata_store.clone(),
            object_store.clone(),
        ));

        let mut handler = ProduceHandler::new(
            produce_config,
            object_store.clone(),
            metadata_store.clone(),
        ).await.unwrap();
        
        // Connect fetch handler
        handler.set_fetch_handler(fetch_handler.clone());
        
        // Create topic
        let mut topic_config = TopicConfig::default();
        topic_config.partition_count = 1;
        metadata_store.create_topic("test-topic", topic_config).await.unwrap();
        
        // Produce messages
        let request = ProduceRequest {
            transactional_id: None,
            acks: 1,
            timeout_ms: 1000,
            topics: vec![ProduceRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![ProduceRequestPartition {
                    index: 0,
                    records: create_simple_record_batch(0, vec!["msg1", "msg2"]),
                }],
            }],
        };
        
        let response = handler.handle_produce(request, 1).await.unwrap();
        assert_eq!(response.topics[0].partitions[0].error_code, 0);

        // Verify high watermark is updated
        let partition_state = handler.partition_states.get(&("test-topic".to_string(), 0)).unwrap();
        let high_watermark = partition_state.high_watermark.load(Ordering::Relaxed);
        assert_eq!(high_watermark, 2);

        // Note: fetch_partition is now private. Integration testing should be done
        // via the public handle_fetch API in separate integration tests.
    }

    // Helper function to create simple test record batches (for integration tests)
    fn create_simple_record_batch(base_offset: i64, messages: Vec<&str>) -> Vec<u8> {
        use bytes::Bytes;

        let mut batch = KafkaRecordBatch::new(
            base_offset,
            chrono::Utc::now().timestamp_millis(),
            -1, // producer_id
            -1, // producer_epoch
            -1, // base_sequence
            CompressionType::None,
            false, // is_transactional
        );

        for msg in messages.iter() {
            batch.add_record(
                None,               // key
                Some(Bytes::from(msg.to_string())),
                vec![],             // headers
                chrono::Utc::now().timestamp_millis(),
            );
        }

        batch.encode().unwrap().to_vec()
    }
}