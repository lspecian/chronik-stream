//! Unified Chronik Stream Server
//! 
//! This is the main entry point for Chronik Stream, supporting multiple operational modes:
//! - Standalone (default): Single-node Kafka-compatible server
//! - Ingest: Data ingestion node (future: for distributed mode)
//! - Search: Search node (future: for distributed mode)
//! - All: All components in one process

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::{error, info, warn};
use chronik_monitoring::{init_monitoring, TracingConfig};

mod integrated_server;
mod error_handler;
mod kafka_handler;
mod produce_handler;
mod storage;
mod consumer_group;
mod fetch_handler;
mod handler;
mod offset_storage;
mod coordinator_manager;

use integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};

#[derive(Parser, Debug)]
#[command(
    name = "chronik-server",
    about = "Chronik Stream - Unified Kafka-compatible streaming platform",
    version,
    author,
    long_about = "A high-performance, Kafka-compatible streaming platform with optional search capabilities"
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

    /// Port for Metrics endpoint (default: 9093)
    #[arg(short = 'm', long, env = "CHRONIK_METRICS_PORT", default_value = "9093")]
    metrics_port: u16,

    /// Data directory for storage
    #[arg(short = 'd', long, env = "CHRONIK_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, env = "RUST_LOG", default_value = "info")]
    log_level: String,

    /// Bind address (default: 0.0.0.0)
    #[arg(short = 'b', long, env = "CHRONIK_BIND_ADDR", default_value = "0.0.0.0")]
    bind_addr: String,
    
    /// Advertised address for clients to connect (defaults to bind address)
    #[arg(long, env = "CHRONIK_ADVERTISED_ADDR")]
    advertised_addr: Option<String>,
    
    /// Advertised port for clients to connect (defaults to kafka port)
    #[arg(long, env = "CHRONIK_ADVERTISED_PORT")]
    advertised_port: Option<u16>,

    /// Enable search functionality
    #[cfg(feature = "search")]
    #[arg(long, env = "CHRONIK_ENABLE_SEARCH", default_value = "false")]
    enable_search: bool,

    /// Enable backup functionality
    #[cfg(feature = "backup")]
    #[arg(long, env = "CHRONIK_ENABLE_BACKUP", default_value = "false")]
    enable_backup: bool,

    /// Enable dynamic configuration
    #[cfg(feature = "dynamic-config")]
    #[arg(long, env = "CHRONIK_ENABLE_DYNAMIC_CONFIG", default_value = "false")]
    enable_dynamic_config: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run in standalone mode (default)
    #[command(about = "Run as a standalone Kafka-compatible server")]
    Standalone {
        /// Enable dual storage (raw Kafka + indexed for search)
        #[arg(long, default_value = "false")]
        dual_storage: bool,
    },
    
    /// Run as ingest node (future: for distributed mode)
    #[command(about = "Run as an ingest node in a distributed cluster")]
    Ingest {
        /// Controller URL for cluster coordination
        #[arg(long, env = "CHRONIK_CONTROLLER_URL")]
        controller_url: Option<String>,
    },
    
    /// Run as search node (future: for distributed mode)
    #[cfg(feature = "search")]
    #[command(about = "Run as a search node in a distributed cluster")]
    Search {
        /// Storage backend URL
        #[arg(long, env = "CHRONIK_STORAGE_URL")]
        storage_url: String,
    },
    
    /// Run with all components enabled
    #[command(about = "Run with all components in a single process")]
    All {
        /// Enable experimental features
        #[arg(long, default_value = "false")]
        experimental: bool,
    },

    /// Show version and build information
    #[command(about = "Display version and build information")]
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    
    // Initialize logging
    std::env::set_var("RUST_LOG", &cli.log_level);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Show deprecation warnings for old binaries
    if std::env::args().next().unwrap().contains("chronik-ingest") {
        warn!("chronik-ingest is deprecated. Please use chronik-server instead.");
    }
    if std::env::args().next().unwrap().contains("chronik-all-in-one") {
        warn!("chronik-all-in-one is deprecated. Please use chronik-server instead.");
    }

    match cli.command {
        Some(Commands::Version) => {
            println!("Chronik Server v{}", env!("CARGO_PKG_VERSION"));
            println!("Build features:");
            #[cfg(feature = "search")]
            println!("  - Search: enabled");
            #[cfg(feature = "backup")]
            println!("  - Backup: enabled");
            #[cfg(feature = "dynamic-config")]
            println!("  - Dynamic Config: enabled");
            return Ok(());
        }
        
        Some(Commands::Standalone { dual_storage }) => {
            info!("Starting Chronik Server in standalone mode");
            info!("Dual storage: {}", dual_storage);
            run_standalone_server(&cli, dual_storage).await?;
        }
        
        Some(Commands::Ingest { ref controller_url }) => {
            info!("Starting Chronik Server as ingest node");
            if controller_url.is_some() {
                warn!("Distributed mode not yet implemented - running in standalone mode");
            }
            run_ingest_server(&cli).await?;
        }
        
        #[cfg(feature = "search")]
        Some(Commands::Search { ref storage_url }) => {
            info!("Starting Chronik Server as search node");
            info!("Storage URL: {}", storage_url);
            warn!("Search-only mode not yet implemented - running in standalone mode");
            run_standalone_server(&cli, true).await?;
        }
        
        Some(Commands::All { experimental }) => {
            info!("Starting Chronik Server with all components");
            if experimental {
                warn!("Experimental features enabled");
            }
            run_all_components(&cli).await?;
        }
        
        None => {
            // Default to standalone mode
            info!("Starting Chronik Server in standalone mode (default)");
            run_standalone_server(&cli, false).await?;
        }
    }

    Ok(())
}

async fn run_standalone_server(cli: &Cli, dual_storage: bool) -> Result<()> {
    // Determine advertised address - use explicit config or fall back to bind address
    let advertised_host = cli.advertised_addr.as_ref()
        .unwrap_or(&cli.bind_addr)
        .clone();
    let advertised_port = cli.advertised_port
        .unwrap_or(cli.kafka_port) as i32;
    
    // Warn if using 0.0.0.0 as advertised address
    if advertised_host == "0.0.0.0" {
        warn!("Advertised address is 0.0.0.0 - clients may not be able to connect.");
        warn!("Consider setting --advertised-addr or CHRONIK_ADVERTISED_ADDR to a resolvable hostname/IP.");
    }
    
    // Create server configuration
    let config = IntegratedServerConfig {
        node_id: 1,
        advertised_host,
        advertised_port,
        data_dir: cli.data_dir.to_string_lossy().to_string(),
        enable_indexing: false,
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor: 1,
        enable_dual_storage: dual_storage,
    };

    // Create and start the integrated server
    let server = IntegratedKafkaServer::new(config).await?;
    
    // Initialize monitoring (metrics + optional tracing)
    let _metrics_registry = init_monitoring(
        "chronik-server",
        cli.metrics_port,
        None, // TODO: Add tracing config support
    ).await?;
    
    let kafka_addr = format!("{}:{}", cli.bind_addr, cli.kafka_port);
    info!("Kafka protocol listening on {}", kafka_addr);
    info!("Metrics endpoint available at http://{}:{}/metrics", cli.bind_addr, cli.metrics_port);
    
    // Start search API if enabled
    #[cfg(feature = "search")]
    if cli.enable_search {
        info!("Starting Search API on port 8080");
        
        let search_bind = cli.bind_addr.clone();
        tokio::spawn(async move {
            use chronik_search::api::SearchApi;
            use std::sync::Arc;
            
            // Create search API instance
            let search_api = Arc::new(SearchApi::new().unwrap());
            
            // Create router
            let app = search_api.router();
            
            // Start server
            let addr = format!("{}:8080", search_bind);
            info!("Search API listening on http://{}", addr);
            
            let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
            chronik_search::serve_app(listener, app).await.unwrap()
        });
        
        info!("Search API available at http://{}:8080", cli.bind_addr);
        info!("  - Search: POST http://{}:8080/_search", cli.bind_addr);
        info!("  - Index: PUT http://{}:8080/{{index}}", cli.bind_addr);
        info!("  - Document: POST http://{}:8080/{{index}}/_doc/{{id}}", cli.bind_addr);
    }
    
    server.run(&kafka_addr).await?;
    
    Ok(())
}

async fn run_ingest_server(cli: &Cli) -> Result<()> {
    // For now, just run standalone
    // In the future, this would connect to a controller for coordination
    run_standalone_server(cli, false).await
}

async fn run_all_components(cli: &Cli) -> Result<()> {
    // Run with all features enabled
    let mut tasks = vec![];
    
    // Initialize monitoring (metrics + optional tracing)
    let _metrics_registry = init_monitoring(
        "chronik-server-all",
        cli.metrics_port,
        None, // TODO: Add tracing config support
    ).await?;
    info!("Metrics endpoint available at http://{}:{}/metrics", cli.bind_addr, cli.metrics_port);
    
    // Determine advertised address - use explicit config or fall back to bind address
    let advertised_host = cli.advertised_addr.as_ref()
        .unwrap_or(&cli.bind_addr)
        .clone();
    let advertised_port = cli.advertised_port
        .unwrap_or(cli.kafka_port) as i32;
    
    // Warn if using 0.0.0.0 as advertised address
    if advertised_host == "0.0.0.0" {
        warn!("Advertised address is 0.0.0.0 - clients may not be able to connect.");
        warn!("Consider setting --advertised-addr or CHRONIK_ADVERTISED_ADDR to a resolvable hostname/IP.");
    }
    
    // Create server configuration with all features
    let config = IntegratedServerConfig {
        node_id: 1,
        advertised_host,
        advertised_port,
        data_dir: cli.data_dir.to_string_lossy().to_string(),
        enable_indexing: cfg!(feature = "search"),
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor: 1,
        enable_dual_storage: true,  // Enable dual storage for search
    };
    
    // Start Kafka protocol server
    let kafka_addr = format!("{}:{}", cli.bind_addr, cli.kafka_port);
    let kafka_task = tokio::spawn(async move {
        let server = IntegratedKafkaServer::new(config).await?;
        server.run(&kafka_addr).await
    });
    tasks.push(kafka_task);
    
    info!("Kafka protocol listening on {}:{}", cli.bind_addr, cli.kafka_port);
    
    #[cfg(feature = "search")]
    {
        info!("Search API enabled, starting on port 8080");
        
        // Start the search API server
        let search_bind = cli.bind_addr.clone();
        let search_port = 8080u16; // Search API port
        let search_task = tokio::spawn(async move {
            use chronik_search::api::SearchApi;
            use std::sync::Arc;
            
            // Create search API instance
            let search_api = Arc::new(SearchApi::new().map_err(|e| anyhow::anyhow!("Failed to create search API: {}", e))?);
            
            // Create router
            let app = search_api.router();
            
            // Start server
            let addr = format!("{}:{}", search_bind, search_port);
            info!("Search API listening on http://{}", addr);
            
            let listener = tokio::net::TcpListener::bind(&addr).await
                .map_err(|e| anyhow::anyhow!("Failed to bind search API port: {}", e))?;
            
            chronik_search::serve_app(listener, app).await
                .map_err(|e| anyhow::anyhow!("Search API server error: {}", e))
        });
        tasks.push(search_task);
        
        info!("Search API available at http://{}:8080", cli.bind_addr);
        info!("  - Search: POST http://{}:8080/_search", cli.bind_addr);
        info!("  - Index: PUT http://{}:8080/{{index}}", cli.bind_addr);
        info!("  - Document: POST http://{}:8080/{{index}}/_doc/{{id}}", cli.bind_addr);
    }
    
    #[cfg(feature = "backup")]
    {
        info!("Backup service enabled");
        // Future: Start backup scheduler
    }
    
    // Wait for all components
    for task in tasks {
        task.await??;
    }
    
    Ok(())
}

