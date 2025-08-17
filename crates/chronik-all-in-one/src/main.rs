use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

mod embedded_storage;
mod server;

use embedded_storage::EmbeddedMetadataStore;
use server::AllInOneServer;

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

    /// Port for metrics (default: 9090)
    #[arg(short = 'm', long, env = "CHRONIK_METRICS_PORT", default_value = "9090")]
    metrics_port: u16,

    /// Data directory for storage
    #[arg(short = 'd', long, env = "CHRONIK_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Enable search indexing
    #[arg(long, env = "CHRONIK_ENABLE_SEARCH", default_value = "true")]
    enable_search: bool,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, env = "RUST_LOG", default_value = "info")]
    log_level: String,

    /// Bind address (default: 0.0.0.0)
    #[arg(short = 'b', long, env = "CHRONIK_BIND_ADDR", default_value = "0.0.0.0")]
    bind_addr: String,

    /// Advertised address for clients
    #[arg(long, env = "CHRONIK_ADVERTISED_ADDR")]
    advertised_addr: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the all-in-one server (default)
    Start,
    
    /// Run in standalone mode (no external dependencies)
    Standalone {
        /// Memory limit in MB
        #[arg(long, default_value = "1024")]
        memory_limit: usize,
    },
    
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

    info!("Starting Chronik Stream All-in-One Server");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));

    // Handle commands
    match cli.command.as_ref().unwrap_or(&Commands::Start) {
        Commands::Start => run_server(cli).await,
        Commands::Standalone { memory_limit } => run_standalone(cli, *memory_limit).await,
        Commands::Check => check_config(cli),
    }
}

async fn run_server(cli: Cli) -> Result<()> {
    info!("Starting in standard mode");
    
    // Create data directory if it doesn't exist
    std::fs::create_dir_all(&cli.data_dir)?;
    
    // Initialize embedded storage
    let metadata_store = Arc::new(RwLock::new(
        EmbeddedMetadataStore::new(&cli.data_dir)?
    ));
    
    // Create the all-in-one server
    let server = AllInOneServer::new(
        &cli.bind_addr,
        cli.kafka_port,
        cli.admin_port,
        cli.metrics_port,
        metadata_store,
        cli.data_dir.clone(),
        cli.enable_search,
    )?;
    
    // Set advertised address
    if let Some(addr) = cli.advertised_addr {
        server.set_advertised_address(&addr);
    } else {
        let advertised = format!("{}:{}", cli.bind_addr, cli.kafka_port);
        server.set_advertised_address(&advertised);
    }
    
    info!("Server configuration:");
    info!("  Kafka API: {}:{}", cli.bind_addr, cli.kafka_port);
    info!("  Admin API: {}:{}", cli.bind_addr, cli.admin_port);
    info!("  Metrics:   {}:{}", cli.bind_addr, cli.metrics_port);
    info!("  Data dir:  {:?}", cli.data_dir);
    info!("  Search:    {}", if cli.enable_search { "enabled" } else { "disabled" });
    
    // Start the server
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            error!("Server error: {}", e);
        }
    });
    
    info!("Chronik Stream is ready!");
    info!("Connect with: kafkactl --brokers {}:{}", 
        cli.advertised_addr.as_ref().unwrap_or(&cli.bind_addr.to_string()), 
        cli.kafka_port
    );
    
    // Wait for shutdown signal
    wait_for_shutdown().await;
    
    info!("Shutting down...");
    server_handle.abort();
    
    Ok(())
}

async fn run_standalone(cli: Cli, memory_limit: usize) -> Result<()> {
    info!("Starting in standalone mode (memory limit: {} MB)", memory_limit);
    
    // Use in-memory storage for standalone mode
    let metadata_store = Arc::new(RwLock::new(
        EmbeddedMetadataStore::new_in_memory()?
    ));
    
    // Create the all-in-one server with memory constraints
    let mut server = AllInOneServer::new(
        &cli.bind_addr,
        cli.kafka_port,
        cli.admin_port,
        cli.metrics_port,
        metadata_store,
        cli.data_dir.clone(),
        cli.enable_search,
    )?;
    
    server.set_memory_limit(memory_limit);
    
    info!("Standalone server ready (no persistence)");
    info!("Connect with: kafkactl --brokers localhost:{}", cli.kafka_port);
    
    // Start the server
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            error!("Server error: {}", e);
        }
    });
    
    // Wait for shutdown signal
    wait_for_shutdown().await;
    
    info!("Shutting down...");
    server_handle.abort();
    
    Ok(())
}

fn check_config(cli: Cli) -> Result<()> {
    info!("Checking configuration...");
    
    // Check if ports are available
    for (name, port) in [
        ("Kafka", cli.kafka_port),
        ("Admin", cli.admin_port),
        ("Metrics", cli.metrics_port),
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

async fn wait_for_shutdown() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received");
}