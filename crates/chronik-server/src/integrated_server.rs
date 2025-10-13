//! Integrated Kafka server using chronik-ingest components.
//! 
//! This module properly integrates the complete, production-ready implementation
//! from chronik-ingest instead of reimplementing everything from scratch.

use anyhow::Result;
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
use chronik_common::metadata::{
    file_store::FileMetadataStore,
    traits::MetadataStore,
    ChronikMetaLogStore,
};

// Protocol types - BrokerMetadata is in chronik_common
use chronik_common::metadata::traits::BrokerMetadata;

/// Configuration for the integrated Kafka server
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
    /// Use WAL-based metadata store instead of file-based
    pub use_wal_metadata: bool,
    /// Enable background WAL indexing (WAL → Tantivy → Object Store)
    pub enable_wal_indexing: bool,
    /// WAL indexing interval in seconds
    pub wal_indexing_interval_secs: u64,
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
            use_wal_metadata: true, // Default to WAL-based metadata
            enable_wal_indexing: true, // Enable WAL→Tantivy indexing by default
            wal_indexing_interval_secs: 30, // Index every 30 seconds
        }
    }
}

/// Integrated Kafka server with full functionality
pub struct IntegratedKafkaServer {
    config: IntegratedServerConfig,
    kafka_handler: Arc<KafkaProtocolHandler>,
    metadata_store: Arc<dyn MetadataStore>,
    wal_indexer: Arc<WalIndexer>,
}

impl IntegratedKafkaServer {
    /// Create a new integrated Kafka server
    pub async fn new(config: IntegratedServerConfig) -> Result<Self> {
        info!("Initializing integrated Kafka server with chronik-ingest components");

        // Create data directory
        std::fs::create_dir_all(&config.data_dir)?;
        let segments_dir = format!("{}/segments", config.data_dir);
        std::fs::create_dir_all(&segments_dir)?;
        
        // Initialize metadata store based on configuration
        let metadata_store: Arc<dyn MetadataStore> = if config.use_wal_metadata {
            info!("Initializing WAL-based metadata store with real WAL adapter");

            // Create WAL configuration for metadata
            let wal_config = chronik_wal::config::WalConfig {
                enabled: true,
                data_dir: PathBuf::from(format!("{}/wal_metadata", config.data_dir)),
                segment_size: 50 * 1024 * 1024, // 50MB segments for metadata
                flush_interval_ms: 100, // Flush every 100ms
                flush_threshold: 1024 * 1024, // 1MB buffer threshold
                compression: chronik_wal::config::CompressionType::None,
                checkpointing: chronik_wal::config::CheckpointConfig {
                    enabled: true,
                    interval_records: 1000,
                    interval_bytes: 10 * 1024 * 1024,
                },
                rotation: chronik_wal::config::RotationConfig {
                    max_segment_size: 50 * 1024 * 1024,
                    max_segment_age_ms: 60 * 60 * 1000, // 1 hour
                    coordinate_with_storage: false,
                },
                fsync: chronik_wal::config::FsyncConfig {
                    enabled: true,
                    batch_size: 1,
                    batch_timeout_ms: 0,
                },
                ..Default::default()
            };

            // Create the real WAL adapter
            use chronik_storage::WalMetadataAdapter;
            let wal_adapter = Arc::new(WalMetadataAdapter::new(wal_config).await?);

            let metalog_store = ChronikMetaLogStore::new(
                wal_adapter,
                PathBuf::from(format!("{}/metalog_snapshots", config.data_dir)),
            ).await?;

            info!("Successfully initialized WAL-based metadata store with real WAL adapter");

            // Ensure __meta topic exists for metadata storage
            let metalog_store_arc = Arc::new(metalog_store);
            if metalog_store_arc.get_topic(chronik_common::metadata::METADATA_TOPIC).await?.is_none() {
                info!("Creating internal metadata topic: {}", chronik_common::metadata::METADATA_TOPIC);
                let meta_config = chronik_common::metadata::TopicConfig {
                    partition_count: 1, // Metadata topic only needs 1 partition
                    replication_factor: 1,
                    retention_ms: None, // Never delete metadata
                    segment_bytes: 50 * 1024 * 1024, // 50MB segments
                    config: {
                        let mut cfg = std::collections::HashMap::new();
                        cfg.insert("compression.type".to_string(), "snappy".to_string());
                        cfg.insert("cleanup.policy".to_string(), "compact".to_string());
                        cfg
                    },
                };
                metalog_store_arc.create_topic(chronik_common::metadata::METADATA_TOPIC, meta_config).await?;
                info!("Successfully created internal metadata topic");
            }

            metalog_store_arc
        } else {
            info!("Initializing file-based metadata store (legacy mode)");
            let metadata_dir = format!("{}/metadata", config.data_dir);
            std::fs::create_dir_all(&metadata_dir)?;

            match FileMetadataStore::new(&metadata_dir).await {
                Ok(store) => {
                    info!("Successfully initialized file-based metadata store (legacy) at {}", metadata_dir);
                    Arc::new(store)
                },
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to initialize file-based metadata store: {:?}. Please ensure the metadata directory {} is writable.", e, metadata_dir));
                }
            }
        };
        
        // Register this broker in metadata
        metadata_store.register_broker(BrokerMetadata {
            broker_id: config.node_id,
            host: config.advertised_host.clone(),
            port: config.advertised_port,
            rack: None,
            status: chronik_common::metadata::traits::BrokerStatus::Online,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }).await?;
        
        // Create object store configuration for local filesystem
        let object_store_config = ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: segments_dir.clone(),
            },
            bucket: "chronik".to_string(),
            prefix: None,
            connection: Default::default(),
            performance: Default::default(),
            retry: Default::default(),
            auth: chronik_storage::object_store::AuthConfig::None,
            default_metadata: None,
            encryption: None,
        };
        
        // Create object store
        let object_store = ObjectStoreFactory::create(object_store_config.clone()).await?;
        let object_store_arc: Arc<dyn ObjectStoreTrait> = Arc::from(object_store);
        
        // Create storage service with proper configuration
        let storage_config = IngestStorageConfig {
            object_store_config: object_store_config.clone(),
            segment_writer_config: SegmentWriterConfig {
                data_dir: PathBuf::from(&segments_dir),
                compression_codec: if config.enable_compression { 
                    "snappy".to_string() 
                } else { 
                    "none".to_string() 
                },
                max_segment_size: 256 * 1024 * 1024, // 256MB
                max_segment_age_secs: 30, // 30 seconds - flush on produce for immediate availability
                retention_period_secs: 7 * 24 * 3600, // 7 days  
                enable_cleanup: true,
            },
            segment_reader_config: SegmentReaderConfig::default(),
        };
        
        let storage_service = Arc::new(StorageService::new(storage_config.clone()).await?);
        
        // Initialize segment reader with object store
        let segment_reader = Arc::new(SegmentReader::new(
            SegmentReaderConfig::default(),
            storage_service.object_store(),
        ));
        
        // Configure produce handler with proper indexer config
        let indexer_config = chronik_search::realtime_indexer::RealtimeIndexerConfig {
            index_base_path: PathBuf::from(format!("{}/index", config.data_dir)),
            ..Default::default()
        };
        
        let flush_profile = crate::produce_handler::ProduceFlushProfile::auto_select();
        let produce_config = ProduceHandlerConfig {
            node_id: config.node_id,
            storage_config: storage_config.clone(),
            indexer_config,
            enable_indexing: config.enable_indexing,
            enable_idempotence: true,
            enable_transactions: false, // Start without transactions
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: flush_profile.linger_ms(),
            compression_type: if config.enable_compression {
                chronik_storage::kafka_records::CompressionType::Snappy
            } else {
                chronik_storage::kafka_records::CompressionType::None
            },
            request_timeout_ms: 120000,  // 120 seconds (increased from 30s to handle slow topic auto-creation)
            buffer_memory: flush_profile.buffer_memory(),
            auto_create_topics_enable: config.auto_create_topics,
            num_partitions: config.num_partitions,
            default_replication_factor: config.replication_factor,
            flush_profile,
        };

        // Create WAL configuration with default settings (v1.3.47: moved before ProduceHandler)
        use chronik_wal::{CompressionType, CheckpointConfig, RecoveryConfig, RotationConfig, FsyncConfig, WalConfig, WalManager};
        use chronik_wal::config::AsyncIoConfig;

        let wal_config = WalConfig {
            enabled: true,  // WAL is always enabled now as the default durability mechanism
            data_dir: PathBuf::from(format!("{}/wal", config.data_dir)),
            segment_size: 128 * 1024 * 1024, // 128MB segments
            flush_interval_ms: 100, // Sync to disk every 100ms for durability
            flush_threshold: 1024 * 1024, // 1MB buffer threshold
            compression: CompressionType::None,
            checkpointing: CheckpointConfig::default(),
            recovery: RecoveryConfig::default(),
            rotation: RotationConfig {
                max_segment_age_ms: 30 * 60 * 1000, // 30 minutes
                max_segment_size: 128 * 1024 * 1024, // 128MB
                coordinate_with_storage: true,
            },
            fsync: FsyncConfig {
                enabled: true,
                batch_size: 8,     // Batch up to 8 writes for efficiency
                batch_timeout_ms: 50, // Max 50ms latency for fsync batching
            },
            async_io: AsyncIoConfig::default(),
        };

        // CRITICAL FIX (v1.3.52): Clear segments directory before WAL recovery
        // This prevents duplicate messages during recovery. The flow is:
        // 1. Normal operation: Data → WAL (fsync) → Segments (async)
        // 2. Crash: Segments may have partial data
        // 3. Recovery: Clear segments, replay WAL → Segments
        // Without clearing, we'd have: WAL data + Old segment data = Duplicates
        info!("Clearing segments directory to prevent duplicates during WAL recovery...");
        let segments_dir_path = std::path::Path::new(&segments_dir);
        if segments_dir_path.exists() {
            match tokio::fs::remove_dir_all(segments_dir_path).await {
                Ok(_) => info!("Cleared segments directory successfully: {}", segments_dir),
                Err(e) => warn!("Failed to clear segments directory {}: {}. Recovery may have duplicates.", segments_dir, e),
            }
        }

        // Initialize WAL manager with recovery (v1.3.47+: lock-free WAL architecture)
        // WalManager uses DashMap internally - no external RwLock needed
        info!("Initializing WAL manager with recovery...");
        let wal_manager_recovered = WalManager::recover(&wal_config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to recover WAL: {}", e))?;
        let wal_manager = Arc::new(wal_manager_recovered);

        info!("WAL recovery complete - {} partitions loaded", wal_manager.get_partitions().len());

        // Initialize produce handler with inline WAL support (v1.3.47)
        let mut produce_handler_inner = ProduceHandler::new_with_wal(
            produce_config,
            object_store_arc.clone(),
            metadata_store.clone(),
            wal_manager.clone(),
        ).await?;

        let produce_handler_base = Arc::new(produce_handler_inner);

        // CRITICAL FIX (v1.3.52): Clear all partition buffers before WAL recovery
        // This prevents duplicate messages by ensuring we start with clean in-memory state.
        // Without this, WAL replay adds recovered data on top of any existing buffers,
        // causing 140%+ message recovery (e.g., 7000/5000 messages).
        info!("Clearing all partition buffers before WAL recovery...");
        if let Err(e) = produce_handler_base.clear_all_buffers().await {
            warn!("Failed to clear partition buffers: {}. Recovery may have duplicates.", e);
        }

        // CRITICAL (v1.3.48): Replay WAL to restore high watermarks after crash
        // Without this, all partitions have high_watermark=0 and consumers get no data
        let recovered_partitions = wal_manager.get_partitions();
        if !recovered_partitions.is_empty() {
            info!("Replaying WAL to restore high watermarks for {} partitions...", recovered_partitions.len());

            for tp in recovered_partitions {
                // Read all WAL records to find highest offset
                match wal_manager.read_from(&tp.topic, tp.partition, 0, usize::MAX).await {
                    Ok(records) if !records.is_empty() => {
                        use chronik_wal::record::WalRecord;
                        use chronik_storage::CanonicalRecord;

                        let mut max_offset: i64 = -1;

                        // Find max offset across all records (handle both V1 and V2 formats)
                        for record in &records {
                            match record {
                                WalRecord::V1 { offset, .. } => {
                                    if *offset > max_offset {
                                        max_offset = *offset;
                                    }
                                },
                                WalRecord::V2 { canonical_data, .. } => {
                                    // Deserialize CanonicalRecord to get offsets
                                    if let Ok(canonical) = bincode::deserialize::<CanonicalRecord>(canonical_data) {
                                        let last_offset = canonical.last_offset();
                                        if last_offset > max_offset {
                                            max_offset = last_offset;
                                        }
                                    }
                                }
                            }
                        }

                        if max_offset >= 0 {
                            // High watermark is max_offset + 1 (next offset to be written)
                            let high_watermark = (max_offset + 1) as u64;

                            // Restore partition state
                            if let Err(e) = produce_handler_base.restore_partition_state(&tp.topic, tp.partition, high_watermark).await {
                                warn!("Failed to restore partition state for {}-{}: {}", tp.topic, tp.partition, e);
                            } else {
                                info!("Restored {}-{}: {} records, high watermark = {}",
                                      tp.topic, tp.partition, records.len(), high_watermark);
                            }
                        }
                    },
                    Ok(_) => {
                        // No records, nothing to restore
                    },
                    Err(e) => {
                        warn!("Failed to read WAL for {}-{}: {}", tp.topic, tp.partition, e);
                    }
                }
            }

            info!("WAL replay complete");
        }

        // Wrap with WalProduceHandler for backward compatibility (will be removed in future)
        let wal_handler = Arc::new(WalProduceHandler::new_passthrough(
            wal_manager.clone(),
            produce_handler_base.clone()
        ));

        // WAL manager reference for FetchHandler
        let wal_manager_ref = wal_manager.clone();

        // Create FetchHandler with WAL and ProduceHandler integration (v1.3.39+)
        // FetchHandler needs ProduceHandler to get the real-time high watermark
        let fetch_handler = Arc::new(FetchHandler::new_with_wal(
            segment_reader.clone(),
            metadata_store.clone(),
            object_store_arc.clone(),
            wal_manager_ref,
            produce_handler_base.clone(),
        ));

        // Start background tasks for segment rotation on the base handler
        produce_handler_base.start_background_tasks().await;
        
        // Initialize Kafka protocol handler with all components
        // WAL is MANDATORY - no longer optional
        let kafka_handler = Arc::new(KafkaProtocolHandler::new(
            produce_handler_base.clone(),
            segment_reader,
            metadata_store.clone(),
            object_store_arc.clone(),
            fetch_handler.clone(),
            wal_handler.clone(),  // WAL is mandatory, not optional
            config.node_id,
            config.advertised_host.clone(),
            config.advertised_port,
            config.num_partitions,  // Pass default partition count from config
        ).await?);

        // Initialize WAL Indexer (always enabled for search integration)
        info!("Initializing WAL Indexer for background indexing (WAL → Tantivy → Object Store)");

        let indexer_config = WalIndexerConfig {
            interval_secs: config.wal_indexing_interval_secs,
            min_segment_age_secs: 10,
            max_segments_per_run: 100,
            delete_after_index: true,
            object_store: object_store_config.clone(),
            index_base_path: format!("{}/tantivy_indexes", config.data_dir),
            parallel_indexing: false, // Start with serial processing
            max_concurrent_tasks: 4,
            segment_index_path: Some(std::path::PathBuf::from(format!("{}/segment_index.json", config.data_dir))),
            segment_index_auto_save: true,
        };

        // Get WAL manager from wal_handler
        let wal_manager_ref = wal_handler.wal_manager().clone();

        let wal_indexer = Arc::new(WalIndexer::new(
            indexer_config,
            wal_manager_ref,
            object_store_arc.clone(),
        ));

        // Start the background indexing task
        wal_indexer.start().await
            .map_err(|e| anyhow::anyhow!("Failed to start WAL indexer: {}", e))?;

        info!("WAL Indexer started successfully (interval: {}s)", config.wal_indexing_interval_secs);
        info!("  Segment index will track Tantivy segments for future query optimization");

        info!("Integrated Kafka server initialized successfully");
        info!("  Node ID: {}", config.node_id);
        info!("  Advertised: {}:{}", config.advertised_host, config.advertised_port);
        info!("  Data dir: {}", config.data_dir);
        info!("  Auto-create topics: {}", config.auto_create_topics);
        info!("  Compression: {}", config.enable_compression);
        info!("  Indexing: {}", config.enable_indexing);
        info!("  WAL Indexing: {}", config.enable_wal_indexing);

        // Create default topic on startup to ensure clients can connect
        // This solves the chicken-and-egg problem where clients need at least one topic
        // in metadata responses before they can produce messages
        let server = Self {
            config,
            kafka_handler,
            metadata_store: metadata_store.clone(),
            wal_indexer,
        };
        
        // Create default topic
        info!("Creating default topic 'chronik-default' for client compatibility");
        if let Err(e) = server.ensure_default_topic().await {
            warn!("Failed to create default topic on startup: {:?}", e);
        }

        Ok(server)
    }

    /// Get reference to the WAL indexer for search integration
    pub fn get_wal_indexer(&self) -> Arc<WalIndexer> {
        self.wal_indexer.clone()
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
    
    /// Run the Kafka server
    pub async fn run(&self, bind_addr: &str) -> Result<()> {
        let listener = TcpListener::bind(bind_addr).await?;
        info!("Integrated Kafka server listening on {}", bind_addr);
        info!("Ready to accept Kafka client connections");

        // CRITICAL FIX (v1.3.56): Limit concurrent requests to prevent task overload
        // With acks=0, clients can send faster than server can process, causing
        // tokio runtime to spawn thousands of tasks that never get scheduled.
        // Semaphore provides backpressure at TCP level.
        let max_concurrent_requests = 1000; // Kafka default is 500-1000
        let request_semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent_requests));
        info!("Request concurrency limited to {} (prevents acks=0 overload)", max_concurrent_requests);

        loop {
            match listener.accept().await {
                Ok((mut socket, addr)) => {
                    debug!("New connection from {}", addr);

                    // Enable TCP_NODELAY to disable Nagle's algorithm for immediate sending
                    if let Err(e) = socket.set_nodelay(true) {
                        error!("Failed to set TCP_NODELAY for {}: {}", addr, e);
                    }

                    let kafka_handler = self.kafka_handler.clone();
                    let error_handler = Arc::new(ErrorHandler::new());
                    let semaphore = request_semaphore.clone();

                    // CRITICAL FIX (v1.3.60): Channel-based concurrent request processing with response ordering
                    // Split socket into read/write halves for concurrent operation
                    let (socket_reader, mut socket_writer) = socket.into_split();

                    // Elastic response channel capacity for burst traffic handling
                    // 10x semaphore limit provides buffer for burst traffic when handlers complete simultaneously
                    // With 1000 concurrent handlers + 100ms WAL batch window (ultra profile),
                    // handlers complete together and need elastic response buffering
                    // Channel now carries (sequence_number, correlation_id, response_data)
                    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<(u64, i32, Vec<u8>)>(10_000);

                    // Spawn response writer task with ordering guarantee
                    // CRITICAL: Responses MUST be sent in request order (Kafka protocol requirement)
                    tokio::spawn(async move {
                        use std::collections::BTreeMap;

                        // Buffer for out-of-order responses
                        let mut pending_responses: BTreeMap<u64, (i32, Vec<u8>)> = BTreeMap::new();
                        let mut next_sequence: u64 = 0;

                        while let Some((sequence, correlation_id, response_data)) = response_rx.recv().await {
                            // Add response to buffer
                            pending_responses.insert(sequence, (correlation_id, response_data));

                            // Send all consecutive responses starting from next_sequence
                            while let Some((corr_id, resp_data)) = pending_responses.remove(&next_sequence) {
                                // Write response to socket
                                if let Err(e) = socket_writer.write_all(&resp_data).await {
                                    error!("Failed to write response for sequence={}, correlation_id={}: {}",
                                           next_sequence, corr_id, e);
                                    return;
                                }
                                if let Err(e) = socket_writer.flush().await {
                                    error!("Failed to flush response for sequence={}, correlation_id={}: {}",
                                           next_sequence, corr_id, e);
                                    return;
                                }

                                tracing::debug!("Sent response: sequence={}, correlation_id={}", next_sequence, corr_id);
                                next_sequence += 1;
                            }
                        }
                    });

                    // Spawn a task to handle this connection with proper error handling
                    tokio::spawn(async move {
                        let mut request_buffer = vec![0; 65536];
                        let mut socket_reader = socket_reader;
                        let mut request_sequence: u64 = 0; // Sequence number for request ordering

                        loop {
                            // Read the size header (4 bytes)
                            let mut size_buf = [0u8; 4];
                            match socket_reader.read_exact(&mut size_buf).await {
                                Ok(_) => {},
                                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                                    debug!("Connection closed by {}", addr);
                                    break;
                                }
                                Err(e) => {
                                    crate::error_handler::handle_connection_error(e, addr).await;
                                    break;
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

                                // Continue to next request instead of breaking connection
                                continue;
                            }

                            if request_size == 0 || request_size > 10_000_000 {
                                error!("Invalid request size {} from {} (bytes: {:02x?})", request_size, addr, size_buf);

                                // Try to recover instead of breaking connection
                                // Clear the socket buffer and continue
                                let mut discard_buf = vec![0u8; 1024];
                                while let Ok(n) = socket_reader.read(&mut discard_buf).await {
                                    if n == 0 { break; }
                                }

                                // Continue to next request
                                continue;
                            }

                            // Resize buffer if needed
                            if request_buffer.len() < request_size {
                                request_buffer.resize(request_size, 0);
                            }

                            // Read the request body
                            match socket_reader.read_exact(&mut request_buffer[..request_size]).await {
                                Ok(_) => {},
                                Err(e) => {
                                    error!("Error reading request body from {}: {}", addr, e);
                                    break;
                                }
                            }

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
                            }

                            // Parse correlation ID from request for error handling
                            // Kafka protocol: API key (2), API version (2), correlation ID (4)
                            let correlation_id = if request_size >= 8 {
                                i32::from_be_bytes([
                                    request_buffer[4],
                                    request_buffer[5],
                                    request_buffer[6],
                                    request_buffer[7]
                                ])
                            } else {
                                0
                            };

                            // CRITICAL FIX (v1.3.60): Spawn request handler as separate task with sequence number
                            // This enables concurrent request processing while preserving response order
                            // Copy request data and sequence number, then spawn handler
                            let request_data = request_buffer[..request_size].to_vec();
                            let handler_clone = kafka_handler.clone();
                            let response_sender = response_tx.clone();
                            let semaphore_clone = semaphore.clone();
                            let error_handler_clone = error_handler.clone();
                            let addr_clone = addr;
                            let sequence = request_sequence; // Capture sequence number for this request

                            // Increment sequence for next request
                            request_sequence += 1;

                            tokio::spawn(async move {
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
                                    // Build complete response with size header
                                    let mut header_bytes = Vec::new();
                                    header_bytes.extend_from_slice(&response.header.correlation_id.to_be_bytes());

                                    if response.is_flexible {
                                        if response.api_key != chronik_protocol::parser::ApiKey::ApiVersions {
                                            header_bytes.push(0);
                                        }
                                    }

                                    let mut full_response = Vec::with_capacity(header_bytes.len() + response.body.len() + 4);
                                    let size = (header_bytes.len() + response.body.len()) as i32;
                                    full_response.extend_from_slice(&size.to_be_bytes());
                                    full_response.extend_from_slice(&header_bytes);
                                    full_response.extend_from_slice(&response.body);

                                    // Measure channel send delay to detect backpressure
                                    let send_start = std::time::Instant::now();
                                    // Send response with sequence number for ordering
                                    if let Err(e) = response_sender.send((sequence, response.header.correlation_id, full_response)).await {
                                        error!("Failed to send response to writer for addr={}: {}", addr_clone, e);
                                    }
                                    let send_duration = send_start.elapsed();
                                    if send_duration.as_millis() > 10 {
                                        warn!("Response channel send took {}ms (sequence={}, correlation_id={}) - channel backpressure detected",
                                              send_duration.as_millis(), sequence, response.header.correlation_id);
                                    }
                                }
                                Err(e) => {
                                    // Convert to ServerError for proper handling
                                    let server_error = ErrorHandler::from_anyhow(anyhow::anyhow!(e));
                                    let recovery = error_handler_clone.handle_error(
                                        server_error,
                                        &format!("request from {}", addr_clone)
                                    ).await;

                                    match recovery {
                                        ErrorRecovery::ReturnError(error_code) => {
                                            // Build proper error response with preserved correlation ID
                                            let error_response = error_handler_clone.build_error_response(
                                                error_code,
                                                correlation_id,
                                                0, // Unknown API key
                                                0, // Unknown API version
                                            );

                                            // Measure channel send delay for error responses too
                                            let send_start = std::time::Instant::now();
                                            // Send error response with sequence number for ordering
                                            if let Err(e) = response_sender.send((sequence, correlation_id, error_response)).await {
                                                error!("Failed to send error response: {}", e);
                                            }
                                            let send_duration = send_start.elapsed();
                                            if send_duration.as_millis() > 10 {
                                                warn!("Error response channel send took {}ms (correlation_id={}) - channel backpressure detected",
                                                      send_duration.as_millis(), correlation_id);
                                            }
                                        }
                                        ErrorRecovery::CloseConnection => {
                                            info!("Closing connection to {} due to error", addr_clone);
                                            // Channel will be dropped, closing the connection
                                        }
                                        ErrorRecovery::Throttle(ms) => {
                                            debug!("Throttling client {} for {}ms", addr_clone, ms);
                                            tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            });  // End of spawned handler task
                        }
                        
                        debug!("Connection handler for {} terminated", addr);
                    });
                }
                Err(e) => {
                    error!("Error accepting connection: {}", e);
                }
            }
        }
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
    
    #[tokio::test]
    async fn test_server_creation() {
        let config = IntegratedServerConfig {
            data_dir: "/tmp/chronik-test".to_string(),
            ..Default::default()
        };
        
        let server = IntegratedKafkaServer::new(config).await.unwrap();
        let stats = server.get_stats().await.unwrap();
        
        assert_eq!(stats.node_id, 1);
        assert_eq!(stats.brokers_count, 1);
    }
}