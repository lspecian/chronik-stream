//! Integrated Kafka server using chronik-ingest components.
//! 
//! This module properly integrates the complete, production-ready implementation
//! from chronik-ingest instead of reimplementing everything from scratch.

use anyhow::Result;
use std::sync::Arc;
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
    /// Enable dual storage (raw Kafka + indexed records for search)
    pub enable_dual_storage: bool,
    /// Use WAL-based metadata store instead of file-based
    pub use_wal_metadata: bool,
}

impl Default for IntegratedServerConfig {
    fn default() -> Self {
        Self {
            node_id: 1,  // Changed from 0 to 1 (controller_id of 0 means no controller in Kafka)
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
            data_dir: "./data".to_string(),
            enable_indexing: false, // Disabled for now to avoid Tantivy complexity
            enable_compression: true,
            auto_create_topics: true,
            num_partitions: 3,
            replication_factor: 1,
            enable_dual_storage: false, // Default to raw-only for better performance
            use_wal_metadata: true, // Default to WAL-based metadata
        }
    }
}

/// Integrated Kafka server with full functionality
pub struct IntegratedKafkaServer {
    config: IntegratedServerConfig,
    kafka_handler: Arc<KafkaProtocolHandler>,
    metadata_store: Arc<dyn MetadataStore>,
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
                enable_dual_storage: config.enable_dual_storage,
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
        
        let produce_config = ProduceHandlerConfig {
            node_id: config.node_id,
            storage_config: storage_config.clone(),
            indexer_config,
            enable_indexing: config.enable_indexing,
            enable_idempotence: true,
            enable_transactions: false, // Start without transactions
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: 10,
            compression_type: if config.enable_compression {
                chronik_storage::kafka_records::CompressionType::Snappy
            } else {
                chronik_storage::kafka_records::CompressionType::None
            },
            request_timeout_ms: 30000,
            buffer_memory: 32 * 1024 * 1024, // 32MB
            auto_create_topics_enable: config.auto_create_topics,
            num_partitions: config.num_partitions,
            default_replication_factor: config.replication_factor,
        };
        
        // Create FetchHandler first
        let fetch_handler = Arc::new(FetchHandler::new(
            segment_reader.clone(),
            metadata_store.clone(),
            object_store_arc.clone(),
        ));
        
        // Initialize produce handler with object store directly
        let mut produce_handler_inner = ProduceHandler::new(
            produce_config,
            object_store_arc.clone(),
            metadata_store.clone(),
        ).await?;
        
        // Connect the fetch handler to the produce handler
        produce_handler_inner.set_fetch_handler(fetch_handler.clone());
        let produce_handler_base = Arc::new(produce_handler_inner);
        
        // Create WAL configuration with default settings
        use chronik_wal::{CompressionType, CheckpointConfig, RecoveryConfig, RotationConfig, FsyncConfig, WalConfig};
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
        
        // Wrap the produce handler with WAL for durability
        let wal_handler = Arc::new(WalProduceHandler::new(wal_config, produce_handler_base.clone()).await
            .map_err(|e| anyhow::anyhow!("Failed to initialize WAL handler: {}", e))?);
        
        // Recover from WAL on startup
        info!("Recovering from WAL...");
        wal_handler.recover().await
            .map_err(|e| anyhow::anyhow!("Failed to recover from WAL: {}", e))?;
        
        // Store the WAL handler for later use (will be used in kafka_handler)
        // For now, we pass the base handler to KafkaProtocolHandler
        // and we'll integrate WAL at the kafka_handler level
        
        // Start background tasks for segment rotation on the base handler
        produce_handler_base.start_background_tasks().await;
        
        // Initialize Kafka protocol handler with all components
        // WAL is MANDATORY - no longer optional
        let kafka_handler = Arc::new(KafkaProtocolHandler::new(
            produce_handler_base.clone(),
            segment_reader,
            metadata_store.clone(),
            object_store_arc.clone(),
            fetch_handler,
            wal_handler.clone(),  // WAL is mandatory, not optional
            config.node_id,
            config.advertised_host.clone(),
            config.advertised_port,
        ).await?);
        
        info!("Integrated Kafka server initialized successfully");
        info!("  Node ID: {}", config.node_id);
        info!("  Advertised: {}:{}", config.advertised_host, config.advertised_port);
        info!("  Data dir: {}", config.data_dir);
        info!("  Auto-create topics: {}", config.auto_create_topics);
        info!("  Compression: {}", config.enable_compression);
        info!("  Indexing: {}", config.enable_indexing);
        info!("  Dual storage: {}", config.enable_dual_storage);
        
        // Create default topic on startup to ensure clients can connect
        // This solves the chicken-and-egg problem where clients need at least one topic
        // in metadata responses before they can produce messages
        let server = Self {
            config,
            kafka_handler,
            metadata_store: metadata_store.clone(),
        };
        
        // Create default topic
        info!("Creating default topic 'chronik-default' for client compatibility");
        if let Err(e) = server.ensure_default_topic().await {
            warn!("Failed to create default topic on startup: {:?}", e);
        }
        
        Ok(server)
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
                    
                    // Spawn a task to handle this connection with proper error handling
                    tokio::spawn(async move {
                        let mut request_buffer = vec![0; 65536];
                        
                        loop {
                            // Read the size header (4 bytes)
                            let mut size_buf = [0u8; 4];
                            match socket.read_exact(&mut size_buf).await {
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

                            // Check for non-Kafka protocols
                            // Common patterns:
                            // - HTTP: "GET ", "POST", "PUT ", "HEAD", "HTTP"
                            // - Version strings: "5.0\0" (0x35 0x2E 0x30 0x00) (seen from Kafka UI)
                            // - TLS handshake: 0x16 (22) as first byte

                            // Specifically check for "5.0\0" pattern from Kafka UI
                            if size_buf == [0x35, 0x2e, 0x30, 0x00] { // "5.0\0"
                                warn!("Kafka UI version string detected from {}: '5.0' (bytes: {:02x?})",
                                      addr, size_buf);

                                // Send a minimal HTTP response to inform the client
                                let http_response = b"HTTP/1.1 400 Bad Request\r\n\
                                                     Content-Type: text/plain\r\n\
                                                     Content-Length: 115\r\n\
                                                     \r\n\
                                                     This is a Kafka protocol server. Kafka UI sent '5.0' version string instead of Kafka protocol. Check UI config.";

                                let _ = socket.write_all(http_response).await;
                                let _ = socket.flush().await;
                                break;
                            }

                            // Check for HTTP methods - be specific to avoid false positives
                            // HTTP methods: GET, POST, PUT, HEAD, DELETE, PATCH, OPTIONS
                            let is_http = match &size_buf {
                                [0x47, 0x45, 0x54, 0x20] => true, // "GET "
                                [0x50, 0x4f, 0x53, 0x54] => true, // "POST"
                                [0x50, 0x55, 0x54, 0x20] => true, // "PUT "
                                [0x48, 0x45, 0x41, 0x44] => true, // "HEAD"
                                [0x48, 0x54, 0x54, 0x50] => true, // "HTTP"
                                _ => false,
                            };

                            if is_http {
                                // This looks like HTTP or a version string
                                let preview = String::from_utf8_lossy(&size_buf);
                                warn!("Non-Kafka protocol detected from {}: '{}' (bytes: {:02x?})",
                                      addr, preview, size_buf);

                                // Send a minimal HTTP response to inform the client
                                let http_response = b"HTTP/1.1 400 Bad Request\r\n\
                                                     Content-Type: text/plain\r\n\
                                                     Content-Length: 89\r\n\
                                                     \r\n\
                                                     This is a Kafka protocol server. Please use a Kafka client, not HTTP or version strings.";

                                let _ = socket.write_all(http_response).await;
                                let _ = socket.flush().await;
                                break;
                            }

                            // Check for TLS handshake (client trying SSL on non-SSL port)
                            if size_buf[0] == 0x16 {
                                warn!("TLS handshake detected from {} on non-TLS port", addr);
                                // Just close the connection, TLS negotiation won't work
                                break;
                            }

                            let request_size = i32::from_be_bytes(size_buf) as usize;
                            if request_size == 0 || request_size > 10_000_000 {
                                error!("Invalid request size {} from {} (bytes: {:02x?})",
                                       request_size, addr, size_buf);
                                break;
                            }
                            
                            // Resize buffer if needed
                            if request_buffer.len() < request_size {
                                request_buffer.resize(request_size, 0);
                            }
                            
                            // Read the request body
                            match socket.read_exact(&mut request_buffer[..request_size]).await {
                                Ok(_) => {},
                                Err(e) => {
                                    error!("Error reading request body from {}: {}", addr, e);
                                    break;
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
                            
                            // Handle request using the integrated handler
                            match kafka_handler.handle_request(&request_buffer[..request_size]).await {
                                Ok(response) => {
                                    // Build complete response with size header
                                    // We need to handle flexible versions properly
                                    let mut header_bytes = Vec::new();
                                    
                                    // Add correlation ID
                                    header_bytes.extend_from_slice(&response.header.correlation_id.to_be_bytes());
                                    
                                    // For flexible versions, add tagged fields after correlation ID
                                    // BUT ApiVersions v3 and Produce v9+ are special - the response headers have NO tagged fields!
                                    // The tagged fields are only in the response BODY for these APIs
                                    if response.is_flexible {
                                        // ApiVersions v3 and Produce v9+ don't have header tagged fields
                                        // All other flexible APIs do
                                        if response.api_key != chronik_protocol::parser::ApiKey::ApiVersions && 
                                           response.api_key != chronik_protocol::parser::ApiKey::Produce {
                                            // Add empty tagged fields (varint 0) for other flexible APIs
                                            header_bytes.push(0);
                                        }
                                    }
                                    
                                    let mut full_response = Vec::with_capacity(header_bytes.len() + response.body.len() + 4);
                                    
                                    // Add size (4 bytes)
                                    let size = (header_bytes.len() + response.body.len()) as i32;
                                    eprintln!("DEBUG SIZE: header_bytes.len()={}, body.len()={}, calculated size={}, is_flexible={}, api_key={:?}", 
                                        header_bytes.len(), response.body.len(), size, response.is_flexible, response.api_key);
                                    eprintln!("DEBUG: response.body first 32 bytes: {:?}", 
                                        &response.body[..response.body.len().min(32)]);
                                    
                                    let size_bytes = size.to_be_bytes();
                                    eprintln!("DEBUG: size_bytes = {:?}", size_bytes);
                                    full_response.extend_from_slice(&size_bytes);
                                    eprintln!("DEBUG: After adding size, full_response.len()={}", full_response.len());
                                    
                                    // Add header (correlation ID + optional tagged fields)
                                    eprintln!("DEBUG: header_bytes = {:?}", header_bytes);
                                    full_response.extend_from_slice(&header_bytes);
                                    eprintln!("DEBUG: After adding header, full_response.len()={}", full_response.len());
                                    
                                    // Add response body
                                    full_response.extend_from_slice(&response.body);
                                    eprintln!("DEBUG: After adding body, full_response.len()={} (expected={})", 
                                        full_response.len(), 4 + header_bytes.len() + response.body.len());
                                    
                                    // Validate the complete response before sending
                                    eprintln!("CRITICAL: About to send {} bytes. First 16 bytes: {:02x?}", 
                                        full_response.len(), &full_response[..std::cmp::min(16, full_response.len())]);
                                    eprintln!("CRITICAL: Last 16 bytes: {:02x?}", 
                                        &full_response[full_response.len().saturating_sub(16)..]);
                                    
                                    debug!("Sending response: size={}, correlation_id={}, body_len={}, total_len={}", 
                                        size, response.header.correlation_id, response.body.len(), full_response.len());
                                    tracing::trace!("Response bytes (first 64): {:?}", 
                                        &full_response[..std::cmp::min(64, full_response.len())]);
                                    
                                    // Send the response
                                    let response_size = full_response.len();
                                    
                                    // Option 1: Try vectored write first (most efficient)
                                    let size_slice = &full_response[..4];
                                    let header_and_body = &full_response[4..];
                                    
                                    eprintln!("CRITICAL: Attempting vectored write with 2 IoSlices");
                                    eprintln!("  - Size prefix: {:02x?}", size_slice);
                                    eprintln!("  - Header+Body first 16 bytes: {:02x?}", &header_and_body[..std::cmp::min(16, header_and_body.len())]);
                                    
                                    let bufs = &[
                                        IoSlice::new(size_slice),
                                        IoSlice::new(header_and_body)
                                    ];
                                    
                                    // Use write_vectored to send both parts atomically
                                    let mut total_written = 0;
                                    while total_written < response_size {
                                        match socket.write_vectored(bufs).await {
                                            Ok(n) => {
                                                total_written += n;
                                                eprintln!("CRITICAL: write_vectored wrote {} bytes (total: {}/{})", 
                                                    n, total_written, response_size);
                                                
                                                if n == 0 {
                                                    eprintln!("ERROR: write_vectored returned 0 bytes");
                                                    break;
                                                }
                                                
                                                // If partial write, fall back to regular write for remainder
                                                if total_written < response_size {
                                                    eprintln!("CRITICAL: Partial write detected, writing remaining {} bytes", 
                                                        response_size - total_written);
                                                    match socket.write_all(&full_response[total_written..]).await {
                                                        Ok(()) => {
                                                            eprintln!("CRITICAL: Remainder write_all completed");
                                                            total_written = response_size;
                                                        }
                                                        Err(e) => {
                                                            error!("Error writing remainder to {}: {}", addr, e);
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!("Error in write_vectored to {}: {}", addr, e);
                                                eprintln!("CRITICAL: write_vectored FAILED: {}, falling back to write_all", e);
                                                
                                                // Fallback: Try single write_all
                                                match socket.write_all(&full_response[total_written..]).await {
                                                    Ok(()) => {
                                                        eprintln!("CRITICAL: Fallback write_all completed successfully");
                                                        total_written = response_size;
                                                    }
                                                    Err(e) => {
                                                        error!("Error in fallback write to {}: {}", addr, e);
                                                        break;
                                                    }
                                                }
                                                break;
                                            }
                                        }
                                    }
                                    
                                    // Ensure all data is flushed
                                    eprintln!("CRITICAL: Flushing socket to ensure transmission");
                                    match socket.flush().await {
                                        Ok(()) => {
                                            eprintln!("CRITICAL: Socket flush completed successfully");
                                        }
                                        Err(e) => {
                                            error!("Error flushing socket to {}: {}", addr, e);
                                            eprintln!("CRITICAL: Socket flush FAILED: {}", e);
                                        }
                                    }
                                    
                                    if total_written == response_size {
                                        eprintln!("CRITICAL: Successfully sent complete {} byte response", response_size);
                                    } else {
                                        eprintln!("ERROR: Only sent {}/{} bytes!", total_written, response_size);
                                    }
                                    
                                    debug!("Response sent successfully to {}", addr);
                                }
                                Err(e) => {
                                    // Convert to ServerError for proper handling
                                    let server_error = ErrorHandler::from_anyhow(anyhow::anyhow!(e));
                                    let recovery = error_handler.handle_error(
                                        server_error, 
                                        &format!("request from {}", addr)
                                    ).await;
                                    
                                    match recovery {
                                        ErrorRecovery::ReturnError(error_code) => {
                                            // Build proper error response with preserved correlation ID
                                            let error_response = error_handler.build_error_response(
                                                error_code,
                                                correlation_id, // Use the preserved correlation ID
                                                0, // Unknown API key
                                                0, // Unknown API version
                                            );
                                            
                                            if let Err(e) = socket.write_all(&error_response).await {
                                                error!("Error writing error response to {}: {}", addr, e);
                                                break;
                                            }
                                        }
                                        ErrorRecovery::CloseConnection => {
                                            info!("Closing connection to {} due to error", addr);
                                            break;
                                        }
                                        ErrorRecovery::Throttle(ms) => {
                                            debug!("Throttling client {} for {}ms", addr, ms);
                                            tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                                        }
                                        _ => {}
                                    }
                                }
                            }
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