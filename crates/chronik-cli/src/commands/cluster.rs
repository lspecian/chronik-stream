//! Cluster management commands.

use crate::{client::AdminClient, output};
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};

/// Cluster management commands
#[derive(Debug, clap::Parser)]
pub struct ClusterCommand {
    #[command(subcommand)]
    command: ClusterSubcommands,
}

#[derive(Debug, Subcommand)]
enum ClusterSubcommands {
    /// Show cluster information
    Info,
    
    /// Check cluster health
    Health,
    
    /// Show cluster metrics
    Metrics,
}

impl ClusterCommand {
    pub async fn execute(
        &self,
        client: &AdminClient,
        format: super::super::OutputFormat,
    ) -> Result<()> {
        match &self.command {
            ClusterSubcommands::Info => {
                let info: ClusterInfo = client.get("/api/v1/cluster/info").await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        println!("Cluster Information:");
                        println!("  ID:          {}", info.cluster_id);
                        println!("  Name:        {}", info.name);
                        println!("  Version:     {}", info.version);
                        println!("  Brokers:     {}", info.broker_count);
                        println!("  Topics:      {}", info.topic_count);
                        println!("  Partitions:  {}", info.partition_count);
                        println!("  Controller:  {}", info.controller_leader);
                    }
                    _ => println!("{}", output::format_output(&info, format)?),
                }
            }
            ClusterSubcommands::Health => {
                let health: HealthStatus = client.get("/api/v1/cluster/health").await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        println!("Cluster Health: {}", health.status);
                        println!("\nComponents:");
                        for component in &health.components {
                            let status = if component.status == "healthy" {
                                "✓".to_string()
                            } else {
                                "✗".to_string()
                            };
                            println!("  {} {}", status, component.name);
                            if let Some(msg) = &component.message {
                                println!("    {}", msg);
                            }
                        }
                    }
                    _ => println!("{}", output::format_output(&health, format)?),
                }
            }
            ClusterSubcommands::Metrics => {
                let metrics: ClusterMetrics = client.get("/api/v1/cluster/metrics").await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        println!("Cluster Metrics:");
                        println!("  Messages/sec:    {:.2}", metrics.messages_per_sec);
                        println!("  Bytes In/sec:    {:.2} MB", metrics.bytes_in_per_sec / 1024.0 / 1024.0);
                        println!("  Bytes Out/sec:   {:.2} MB", metrics.bytes_out_per_sec / 1024.0 / 1024.0);
                        println!("  Total Messages:  {}", metrics.total_messages);
                        println!("  Total Bytes:     {:.2} GB", metrics.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0);
                    }
                    _ => println!("{}", output::format_output(&metrics, format)?),
                }
            }
        }
        
        Ok(())
    }
}

// Response types (matching admin API)

#[derive(Debug, Serialize, Deserialize)]
struct ClusterInfo {
    cluster_id: String,
    name: String,
    version: String,
    broker_count: u32,
    topic_count: u32,
    partition_count: u32,
    controller_leader: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct HealthStatus {
    status: String,
    components: Vec<ComponentHealth>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ComponentHealth {
    name: String,
    status: String,
    message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClusterMetrics {
    messages_per_sec: f64,
    bytes_in_per_sec: f64,
    bytes_out_per_sec: f64,
    total_messages: u64,
    total_bytes: u64,
}