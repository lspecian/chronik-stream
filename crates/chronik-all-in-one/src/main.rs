use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{info, error};

mod storage;
mod kafka_server;
mod integrated_server;
mod error_handler;

use storage::EmbeddedStorage;
use integrated_server::IntegratedKafkaServer;

#[derive(Parser, Debug)]
#[command(
    name = "chronik",
    about = "Chronik Stream - All-in-one Kafka-compatible streaming platform",
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

    /// Enable persistent metadata storage (using object storage)
    #[arg(long, env = "CHRONIK_PERSISTENT_METADATA", default_value = "false")]
    persistent_metadata: bool,

    /// Enable dual storage (raw Kafka + indexed records for search)
    /// If false, only stores raw Kafka batches for protocol compatibility
    #[arg(long, env = "CHRONIK_DUAL_STORAGE", default_value = "false")]
    dual_storage: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the all-in-one server (default)
    Start,
    
    /// Run in standalone mode (no persistence)
    Standalone,
    
    /// Check configuration and exit
    Check,
    
    /// Run with integrated chronik-ingest components (full Kafka compatibility)
    Integrated,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(&cli.log_level)
        .init();

    info!("Chronik Stream All-in-One Server");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    
    match cli.command.as_ref().unwrap_or(&Commands::Start) {
        Commands::Start => run_server(cli, false).await,
        Commands::Standalone => run_server(cli, true).await,
        Commands::Check => check_config(cli),
        Commands::Integrated => run_integrated_server(cli).await,
    }
}

async fn run_server(cli: Cli, in_memory: bool) -> Result<()> {
    info!("Starting server ({})", if in_memory { "in-memory" } else { "persistent" });
    
    // Create data directory if needed
    if !in_memory {
        std::fs::create_dir_all(&cli.data_dir)?;
    }
    
    // Start Admin API with embedded storage for now
    let admin_storage = Arc::new(RwLock::new(
        if in_memory {
            EmbeddedStorage::new_in_memory()?
        } else {
            EmbeddedStorage::new(&cli.data_dir)?
        }
    ));
    
    let admin_addr = format!("{}:{}", cli.bind_addr, cli.admin_port);
    let admin_storage_clone = admin_storage.clone();
    tokio::spawn(async move {
        if let Err(e) = run_admin_server(admin_addr, admin_storage_clone).await {
            error!("Admin server error: {}", e);
        }
    });
    
    // Start Integrated Kafka server with proper components
    let kafka_addr = format!("{}:{}", cli.bind_addr, cli.kafka_port);
    let config = integrated_server::IntegratedServerConfig {
        node_id: 0,
        advertised_host: cli.bind_addr.clone(),
        advertised_port: cli.kafka_port as i32,
        data_dir: cli.data_dir.clone().to_string_lossy().to_string(),
        enable_indexing: true,
        enable_compression: false,
        auto_create_topics: true,
        num_partitions: 1,
        enable_persistent_metadata: true,
        replication_factor: 1,
        enable_dual_storage: cli.dual_storage,
    };
    let kafka_server = IntegratedKafkaServer::new(config).await?;
    
    info!("Server configuration:");
    info!("  Kafka API: {}", kafka_addr);
    info!("  Admin API: {}:{}", cli.bind_addr, cli.admin_port);
    info!("  Data dir:  {:?}", if in_memory { "in-memory".into() } else { cli.data_dir });
    info!("");
    info!("Chronik Stream is ready!");
    info!("Connect with: kafkactl --brokers {}", kafka_addr);
    
    // Run Integrated Kafka server
    kafka_server.run(&kafka_addr).await?;
    
    Ok(())
}

async fn run_admin_server(
    addr: String,
    storage: Arc<RwLock<EmbeddedStorage>>
) -> Result<()> {
    use axum::{
        routing::{get, post},
        Json, Router,
        extract::State,
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
    
    let name = payload["name"].as_str().unwrap_or("unknown");
    let partitions = payload["partitions"].as_u64().unwrap_or(1) as i32;
    
    let mut storage = storage.write().await;
    match storage.create_topic(name.to_string(), partitions).await {
        Ok(_) => axum::Json(json!({ "status": "created", "topic": name })),
        Err(e) => axum::Json(json!({ "error": e.to_string() })),
    }
}

async fn run_integrated_server(cli: Cli) -> Result<()> {
    use integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};
    
    info!("Starting INTEGRATED server with full Kafka compatibility");
    info!("Using chronik-ingest components for production-ready functionality");
    
    if cli.persistent_metadata {
        info!("Persistent metadata storage ENABLED (using object storage)");
    } else {
        info!("Persistent metadata storage DISABLED (using in-memory)");
    }
    
    // Create configuration
    let config = IntegratedServerConfig {
        node_id: 1,  // Use 1 instead of 0 (controller_id of 0 means no controller in Kafka)
        enable_dual_storage: cli.dual_storage,
        advertised_host: if cli.bind_addr == "0.0.0.0" {
            "localhost".to_string()
        } else {
            cli.bind_addr.clone()
        },
        advertised_port: cli.kafka_port as i32,
        data_dir: cli.data_dir.to_string_lossy().to_string(),
        enable_indexing: false, // Can be enabled later
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor: 1,
        enable_persistent_metadata: cli.persistent_metadata,
    };
    
    // Create and run the integrated server
    let server = IntegratedKafkaServer::new(config).await?;
    
    // Start Admin API in background (simplified for now)
    let admin_addr = format!("{}:{}", cli.bind_addr, cli.admin_port);
    info!("Admin API would be at: {}", admin_addr);
    
    // Run the Kafka server
    let kafka_addr = format!("{}:{}", cli.bind_addr, cli.kafka_port);
    info!("");
    info!("Chronik Stream INTEGRATED server is ready!");
    info!("Connect with: kafkactl --brokers {}", kafka_addr);
    info!("This server provides FULL Kafka compatibility with:");
    info!("  ✓ Real message persistence");
    info!("  ✓ Consumer group support");
    info!("  ✓ Compression support");
    info!("  ✓ Idempotent producers");
    info!("  ✓ Auto topic creation");
    
    server.run(&kafka_addr).await?;
    
    Ok(())
}

fn check_config(cli: Cli) -> Result<()> {
    info!("Checking configuration...");
    
    // Check if ports are available
    for (name, port) in [
        ("Kafka", cli.kafka_port),
        ("Admin", cli.admin_port),
    ] {
        match std::net::TcpListener::bind(format!("{}:{}", cli.bind_addr, port)) {
            Ok(_) => info!("✓ {} port {} is available", name, port),
            Err(e) => {
                error!("✗ {} port {} is not available: {}", name, port, e);
                return Err(anyhow::anyhow!("Port check failed"));
            }
        }
    }
    
    // Check data directory
    if cli.data_dir.exists() {
        if cli.data_dir.is_dir() {
            info!("✓ Data directory exists: {:?}", cli.data_dir);
        } else {
            error!("✗ Data path exists but is not a directory: {:?}", cli.data_dir);
            return Err(anyhow::anyhow!("Invalid data directory"));
        }
    } else {
        info!("✓ Data directory will be created: {:?}", cli.data_dir);
    }
    
    info!("Configuration check passed!");
    Ok(())
}