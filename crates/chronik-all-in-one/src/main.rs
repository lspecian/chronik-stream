use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

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

    /// Data directory for storage
    #[arg(short = 'd', long, env = "CHRONIK_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, env = "RUST_LOG", default_value = "info")]
    log_level: String,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the all-in-one server (default)
    Start,
    
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

    info!("Chronik Stream All-in-One Server");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    
    match cli.command.as_ref().unwrap_or(&Commands::Start) {
        Commands::Start => {
            info!("Starting server on port {}", cli.kafka_port);
            info!("Data directory: {:?}", cli.data_dir);
            
            // For now, just print a message
            // In a real implementation, this would start all services
            info!("Server would start here (not yet implemented)");
            info!("This is a placeholder for the all-in-one deployment");
            
            // Wait for shutdown signal
            tokio::signal::ctrl_c().await?;
            info!("Shutting down...");
        }
        Commands::Check => {
            info!("Configuration check:");
            info!("  Kafka port: {}", cli.kafka_port);
            info!("  Data directory: {:?}", cli.data_dir);
            info!("Configuration OK!");
        }
    }
    
    Ok(())
}