//! Integrated Kafka server using chronik-ingest components.
//!
//! This module properly integrates the complete, production-ready implementation
//! from chronik-ingest instead of reimplementing everything from scratch.

use anyhow::{Result, Context};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::io::IoSlice;
use tracing::{info, error, debug, warn};
use crate::error_handler::{ErrorHandler, ErrorCode, ErrorRecovery, ServerError};

// Use the local server components (moved from chronik-ingest)
use crate::kafka_handler::KafkaProtocolHandler;
use crate::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use crate::fetch_handler::FetchHandler;
use crate::storage::{StorageConfig as IngestStorageConfig, StorageService};
use crate::wal_integration::WalProduceHandler;

// Storage components
use chronik_storage::{
    ObjectStoreTrait, ObjectStoreConfig, ObjectStoreFactory,
    SegmentReader, SegmentReaderConfig,
    SegmentWriter, SegmentWriterConfig,
    WalIndexer, WalIndexerConfig,
};

// Metadata store
use chronik_common::metadata::traits::MetadataStore;

// Protocol types - BrokerMetadata is in chronik_common
use chronik_common::metadata::traits::BrokerMetadata;

/// Configuration for the integrated Kafka server
#[derive(Clone)]
pub struct IntegratedServerConfig {
    /// Node ID for this broker
    pub node_id: i32,
    /// Hostname to advertise to clients
    pub advertised_host: String,
    /// Port to advertise to clients
    pub advertised_port: i32,
    /// Data directory for storage
    pub data_dir: String,
    /// Enable real-time indexing
    pub enable_indexing: bool,
    /// Enable compression
    pub enable_compression: bool,
    /// Auto-create topics
    pub auto_create_topics: bool,
    /// Default number of partitions
    pub num_partitions: u32,
    /// Default replication factor
    pub replication_factor: u32,
    /// Enable background WAL indexing (WAL → Tantivy → Object Store)
    pub enable_wal_indexing: bool,
    /// WAL indexing interval in seconds
    pub wal_indexing_interval_secs: u64,
    /// Optional object store configuration (overrides default local storage)
    pub object_store_config: Option<ObjectStoreConfig>,
    /// Enable metadata disaster recovery (upload metadata WAL to S3)
    pub enable_metadata_dr: bool,
    /// Metadata upload interval in seconds
    pub metadata_upload_interval_secs: u64,
    /// Optional cluster configuration for Raft clustering
    pub cluster_config: Option<chronik_config::ClusterConfig>,
}

impl Default for IntegratedServerConfig {
    fn default() -> Self {
        Self {
            node_id: 1,  // Changed from 0 to 1 (controller_id of 0 means no controller in Kafka)
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
            data_dir: "./data".to_string(),
            enable_indexing: cfg!(feature = "search"), // Enable when search feature is compiled
            enable_compression: true,
            auto_create_topics: true,
            num_partitions: 3,
            replication_factor: 1,
            enable_wal_indexing: true, // Enable WAL→Tantivy indexing by default
            wal_indexing_interval_secs: 30, // Index every 30 seconds
            object_store_config: None, // Use default local storage unless specified
            enable_metadata_dr: true, // Enable metadata DR by default
            metadata_upload_interval_secs: 60, // Upload metadata every minute
            cluster_config: None, // No clustering by default (standalone mode)
        }
    }
}

/// Integrated Kafka server with full functionality
pub struct IntegratedKafkaServer {
    config: IntegratedServerConfig,
    kafka_handler: Arc<KafkaProtocolHandler>,
    metadata_store: Arc<dyn MetadataStore>,
    wal_indexer: Arc<WalIndexer>,
    metadata_uploader: Option<Arc<chronik_common::metadata::MetadataUploader>>,
    /// v2.2.7 Phase 5: Leader election per partition
    leader_elector: Option<Arc<crate::leader_election::LeaderElector>>,
}

impl Clone for IntegratedKafkaServer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            kafka_handler: self.kafka_handler.clone(),
            metadata_store: self.metadata_store.clone(),
            wal_indexer: self.wal_indexer.clone(),
            metadata_uploader: self.metadata_uploader.clone(),
            leader_elector: self.leader_elector.clone(),
        }
    }
}

impl IntegratedKafkaServer {
    /// Create a new integrated Kafka server from pre-initialized components
    /// This is used by the builder pattern to avoid the monolithic new() constructor
    pub(crate) fn new_from_components(
        config: IntegratedServerConfig,
        kafka_handler: Arc<KafkaProtocolHandler>,
        metadata_store: Arc<dyn MetadataStore>,
        wal_indexer: Arc<WalIndexer>,
        metadata_uploader: Option<Arc<chronik_common::metadata::MetadataUploader>>,
        leader_elector: Option<Arc<crate::leader_election::LeaderElector>>,
    ) -> Self {
        Self {
            config,
            kafka_handler,
            metadata_store,
            wal_indexer,
            metadata_uploader,
            leader_elector,
        }
    }

    /// Get reference to the WAL indexer for search integration
    pub fn get_wal_indexer(&self) -> Arc<WalIndexer> {
        self.wal_indexer.clone()
    }


    /// Assign existing partitions to cluster nodes (Phase 3.4)
    ///
    /// This method is called on cluster startup to ensure all partitions have assignments.
    /// It uses the round-robin strategy from `crates/chronik-common/src/partition_assignment.rs`.
    pub async fn assign_existing_partitions(&self, cluster_config: &chronik_config::ClusterConfig) -> Result<()> {
        use chronik_common::partition_assignment::PartitionAssignment as AssignmentManager;
        use chronik_common::metadata::PartitionAssignment;

        info!("Starting partition assignment for existing topics");

        // Get list of all cluster node IDs (convert from u64 to u32 for partition_assignment module)
        let node_ids: Vec<u32> = cluster_config.peers.iter().map(|p| p.id as u32).collect();
        if node_ids.is_empty() {
            warn!("No cluster nodes found, skipping partition assignment");
            return Ok(());
        }

        info!("Cluster nodes: {:?}", node_ids);

        // Get all topics
        let topics = self.metadata_store.list_topics().await?;
        if topics.is_empty() {
            info!("No topics found, skipping partition assignment");
            return Ok(());
        }

        info!("Found {} topics to assign", topics.len());

        // For each topic, assign partitions using round-robin
        for topic in topics {
            let topic_name = &topic.name;
            let partition_count = topic.config.partition_count;
            let replication_factor = topic.config.replication_factor.min(node_ids.len() as u32);

            info!(
                "Assigning topic '{}': {} partitions, RF={}",
                topic_name, partition_count, replication_factor
            );

            // Check if partitions are already assigned
            let existing_assignments = self.metadata_store.get_partition_assignments(topic_name).await?;
            if !existing_assignments.is_empty() {
                info!("Topic '{}' already has {} assignments, skipping", topic_name, existing_assignments.len());
                continue;
            }

            // Create assignment manager and assign partitions
            let mut assignment_mgr = AssignmentManager::new();
            assignment_mgr.add_topic(
                topic_name,
                partition_count as i32,
                replication_factor as i32,
                &node_ids,
            )?;

            // Convert to metadata PartitionAssignment and persist
            let topic_assignments = assignment_mgr.get_topic_assignments(topic_name);
            for (partition_id, partition_info) in topic_assignments {
                // Create SINGLE assignment per partition with FULL replica list
                let replicas: Vec<u64> = partition_info.replicas.iter().map(|&id| id as u64).collect();
                let leader_id = partition_info.leader as u64;

                let assignment = PartitionAssignment {
                    topic: topic_name.clone(),
                    partition: partition_id as u32,
                    broker_id: leader_id as i32,  // Deprecated field
                    is_leader: true,  // Deprecated field
                    replicas: replicas.clone(),
                    leader_id,
                };

                // Persist assignment via metadata store (will replicate via Raft)
                self.metadata_store.assign_partition(assignment).await?;

                info!(
                    "Assigned partition {}/{} to replicas {:?} (leader={})",
                    topic_name, partition_id, replicas, leader_id
                );
            }

            info!(
                "Successfully assigned {} partitions for topic '{}'",
                partition_count, topic_name
            );
        }

        info!("Partition assignment complete");
        Ok(())
    }

    /// Ensure a default topic exists for client connectivity
    async fn ensure_default_topic(&self) -> Result<()> {
        use chronik_common::metadata::TopicConfig;
        
        // Check if any topics exist
        let existing_topics = self.metadata_store.list_topics().await?;
        
        if existing_topics.is_empty() {
            info!("No topics exist, creating default topic 'chronik-default'");
            
            // Create topic config
            let topic_config = TopicConfig {
                partition_count: 1,
                replication_factor: 1,
                retention_ms: Some(604800000), // 7 days
                segment_bytes: 1073741824, // 1GB
                config: Default::default(),
            };
            
            self.metadata_store.create_topic("chronik-default", topic_config).await?;
            
            info!("Successfully created default topic 'chronik-default'");
        } else {
            info!("Topics already exist ({}), skipping default topic creation", existing_topics.len());
        }
        
        Ok(())
    }

    /// Gracefully shutdown the server, flushing all pending data
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down IntegratedKafkaServer...");

        // Step 1: Shutdown WAL handler first to seal all WAL segments to disk
        self.kafka_handler.get_wal_handler().shutdown().await;
        info!("WAL segments sealed to disk");

        // Step 2: Run WalIndexer once to upload sealed segments to object store
        // This ensures data is available for consumption even after WAL deletion
        info!("Running WalIndexer to upload sealed segments...");
        match self.get_wal_indexer().run_once().await {
            Ok(stats) => {
                info!(
                    "WalIndexer run complete: {} segments processed, {} records indexed",
                    stats.segments_processed, stats.records_indexed
                );
            }
            Err(e) => {
                error!("WalIndexer run failed: {}", e);
            }
        }

        info!("IntegratedKafkaServer shutdown complete");
        Ok(())
    }

    /// Get the metadata store (for updating RaftReplicaManager after initialization)
    pub fn metadata_store(&self) -> Arc<dyn MetadataStore> {
        self.metadata_store.clone()
    }

    // ========================================================================
    // Phase 2.14: run() Helper Methods - Connection and Request Processing
    // ========================================================================

    /// Setup connection semaphores for concurrency control
    ///
    /// Complexity: < 10
    fn setup_connection_semaphores() -> (Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>) {
        // CRITICAL FIX (v2.2.11): Separate semaphores for control vs data plane requests
        // Problem: When disk fills up, Produce handlers block on WAL writes → exhaust semaphore
        // → ApiVersionRequest can't acquire permit → timeout → client thinks broker is down
        // Solution: Separate semaphores ensure control requests never blocked by slow produces
        let max_produce_requests = 1000; // Data plane: Produce/Fetch
        let max_control_requests = 100;  // Control plane: ApiVersions, Metadata, Consumer Groups
        let produce_semaphore = Arc::new(tokio::sync::Semaphore::new(max_produce_requests));
        let control_semaphore = Arc::new(tokio::sync::Semaphore::new(max_control_requests));

        info!("Request concurrency: produce={}, control={} (prevents semaphore exhaustion)",
              max_produce_requests, max_control_requests);

        (produce_semaphore, control_semaphore)
    }

    /// Accept a new TCP connection from the listener
    ///
    /// Complexity: < 10
    async fn handle_connection_accept(
        listener: &TcpListener
    ) -> Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
        debug!("DEBUG: Before listener.accept() - waiting for connection...");

        match listener.accept().await {
            Ok((socket, addr)) => {
                debug!("New connection from {}", addr);
                Ok((socket, addr))
            }
            Err(e) => {
                error!("Error accepting connection: {}", e);
                Err(anyhow::anyhow!("Failed to accept connection: {}", e))
            }
        }
    }

    /// Configure socket settings and split into read/write halves
    ///
    /// Complexity: < 15
    fn setup_connection_socket(
        socket: tokio::net::TcpStream,
        addr: std::net::SocketAddr,
    ) -> Result<(
        tokio::net::tcp::OwnedReadHalf,
        Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>
    )> {
        // Enable TCP_NODELAY to disable Nagle's algorithm for immediate sending
        if let Err(e) = socket.set_nodelay(true) {
            error!("Failed to set TCP_NODELAY for {}: {}", addr, e);
        }

        // CRITICAL FIX (v2.2.13): Split socket for concurrent read/write without async channel
        // Problem: Arc<Mutex<socket>> causes deadlock between read loop and write tasks
        // Solution: Split socket into read/write halves, clone write-half for direct writes
        // Benefits: No async channel complexity, no SOCKET WRITER task, concurrent operations
        let (socket_reader, socket_writer) = socket.into_split();
        let socket_writer = Arc::new(tokio::sync::Mutex::new(socket_writer));

        Ok((socket_reader, socket_writer))
    }

    /// Read and validate a Kafka request frame from the socket
    ///
    /// Complexity: < 25
    async fn read_request_frame(
        socket_reader: &mut tokio::net::tcp::OwnedReadHalf,
        request_buffer: &mut Vec<u8>,
        addr: std::net::SocketAddr,
    ) -> Result<Option<usize>> {
        // Read the size header (4 bytes)
        let mut size_buf = [0u8; 4];
        // v2.2.14: Removed hot-path diagnostic logs (were filling disk at 50MB/sec)

        let read_result = socket_reader.read_exact(&mut size_buf).await;

        match read_result {
            Ok(_) => {
                // v2.2.14: Removed diagnostic log from hot path
            },
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("Connection closed by {}", addr);
                return Ok(None); // Signal clean disconnect
            }
            Err(e) => {
                crate::error_handler::handle_connection_error(e, addr).await;
                return Ok(None); // Signal error disconnect
            }
        }

        let request_size = i32::from_be_bytes(size_buf) as usize;

        // Check for suspicious size values that might indicate protocol mismatch
        // Common issue: "5.0\0" (0x35, 0x2e, 0x30, 0x00) = 892219392 bytes
        if request_size > 100_000_000 {
            // This is likely not a Kafka request - might be version string or other protocol
            let size_str = String::from_utf8_lossy(&size_buf);
            warn!("Suspicious request size {} ({} bytes) from {} - might be protocol mismatch. Bytes as string: '{}', hex: {:02x?}",
                  request_size, request_size, addr, size_str, size_buf);

            // Try to recover by consuming any remaining data and continuing
            // Read and discard up to 1KB of data to clear the buffer
            let mut discard_buf = vec![0u8; 1024];
            let _ = socket_reader.read(&mut discard_buf).await;

            // Return None to skip this request
            return Ok(None);
        }

        if request_size == 0 || request_size > 10_000_000 {
            error!("Invalid request size {} from {} (bytes: {:02x?})", request_size, addr, size_buf);

            // Try to recover instead of breaking connection
            // Clear the socket buffer and continue
            let mut discard_buf = vec![0u8; 1024];
            loop {
                let n = match socket_reader.read(&mut discard_buf).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 { break; }
            }

            // Return None to skip this request
            return Ok(None);
        }

        // Resize buffer if needed
        if request_buffer.len() < request_size {
            request_buffer.resize(request_size, 0);
        }

        // Read the request body
        let body_read_result = socket_reader.read_exact(&mut request_buffer[..request_size]).await;
        match body_read_result {
            Ok(_) => Ok(Some(request_size)),
            Err(e) => {
                error!("Error reading request body from {}: {}", addr, e);
                Ok(None) // Signal error
            }
        }
    }

    /// Parse request metadata (API key, version, correlation ID)
    ///
    /// Complexity: < 20
    fn parse_request_metadata(
        request_buffer: &[u8],
        request_size: usize,
    ) -> (i16, i16, i32) {
        // Debug logging for request identification
        if request_size >= 8 {
            let api_key = i16::from_be_bytes([request_buffer[0], request_buffer[1]]);
            let api_version = i16::from_be_bytes([request_buffer[2], request_buffer[3]]);
            tracing::debug!(
                "Received request: api_key={}, api_version={}, size={} bytes",
                api_key, api_version, request_size
            );

            if api_key == 0 {
                tracing::debug!("Produce request detected: version={}, size={} bytes", api_version, request_size);
            }

            // Parse correlation ID from request for error handling
            // Kafka protocol: API key (2), API version (2), correlation ID (4)
            let correlation_id = i32::from_be_bytes([
                request_buffer[4],
                request_buffer[5],
                request_buffer[6],
                request_buffer[7]
            ]);

            (api_key, api_version, correlation_id)
        } else {
            (0, 0, 0) // Fallback for malformed requests
        }
    }

    /// Select appropriate semaphore based on request API key
    ///
    /// Complexity: < 10
    fn select_semaphore_for_request(
        request_data: &[u8],
        produce_sem: &Arc<tokio::sync::Semaphore>,
        control_sem: &Arc<tokio::sync::Semaphore>,
    ) -> Arc<tokio::sync::Semaphore> {
        // CRITICAL FIX (v2.2.11): Select semaphore based on API key
        // Parse API key to determine if this is control or data plane request
        let api_key = if request_data.len() >= 2 {
            i16::from_be_bytes([request_data[0], request_data[1]])
        } else {
            -1 // Invalid, use control semaphore for safety
        };

        // Data plane (slow, can block on disk I/O): Produce=0, Fetch=1
        // Control plane (fast, never block): ApiVersions=18, Metadata=3, etc.
        match api_key {
            0 | 1 => produce_sem.clone(), // Produce, Fetch
            _ => control_sem.clone(),     // Everything else (control plane)
        }
    }

    /// Build and write response to socket
    ///
    /// Complexity: < 25 (response construction + write)
    async fn build_and_write_response(
        response: chronik_protocol::handler::Response,
        socket_writer: &Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
        addr: std::net::SocketAddr,
    ) -> Result<()> {
        // Build complete response with size header
        let mut header_bytes = Vec::new();
        header_bytes.extend_from_slice(&response.header.correlation_id.to_be_bytes());

        // TODO(v2.2.7): CRITICAL BUG - OffsetCommit v8 flexible protocol issue
        // Current: Only adds tagged fields for non-ApiVersions flexible responses
        // Bug: Consumers get "Protocol read buffer underflow" for OffsetCommit v8
        // Need to research correct format from KIP-482 and working APIs
        let _tagged_byte_added = if response.is_flexible {
            if response.api_key != chronik_protocol::parser::ApiKey::ApiVersions {
                header_bytes.push(0);
                true
            } else {
                false
            }
        } else {
            false
        };

        let mut full_response = Vec::with_capacity(header_bytes.len() + response.body.len() + 4);
        let size = (header_bytes.len() + response.body.len()) as i32;
        full_response.extend_from_slice(&size.to_be_bytes());
        full_response.extend_from_slice(&header_bytes);
        full_response.extend_from_slice(&response.body);

        // DETAILED LOGGING FOR DEBUGGING
        tracing::debug!(
            "[DIRECT WRITE] Built response for API {:?}, correlation_id={}, total_size={} bytes (header={}, body={})",
            response.api_key, response.header.correlation_id, full_response.len(), header_bytes.len(), response.body.len()
        );

        // CRITICAL DEBUGGING: Log full OffsetCommit response bytes
        if matches!(response.api_key, chronik_protocol::parser::ApiKey::OffsetCommit) {
            tracing::error!(
                "!!! OFFSETCOMMIT FULL RESPONSE (all {} bytes): {:02x?}",
                full_response.len(), full_response
            );
            // Verify structure
            if full_response.len() >= 12 {
                let msg_size = i32::from_be_bytes([full_response[0], full_response[1], full_response[2], full_response[3]]);
                let corr_id = i32::from_be_bytes([full_response[4], full_response[5], full_response[6], full_response[7]]);
                let throttle = i32::from_be_bytes([full_response[8], full_response[9], full_response[10], full_response[11]]);
                tracing::error!(
                    "!!! OFFSETCOMMIT STRUCTURE: msg_size={}, correlation_id={}, throttle_time={}",
                    msg_size, corr_id, throttle
                );
                if full_response.len() >= 16 {
                    let topics_len = i32::from_be_bytes([full_response[12], full_response[13], full_response[14], full_response[15]]);
                    tracing::error!("!!! OFFSETCOMMIT topics_array_len={}", topics_len);
                } else {
                    tracing::error!("!!! OFFSETCOMMIT ERROR: Response too short to include topics array! Only {} bytes", full_response.len());
                }
            } else {
                tracing::error!("!!! OFFSETCOMMIT ERROR: Full response too short! Only {} bytes", full_response.len());
            }
        }

        // CRITICAL FIX (v2.2.13): Write response directly to socket write-half (no channel)
        // Socket is split: write-half is exclusive for writing, no contention with read loop
        let write_start = std::time::Instant::now();
        let write_result = {
            let mut writer_guard = socket_writer.lock().await;
            writer_guard.write_all(&full_response).await
        };
        let write_duration = write_start.elapsed();

        match write_result {
            Ok(_) => {
                tracing::debug!(
                    "[DIRECT WRITE] Successfully wrote response for API {:?}, correlation_id={}, size={} bytes, latency={}ms",
                    response.api_key, response.header.correlation_id, full_response.len(), write_duration.as_millis()
                );
            }
            Err(e) => {
                error!("Failed to write response to socket for addr={}: {}", addr, e);
                return Err(anyhow::anyhow!("Socket write failed: {}", e));
            }
        }

        if write_duration.as_millis() > 100 {
            warn!("Direct socket write took {}ms (correlation_id={}) - slow write detected",
                  write_duration.as_millis(), response.header.correlation_id);
        }

        Ok(())
    }

    /// Handle request processing errors with recovery strategies
    ///
    /// Complexity: < 25 (error conversion + recovery)
    async fn handle_request_error_recovery(
        error: chronik_common::Error,
        error_handler: &Arc<ErrorHandler>,
        correlation_id: i32,
        request_data: &[u8],
        socket_writer: &Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
        addr: std::net::SocketAddr,
    ) -> ErrorRecovery {
        // Convert to ServerError for proper handling (chronik_common::Error → anyhow::Error → ServerError)
        let server_error = ErrorHandler::from_anyhow(anyhow::anyhow!("{}", error));
        let recovery = error_handler.handle_error(
            server_error,
            &format!("request from {}", addr)
        ).await;

        match recovery {
            ErrorRecovery::ReturnError(error_code) => {
                // Parse API key and version from request_data for proper error response
                let (api_key, api_version) = if request_data.len() >= 4 {
                    (
                        i16::from_be_bytes([request_data[0], request_data[1]]),
                        i16::from_be_bytes([request_data[2], request_data[3]])
                    )
                } else {
                    (0, 0) // Fallback for malformed requests
                };

                // Build proper error response with preserved correlation ID and API info
                let error_response = error_handler.build_error_response(
                    error_code,
                    correlation_id,
                    api_key,
                    api_version,
                );

                // CRITICAL FIX (v2.2.13): Write error response directly to socket write-half (no channel)
                let write_start = std::time::Instant::now();
                let write_result = {
                    let mut writer_guard = socket_writer.lock().await;
                    writer_guard.write_all(&error_response).await
                };
                let write_duration = write_start.elapsed();

                match write_result {
                    Ok(_) => {
                        tracing::debug!(
                            "[DIRECT WRITE] Successfully wrote error response, correlation_id={}, size={} bytes",
                            correlation_id, error_response.len()
                        );
                    }
                    Err(e) => {
                        error!("Failed to write error response: {}", e);
                    }
                }

                if write_duration.as_millis() > 100 {
                    warn!("Error response socket write took {}ms - slow write detected", write_duration.as_millis());
                }

                ErrorRecovery::ReturnError(error_code)
            }
            ErrorRecovery::CloseConnection => {
                info!("Closing connection to {} due to error", addr);
                ErrorRecovery::CloseConnection
            }
            ErrorRecovery::Throttle(ms) => {
                debug!("Throttling client {} for {}ms", addr, ms);
                tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                ErrorRecovery::Throttle(ms)
            }
            other => other,
        }
    }

    // ========================================================================
    // End of Phase 2.14 Helper Methods
    // ========================================================================

    /// Run the Kafka server (with optional shutdown signal)
    ///
    /// Complexity: < 20 (clean orchestration using helper methods)
    pub async fn run(&self, bind_addr: &str) -> Result<()> {
        let listener = TcpListener::bind(bind_addr).await?;
        info!("Integrated Kafka server listening on {}", bind_addr);
        info!("Ready to accept Kafka client connections");

        let (produce_semaphore, control_semaphore) = Self::setup_connection_semaphores();

        debug!("DEBUG: Entering accept loop - ready to accept connections");
        loop {
            match Self::handle_connection_accept(&listener).await {
                Ok((socket, addr)) => {
                    let (socket_reader, socket_writer) = match Self::setup_connection_socket(socket, addr) {
                        Ok(pair) => pair,
                        Err(e) => {
                            error!("Failed to setup socket for {}: {}", addr, e);
                            continue;
                        }
                    };

                    let kafka_handler = self.kafka_handler.clone();
                    let error_handler = Arc::new(ErrorHandler::new());
                    let produce_sem = produce_semaphore.clone();
                    let control_sem = control_semaphore.clone();

                    // v2.2.14: Removed diagnostic logs from hot path
                    // Spawn a task to handle this connection with proper error handling
                    tokio::spawn(async move {
                        let mut request_buffer = vec![0; 65536];
                        let mut socket_reader = socket_reader;

                        loop {
                            // Read request frame
                            let request_size = match Self::read_request_frame(&mut socket_reader, &mut request_buffer, addr).await {
                                Ok(Some(size)) => size,
                                Ok(None) => continue, // Skip this request (error or protocol mismatch)
                                Err(_) => break, // Fatal error, close connection
                            };

                            // Parse request metadata
                            let (_api_key, _api_version, correlation_id) = Self::parse_request_metadata(&request_buffer, request_size);

                            // CRITICAL FIX (v2.2.13): Process request and write response directly to socket write-half
                            // Socket split: read-half for reading, write-half cloned for concurrent writes
                            // Copy request data, then spawn handler
                            let request_data = request_buffer[..request_size].to_vec();
                            let handler_clone = kafka_handler.clone();
                            let socket_writer_clone = socket_writer.clone();
                            let produce_sem_clone = produce_sem.clone();
                            let control_sem_clone = control_sem.clone();
                            let error_handler_clone = error_handler.clone();
                            let addr_clone = addr;

                            tokio::spawn(async move {
                                // Select semaphore based on API key
                                let semaphore_clone = Self::select_semaphore_for_request(
                                    &request_data,
                                    &produce_sem_clone,
                                    &control_sem_clone
                                );

                                // Acquire semaphore to limit concurrent handlers
                                let _permit = match semaphore_clone.acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => {
                                        error!("Failed to acquire semaphore for request");
                                        return;
                                    }
                                };

                                // Handle request using the integrated handler
                                match handler_clone.handle_request(&request_data).await {
                                    Ok(response) => {
                                        // Build and write response
                                        if let Err(e) = Self::build_and_write_response(response, &socket_writer_clone, addr_clone).await {
                                            error!("Failed to build/write response: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        // Handle error with recovery strategy
                                        Self::handle_request_error_recovery(
                                            e,
                                            &error_handler_clone,
                                            correlation_id,
                                            &request_data,
                                            &socket_writer_clone,
                                            addr_clone
                                        ).await;
                                    }
                                }
                            });  // End of spawned handler task
                        }

                        debug!("Connection handler for {} terminated", addr);
                    });
                }
                Err(_) => {
                    // Error already logged in handle_connection_accept
                }
            }
        }
    }

    /// Flush all partition buffers to ensure data durability before shutdown
    pub async fn flush_all_partitions(&self) -> Result<()> {
        info!("Flushing all partition buffers to storage...");
        self.kafka_handler.flush_all_partitions().await?;
        info!("All partitions flushed successfully");
        Ok(())
    }

    /// Get server statistics
    pub async fn get_stats(&self) -> Result<ServerStats> {
        let topics = self.metadata_store.list_topics().await?;
        let brokers = self.metadata_store.list_brokers().await?;
        
        Ok(ServerStats {
            node_id: self.config.node_id,
            topics_count: topics.len(),
            brokers_count: brokers.len(),
            advertised_address: format!("{}:{}", 
                self.config.advertised_host,
                self.config.advertised_port
            ),
        })
    }

    /// Restore high watermarks from segment metadata on startup
    ///
    /// This is critical for recovery after WAL deletion. Without this, the server
    /// would not know what offsets exist in segments and would report empty partitions.
    async fn restore_high_watermarks_from_segments(
        metadata_store: &Arc<dyn MetadataStore>,
    ) -> Result<()> {
        use std::collections::HashMap;

        // Get all topics
        let topics = metadata_store.list_topics().await
            .map_err(|e| anyhow::anyhow!("Failed to list topics: {}", e))?;

        if topics.is_empty() {
            info!("No topics found, skipping high watermark restoration");
            return Ok(());
        }

        // For each topic, find the maximum offset from segments
        let mut high_watermarks: HashMap<(String, u32), i64> = HashMap::new();

        for topic in topics {
            // List all segments for this topic
            let segments = metadata_store.list_segments(&topic.name, None).await
                .map_err(|e| anyhow::anyhow!("Failed to list segments for topic {}: {}", topic.name, e))?;

            if segments.is_empty() {
                continue;
            }

            // Find maximum end_offset for each partition
            for segment in segments {
                let key = (segment.topic.clone(), segment.partition);
                let current_max = high_watermarks.get(&key).copied().unwrap_or(-1);

                // High watermark should be one past the last offset in the segment
                let segment_high_watermark = segment.end_offset + 1;

                if segment_high_watermark > current_max {
                    high_watermarks.insert(key, segment_high_watermark);
                }
            }
        }

        // Update high watermarks in metadata store
        let mut restored_count = 0;
        for ((topic, partition), high_watermark) in high_watermarks {
            // Check if we already have a high watermark set (from WAL recovery)
            if let Ok(Some((existing_hwm, _))) = metadata_store.get_partition_offset(&topic, partition).await {
                if existing_hwm >= high_watermark {
                    // Already have a higher or equal watermark from WAL, don't overwrite
                    debug!(
                        topic = %topic,
                        partition = partition,
                        existing_hwm = existing_hwm,
                        segment_hwm = high_watermark,
                        "Skipping watermark restore (existing is higher)"
                    );
                    continue;
                }
            }

            // Set the high watermark (log_start_offset = 0 for now)
            metadata_store.update_partition_offset(&topic, partition, high_watermark, 0).await
                .map_err(|e| anyhow::anyhow!("Failed to update high watermark for {}:{}: {}", topic, partition, e))?;

            info!(
                topic = %topic,
                partition = partition,
                high_watermark = high_watermark,
                "Restored high watermark from segments"
            );
            restored_count += 1;
        }

        info!(
            restored_count = restored_count,
            "High watermark restoration complete"
        );

        Ok(())
    }
}

/// Server statistics
#[derive(Debug, Clone)]
pub struct ServerStats {
    pub node_id: i32,
    pub topics_count: usize,
    pub brokers_count: usize,
    pub advertised_address: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrated_server::IntegratedKafkaServerBuilder;
    
    #[tokio::test]
    async fn test_server_creation() {
        let config = IntegratedServerConfig {
            data_dir: "/tmp/chronik-test".to_string(),
            ..Default::default()
        };

        // Use builder pattern (single-node mode, no Raft cluster)
        let server = IntegratedKafkaServerBuilder::new(config)
            .build()
            .await
            .unwrap();
        let stats = server.get_stats().await.unwrap();

        assert_eq!(stats.node_id, 1);
        assert_eq!(stats.brokers_count, 1);
    }
}