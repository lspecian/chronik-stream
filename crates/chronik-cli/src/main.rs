//! Chronik Stream CLI tool.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod client;
mod output;

use commands::*;

/// Chronik Stream command-line tool
#[derive(Parser)]
#[command(name = "chronik-ctl")]
#[command(version, about, long_about = None)]
struct Cli {
    /// Admin API endpoint
    #[arg(short, long, default_value = "http://localhost:8080")]
    admin_url: String,
    
    /// API token
    #[arg(short, long)]
    token: Option<String>,
    
    /// Output format
    #[arg(short, long, default_value = "table")]
    output: OutputFormat,
    
    #[command(subcommand)]
    command: Commands,
}

/// Available commands
#[derive(Subcommand)]
enum Commands {
    /// Cluster management
    Cluster(ClusterCommand),
    
    /// Topic management
    Topic(TopicCommand),
    
    /// Broker management
    Broker(BrokerCommand),
    
    /// Consumer group management
    Group(GroupCommand),
    
    /// Authentication
    Auth(AuthCommand),
}

/// Output format
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum OutputFormat {
    Table,
    Json,
    Yaml,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();
    
    // Override with environment variables if not set via CLI
    if cli.admin_url == "http://localhost:8080" {
        if let Ok(url) = std::env::var("CHRONIK_ADMIN_URL") {
            cli.admin_url = url;
        }
    }
    
    if cli.token.is_none() {
        if let Ok(token) = std::env::var("CHRONIK_API_TOKEN") {
            cli.token = Some(token);
        }
    }
    
    // Create HTTP client
    let client = client::AdminClient::new(&cli.admin_url, cli.token)?;
    
    // Execute command
    match cli.command {
        Commands::Cluster(cmd) => cmd.execute(&client, cli.output).await?,
        Commands::Topic(cmd) => cmd.execute(&client, cli.output).await?,
        Commands::Broker(cmd) => cmd.execute(&client, cli.output).await?,
        Commands::Group(cmd) => cmd.execute(&client, cli.output).await?,
        Commands::Auth(cmd) => cmd.execute(&client, cli.output).await?,
    }
    
    Ok(())
}