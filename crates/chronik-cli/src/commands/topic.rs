//! Topic management commands.

use crate::{client::AdminClient, output};
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use tabled::Tabled;

/// Topic management commands
#[derive(Debug, clap::Parser)]
pub struct TopicCommand {
    #[command(subcommand)]
    command: TopicSubcommands,
}

#[derive(Debug, Subcommand)]
enum TopicSubcommands {
    /// List all topics
    List,
    
    /// Create a new topic
    Create {
        /// Topic name
        name: String,
        
        /// Number of partitions
        #[arg(short, long, default_value = "3")]
        partitions: i32,
        
        /// Replication factor
        #[arg(short, long, default_value = "2")]
        replication_factor: i32,
    },
    
    /// Show topic details
    Get {
        /// Topic name
        name: String,
    },
    
    /// Delete a topic
    Delete {
        /// Topic name
        name: String,
        
        /// Skip confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

impl TopicCommand {
    pub async fn execute(
        &self,
        client: &AdminClient,
        format: super::super::OutputFormat,
    ) -> Result<()> {
        match &self.command {
            TopicSubcommands::List => {
                let topics: Vec<Topic> = client.get("/api/v1/topics").await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        let table_data: Vec<TopicTable> = topics
                            .into_iter()
                            .map(|t| TopicTable {
                                name: t.name,
                                partitions: t.partitions,
                                replication_factor: t.replication_factor,
                                retention_ms: format_duration(t.config.retention_ms),
                            })
                            .collect();
                        
                        output::print_table(table_data);
                    }
                    _ => println!("{}", output::format_output(&topics, format)?),
                }
            }
            TopicSubcommands::Create {
                name,
                partitions,
                replication_factor,
            } => {
                let request = CreateTopicRequest {
                    name: name.clone(),
                    partitions: *partitions,
                    replication_factor: *replication_factor,
                    config: None,
                };
                
                client.post::<_, serde_json::Value>("/api/v1/topics", &request).await?;
                output::print_success(&format!("Topic '{}' created successfully", name));
            }
            TopicSubcommands::Get { name } => {
                let topic: Topic = client.get(&format!("/api/v1/topics/{}", name)).await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        println!("Topic: {}", topic.name);
                        println!("  Partitions:         {}", topic.partitions);
                        println!("  Replication Factor: {}", topic.replication_factor);
                        println!("  Configuration:");
                        println!("    Retention:        {}", format_duration(topic.config.retention_ms));
                        println!("    Segment Size:     {}", format_bytes(topic.config.segment_bytes));
                        println!("    Min ISR:          {}", topic.config.min_insync_replicas);
                        println!("    Compression:      {}", topic.config.compression_type);
                    }
                    _ => println!("{}", output::format_output(&topic, format)?),
                }
            }
            TopicSubcommands::Delete { name, yes } => {
                if !yes {
                    println!("Are you sure you want to delete topic '{}'? (y/N)", name);
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    if !input.trim().eq_ignore_ascii_case("y") {
                        output::print_info("Deletion cancelled");
                        return Ok(());
                    }
                }
                
                client.delete(&format!("/api/v1/topics/{}", name)).await?;
                output::print_success(&format!("Topic '{}' deleted successfully", name));
            }
        }
        
        Ok(())
    }
}

// Response types

#[derive(Debug, Serialize, Deserialize)]
struct Topic {
    name: String,
    partitions: i32,
    replication_factor: i32,
    config: TopicConfig,
}

#[derive(Debug, Serialize, Deserialize)]
struct TopicConfig {
    retention_ms: i64,
    segment_bytes: i64,
    min_insync_replicas: i32,
    compression_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateTopicRequest {
    name: String,
    partitions: i32,
    replication_factor: i32,
    config: Option<TopicConfig>,
}

#[derive(Debug, Tabled)]
struct TopicTable {
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "PARTITIONS")]
    partitions: i32,
    #[tabled(rename = "REPLICATION")]
    replication_factor: i32,
    #[tabled(rename = "RETENTION")]
    retention_ms: String,
}

// Helper functions

fn format_duration(ms: i64) -> String {
    let seconds = ms / 1000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    
    if days > 0 {
        format!("{}d", days)
    } else if hours > 0 {
        format!("{}h", hours)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        format!("{}s", seconds)
    }
}

fn format_bytes(bytes: i64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;
    
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    
    format!("{:.1} {}", size, UNITS[unit_index])
}