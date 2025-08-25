//! Improved Kafka server using chronik-ingest components

use anyhow::{Result, anyhow};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, debug, warn, error};
use bytes::BytesMut;

use chronik_ingest::{
    produce_handler::{ProduceHandler, ProduceHandlerConfig},
    fetch_handler::FetchHandler,
    consumer_group::GroupManager,
};
use chronik_storage::{
    SegmentReader, SegmentReaderConfig,
    ObjectStoreConfig, SegmentWriterConfig,
};
use chronik_protocol::{
    ApiVersionsRequest, ApiVersionsResponse,
    MetadataRequest, MetadataResponse,
    ProduceRequest, ProduceResponse,
    FetchRequest, FetchResponse,
    CreateTopicsRequest, CreateTopicsResponse,
    ListOffsetsRequest, ListOffsetsResponse,
    InitProducerIdRequest, InitProducerIdResponse,
};
use chronik_common::metadata::traits::MetadataStore;

use crate::storage::EmbeddedStorage;

pub struct ImprovedKafkaServer {
    storage: Arc<RwLock<EmbeddedStorage>>,
    produce_handler: Arc<ProduceHandler>,
    fetch_handler: Arc<FetchHandler>,
    group_manager: Arc<GroupManager>,
    node_id: i32,
    advertised_host: String,
    advertised_port: i32,
}

impl ImprovedKafkaServer {
    pub async fn new(
        storage: Arc<RwLock<EmbeddedStorage>>,
        metadata_store: Arc<dyn MetadataStore>,
        segments_dir: std::path::PathBuf,
        node_id: i32,
        advertised_host: String,
        advertised_port: i32,
    ) -> Result<Self> {
        // Configure storage
        let storage_config = chronik_ingest::storage::StorageConfig {
            object_store_config: ObjectStoreConfig::LocalFileSystem {
                root_path: segments_dir.clone(),
            },
            segment_writer_config: SegmentWriterConfig {
                data_dir: segments_dir.clone(),
                compression_codec: "gzip".to_string(),
                max_segment_size: 128 * 1024 * 1024,
            },
            segment_reader_config: SegmentReaderConfig::default(),
        };
        
        let storage_service = chronik_ingest::storage::StorageService::new(storage_config.clone()).await?;
        
        // Create produce handler
        let produce_config = ProduceHandlerConfig {
            node_id,
            storage_config: storage_config.clone(),
            indexer_config: Default::default(),
            enable_indexing: false, // Disable search for now
            enable_idempotence: true,
            enable_transactions: false,
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: 10,
            compression_type: chronik_storage::kafka_records::CompressionType::None,
            request_timeout_ms: 30000,
            buffer_memory: 32 * 1024 * 1024,
            auto_create_topics_enable: true,
            num_partitions: 1,
            default_replication_factor: 1,
        };
        
        let produce_handler = Arc::new(
            ProduceHandler::new(produce_config, storage_service.object_store(), metadata_store.clone()).await?
        );
        
        // Create fetch handler
        let segment_reader = Arc::new(SegmentReader::new(
            storage_config.segment_reader_config,
            storage_service.object_store(),
        ));
        
        let fetch_handler = Arc::new(FetchHandler::new(
            segment_reader,
            metadata_store.clone(),
        ));
        
        // Create group manager
        let group_manager = Arc::new(GroupManager::new(metadata_store.clone()));
        group_manager.clone().start_expiration_checker();
        
        Ok(Self {
            storage,
            produce_handler,
            fetch_handler,
            group_manager,
            node_id,
            advertised_host,
            advertised_port,
        })
    }
    
    pub async fn run(&self, addr: &str) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!("Improved Kafka server listening on {}", addr);
        
        loop {
            let (stream, peer_addr) = listener.accept().await?;
            info!("New connection from {}", peer_addr);
            
            let storage = self.storage.clone();
            let produce_handler = self.produce_handler.clone();
            let fetch_handler = self.fetch_handler.clone();
            let group_manager = self.group_manager.clone();
            let node_id = self.node_id;
            let advertised_host = self.advertised_host.clone();
            let advertised_port = self.advertised_port;
            
            tokio::spawn(async move {
                if let Err(e) = handle_connection(
                    stream,
                    storage,
                    produce_handler,
                    fetch_handler,
                    group_manager,
                    node_id,
                    advertised_host,
                    advertised_port,
                ).await {
                    error!("Connection error: {}", e);
                }
            });
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    storage: Arc<RwLock<EmbeddedStorage>>,
    produce_handler: Arc<ProduceHandler>,
    fetch_handler: Arc<FetchHandler>,
    group_manager: Arc<GroupManager>,
    node_id: i32,
    advertised_host: String,
    advertised_port: i32,
) -> Result<()> {
    let mut buffer = BytesMut::with_capacity(1024 * 64);
    
    loop {
        // Read request size
        let mut size_buf = [0u8; 4];
        match stream.read_exact(&mut size_buf).await {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("Client disconnected");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        
        let size = i32::from_be_bytes(size_buf) as usize;
        if size == 0 || size > 100 * 1024 * 1024 {
            warn!("Invalid request size: {}", size);
            return Err(anyhow!("Invalid request size"));
        }
        
        // Read request
        buffer.resize(size, 0);
        stream.read_exact(&mut buffer).await?;
        
        // Parse request header
        let header = parse_request_header(&buffer)?;
        info!("Request: api_key={:?}, version={}, correlation_id={}, client_id={:?}", 
               header.api_key, header.api_version, header.correlation_id, header.client_id);
        
        // Process request based on API key
        let body_start = header.header_size;
        let response = match header.api_key {
            ApiKey::ApiVersions => handle_api_versions(&header),
            ApiKey::Metadata => handle_metadata(&header, &storage, node_id, &advertised_host, advertised_port).await,
            ApiKey::CreateTopics => handle_create_topics(&header, &buffer[body_start..], storage.clone()).await,
            ApiKey::InitProducerId => handle_init_producer_id(&header),
            ApiKey::Produce => {
                // Use the real produce handler
                let request = ProduceRequest::decode(&buffer[body_start..], header.api_version)?;
                let response = produce_handler.handle_produce(request, header.correlation_id).await?;
                response.encode(header.api_version)?
            },
            ApiKey::Fetch => {
                // Use the real fetch handler
                let request = FetchRequest::decode(&buffer[body_start..], header.api_version)?;
                let response = fetch_handler.handle_fetch(request, header.correlation_id).await?;
                response.encode(header.api_version)?
            },
            ApiKey::ListOffsets => handle_list_offsets(&header, &buffer[body_start..], storage.clone()).await,
            _ => {
                warn!("Unsupported API: {:?}", header.api_key);
                create_error_response(&header, 35) // UNSUPPORTED_VERSION
            }
        };
        
        // Send response
        stream.write_all(&response).await?;
    }
}

// Import the existing handler functions from kafka_server.rs
use crate::kafka_server::{
    ApiKey, RequestHeader,
    parse_request_header,
    handle_api_versions,
    handle_metadata,
    handle_create_topics,
    handle_init_producer_id,
    handle_list_offsets,
    create_error_response,
};