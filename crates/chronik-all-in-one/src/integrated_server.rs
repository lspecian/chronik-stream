//! Integrated Kafka server using chronik-ingest components.
//! 
//! This module properly integrates the complete, production-ready implementation
//! from chronik-ingest instead of reimplementing everything from scratch.

use anyhow::Result;
use std::sync::Arc;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, error, debug, warn};
use crate::error_handler::{ErrorHandler, ErrorCode, ErrorRecovery, ServerError};

// Use the actual chronik-ingest components
use chronik_ingest::kafka_handler::KafkaProtocolHandler;
use chronik_ingest::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use chronik_ingest::storage::{StorageConfig as IngestStorageConfig, StorageService};

// Storage components
use chronik_storage::{
    ObjectStoreTrait, ObjectStoreConfig, ObjectStoreFactory,
    SegmentReader, SegmentReaderConfig,
    SegmentWriter, SegmentWriterConfig,
};

// Metadata store
use chronik_common::metadata::{
    memory::InMemoryMetadataStore,
    file_store::FileMetadataStore,
    traits::MetadataStore,
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
    /// Enable persistent metadata (if false, use in-memory metadata store)
    pub enable_persistent_metadata: bool,
    /// Enable dual storage (raw Kafka + indexed records for search)
    pub enable_dual_storage: bool,
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
            enable_persistent_metadata: false, // False = use in-memory, true = use object storage
            enable_dual_storage: false, // Default to raw-only for better performance
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
        
        // Initialize metadata store (file-based for persistence, otherwise in-memory)
        let metadata_store: Arc<dyn MetadataStore> = if config.enable_persistent_metadata {
            info!("Using persistent file-based metadata store");
            let metadata_dir = format!("{}/metadata", config.data_dir);
            
            match FileMetadataStore::new(&metadata_dir).await {
                Ok(store) => {
                    info!("Successfully initialized file-based metadata store at {}", metadata_dir);
                    Arc::new(store)
                },
                Err(e) => {
                    error!("Failed to initialize file-based metadata: {:?}, falling back to in-memory store", e);
                    Arc::new(InMemoryMetadataStore::new())
                }
            }
        } else {
            info!("Using in-memory metadata store (persistence disabled)");
            Arc::new(InMemoryMetadataStore::new())
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
                max_segment_age_secs: 3600, // 1 hour
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
        
        // Initialize produce handler with object store directly
        let produce_handler = Arc::new(ProduceHandler::new(
            produce_config,
            object_store_arc.clone(),
            metadata_store.clone(),
        ).await?);
        
        // Start background tasks for segment rotation
        produce_handler.start_background_tasks().await;
        
        // Initialize Kafka protocol handler with all components
        let kafka_handler = Arc::new(KafkaProtocolHandler::new(
            produce_handler,
            segment_reader,
            metadata_store.clone(),
            object_store_arc.clone(),
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
                            
                            let request_size = i32::from_be_bytes(size_buf) as usize;
                            if request_size == 0 || request_size > 10_000_000 {
                                error!("Invalid request size {} from {}", request_size, addr);
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
                            
                            // Handle request using the integrated handler
                            match kafka_handler.handle_request(&request_buffer[..request_size]).await {
                                Ok(response) => {
                                    // Build complete response with size header
                                    let mut full_response = Vec::with_capacity(response.body.len() + 8);
                                    
                                    // Add size (4 bytes) - includes correlation ID
                                    let size = (response.body.len() + 4) as i32;
                                    full_response.extend_from_slice(&size.to_be_bytes());
                                    
                                    // Add correlation ID (4 bytes)
                                    full_response.extend_from_slice(&response.header.correlation_id.to_be_bytes());
                                    
                                    // Add response body
                                    full_response.extend_from_slice(&response.body);
                                    
                                    debug!("Sending response: size={}, correlation_id={}, body_len={}, total_len={}", 
                                        size, response.header.correlation_id, response.body.len(), full_response.len());
                                    tracing::trace!("Response bytes (first 64): {:?}", 
                                        &full_response[..std::cmp::min(64, full_response.len())]);
                                    
                                    // Send response
                                    if let Err(e) = socket.write_all(&full_response).await {
                                        error!("Error writing response to {}: {}", addr, e);
                                        break;
                                    }
                                    
                                    // Flush to ensure data is sent
                                    if let Err(e) = socket.flush().await {
                                        error!("Error flushing socket to {}: {}", addr, e);
                                        break;
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
                                            // Build proper error response
                                            let error_response = error_handler.build_error_response(
                                                error_code,
                                                0, // We don't have correlation ID here
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
        
        assert_eq!(stats.node_id, 0);
        assert_eq!(stats.brokers_count, 1);
    }
}