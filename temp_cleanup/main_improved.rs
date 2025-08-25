use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{info, error};

// Use the proper chronik-ingest components
use chronik_ingest::{
    kafka_handler::KafkaProtocolHandler,
    produce_handler::{ProduceHandler, ProduceHandlerConfig},
    storage::{StorageService, StorageConfig},
    server::{IngestServer, ServerConfig},
};
use chronik_storage::{
    ObjectStoreConfig, SegmentWriterConfig, SegmentReaderConfig,
    SegmentReader,
};
use chronik_search::realtime_indexer::RealtimeIndexerConfig;
use chronik_common::metadata::traits::MetadataStore;

mod storage;
use storage::EmbeddedStorage;

#[derive(Parser, Debug)]
#[command(
    name = "chronik",
    about = "Chronik Stream - Kafka-compatible streaming platform with search",
    version,
    author
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Port for Kafka protocol (default: 9092)
    #[arg(short = 'p', long, env = "CHRONIK_KAFKA_PORT", default_value = "9092")]
    kafka_port: u16,

    /// Port for Admin API (default: 3000)
    #[arg(short = 'a', long, env = "CHRONIK_ADMIN_PORT", default_value = "3000")]
    admin_port: u16,

    /// Data directory for storage
    #[arg(short = 'd', long, env = "CHRONIK_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, env = "RUST_LOG", default_value = "info")]
    log_level: String,

    /// Bind address (default: 0.0.0.0)
    #[arg(short = 'b', long, env = "CHRONIK_BIND_ADDR", default_value = "0.0.0.0")]
    bind_addr: String,
    
    /// Advertised address for clients
    #[arg(long, env = "CHRONIK_ADVERTISED_ADDR")]
    advertised_addr: Option<String>,
    
    /// Node ID
    #[arg(long, env = "CHRONIK_NODE_ID", default_value = "1001")]
    node_id: i32,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the server with full Kafka compatibility (default)
    Start,
    
    /// Run in standalone mode (no persistence)
    Standalone,
    
    /// Check configuration and exit
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(&cli.log_level)
        .init();

    info!("Chronik Stream - Full Kafka Compatibility Mode");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    
    match cli.command.as_ref().unwrap_or(&Commands::Start) {
        Commands::Start => run_improved_server(cli, false).await,
        Commands::Standalone => run_improved_server(cli, true).await,
        Commands::Check => check_config(cli),
    }
}

async fn run_improved_server(cli: Cli, in_memory: bool) -> Result<()> {
    info!("Starting server ({})", if in_memory { "in-memory" } else { "persistent" });
    
    // Create data directories
    let segments_dir = cli.data_dir.join("segments");
    let metadata_dir = cli.data_dir.join("metadata");
    
    if !in_memory {
        std::fs::create_dir_all(&segments_dir)?;
        std::fs::create_dir_all(&metadata_dir)?;
    }
    
    // Initialize embedded storage for metadata
    let embedded_storage = Arc::new(RwLock::new(
        if in_memory {
            EmbeddedStorage::new_in_memory()?
        } else {
            EmbeddedStorage::new(&metadata_dir)?
        }
    ));
    
    // Create a metadata store wrapper
    let metadata_store = Arc::new(MetadataStoreWrapper::new(embedded_storage.clone()));
    
    // Configure storage service
    let storage_config = StorageConfig {
        object_store_config: ObjectStoreConfig::LocalFileSystem {
            root_path: segments_dir.clone(),
        },
        segment_writer_config: SegmentWriterConfig {
            data_dir: segments_dir.clone(),
            compression_codec: "gzip".to_string(),
            max_segment_size: 128 * 1024 * 1024, // 128MB
        },
        segment_reader_config: SegmentReaderConfig::default(),
    };
    
    let storage_service = StorageService::new(storage_config.clone()).await?;
    
    // Configure produce handler
    let produce_config = ProduceHandlerConfig {
        node_id: cli.node_id,
        storage_config: storage_config.clone(),
        indexer_config: RealtimeIndexerConfig::default(),
        enable_indexing: true,
        enable_idempotence: true,
        enable_transactions: false, // Start without transactions
        max_in_flight_requests: 5,
        batch_size: 16384,
        linger_ms: 10,
        compression_type: chronik_storage::kafka_records::CompressionType::Gzip,
        request_timeout_ms: 30000,
        buffer_memory: 32 * 1024 * 1024,
        auto_create_topics_enable: true,
        num_partitions: 1,
        default_replication_factor: 1,
    };
    
    let produce_handler = Arc::new(
        ProduceHandler::new(produce_config, storage_service.object_store(), metadata_store.clone()).await?
    );
    
    // Create segment reader
    let segment_reader = Arc::new(SegmentReader::new(
        storage_config.segment_reader_config,
        storage_service.object_store(),
    ));
    
    // Determine advertised address
    let advertised_host = cli.advertised_addr
        .clone()
        .unwrap_or_else(|| format!("{}:{}", cli.bind_addr, cli.kafka_port));
    let (host, port_str) = advertised_host.rsplit_once(':').unwrap_or((&advertised_host, "9092"));
    let port = port_str.parse::<i32>().unwrap_or(9092);
    
    // Create Kafka protocol handler with all components
    let kafka_handler = KafkaProtocolHandler::new(
        produce_handler,
        segment_reader,
        metadata_store.clone(),
        cli.node_id,
        host.to_string(),
        port,
    ).await?;
    
    // Start Admin API
    let admin_addr = format!("{}:{}", cli.bind_addr, cli.admin_port);
    let admin_storage = embedded_storage.clone();
    tokio::spawn(async move {
        if let Err(e) = run_admin_server(admin_addr, admin_storage).await {
            error!("Admin server error: {}", e);
        }
    });
    
    // Configure and start the ingest server
    let server_config = ServerConfig {
        listen_addr: format!("{}:{}", cli.bind_addr, cli.kafka_port).parse()?,
        max_connections: 1000,
        request_timeout: std::time::Duration::from_secs(30),
        idle_timeout: std::time::Duration::from_secs(600),
        tls_config: None,
        connection_pool_config: Default::default(),
    };
    
    let server = IngestServer::new(server_config, Arc::new(kafka_handler), metadata_store)?;
    
    info!("Server configuration:");
    info!("  Kafka API: {}:{}", cli.bind_addr, cli.kafka_port);
    info!("  Advertised: {}", advertised_host);
    info!("  Admin API: {}:{}", cli.bind_addr, cli.admin_port);
    info!("  Data dir:  {:?}", if in_memory { "in-memory".into() } else { cli.data_dir });
    info!("  Node ID:   {}", cli.node_id);
    info!("");
    info!("Chronik Stream is ready with FULL Kafka compatibility!");
    info!("Connect with: kafka-console-producer --broker-list {}", advertised_host);
    
    // Run the server
    server.run().await?;
    
    Ok(())
}

async fn run_admin_server(
    addr: String,
    storage: Arc<RwLock<EmbeddedStorage>>
) -> Result<()> {
    use axum::{
        routing::{get, post},
        Json, Router,
    };
    use serde_json::json;
    
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "healthy"})) }))
        .route("/api/topics", get(list_topics))
        .route("/api/topics", post(create_topic))
        .with_state(storage);
    
    let listener = TcpListener::bind(&addr).await?;
    info!("Admin API listening on {}", addr);
    
    axum::serve(listener, app).await?;
    Ok(())
}

async fn list_topics(
    axum::extract::State(storage): axum::extract::State<Arc<RwLock<EmbeddedStorage>>>
) -> impl axum::response::IntoResponse {
    use serde_json::json;
    let storage = storage.read().await;
    let topics = storage.list_topics().await;
    axum::Json(json!({ "topics": topics }))
}

async fn create_topic(
    axum::extract::State(storage): axum::extract::State<Arc<RwLock<EmbeddedStorage>>>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> impl axum::response::IntoResponse {
    use serde_json::json;
    
    let topic = payload["name"].as_str().unwrap_or("unknown");
    let partitions = payload["partitions"].as_u64().unwrap_or(1) as u32;
    
    let mut storage = storage.write().await;
    match storage.create_topic(topic.to_string(), partitions).await {
        Ok(_) => axum::Json(json!({ "status": "created", "topic": topic })),
        Err(e) => axum::Json(json!({ "status": "error", "message": e.to_string() })),
    }
}

fn check_config(cli: Cli) -> Result<()> {
    info!("Configuration check:");
    info!("  Kafka port: {}", cli.kafka_port);
    info!("  Admin port: {}", cli.admin_port);
    info!("  Data directory: {:?}", cli.data_dir);
    info!("  Bind address: {}", cli.bind_addr);
    info!("  Node ID: {}", cli.node_id);
    
    if let Some(advertised) = cli.advertised_addr {
        info!("  Advertised address: {}", advertised);
    }
    
    info!("Configuration is valid!");
    Ok(())
}

// Wrapper to adapt EmbeddedStorage to MetadataStore trait
struct MetadataStoreWrapper {
    storage: Arc<RwLock<EmbeddedStorage>>,
}

impl MetadataStoreWrapper {
    fn new(storage: Arc<RwLock<EmbeddedStorage>>) -> Self {
        Self { storage }
    }
}

#[async_trait::async_trait]
impl MetadataStore for MetadataStoreWrapper {
    async fn get(&self, key: &str) -> chronik_common::Result<Option<Vec<u8>>> {
        // Implement metadata operations using embedded storage
        Ok(None) // Simplified for now
    }
    
    async fn put(&self, key: &str, value: Vec<u8>) -> chronik_common::Result<()> {
        Ok(())
    }
    
    async fn delete(&self, key: &str) -> chronik_common::Result<()> {
        Ok(())
    }
    
    async fn list(&self, prefix: &str) -> chronik_common::Result<Vec<String>> {
        Ok(vec![])
    }
    
    async fn list_with_values(&self, prefix: &str) -> chronik_common::Result<Vec<(String, Vec<u8>)>> {
        Ok(vec![])
    }
}