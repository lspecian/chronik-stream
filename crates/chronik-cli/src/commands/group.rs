//! Consumer group management commands.

use crate::{client::AdminClient, output};
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use tabled::Tabled;

/// Consumer group management commands
#[derive(Debug, clap::Parser)]
pub struct GroupCommand {
    #[command(subcommand)]
    command: GroupSubcommands,
}

#[derive(Debug, Subcommand)]
enum GroupSubcommands {
    /// List all consumer groups
    List,
    
    /// Show consumer group details
    Get {
        /// Group ID
        id: String,
    },
    
    /// Delete a consumer group
    Delete {
        /// Group ID
        id: String,
        
        /// Skip confirmation
        #[arg(short, long)]
        yes: bool,
    },
    
    /// Show group offsets
    Offsets {
        /// Group ID
        id: String,
    },
}

impl GroupCommand {
    pub async fn execute(
        &self,
        client: &AdminClient,
        format: super::super::OutputFormat,
    ) -> Result<()> {
        match &self.command {
            GroupSubcommands::List => {
                let groups: Vec<ConsumerGroup> = client.get("/api/v1/consumer-groups").await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        let table_data: Vec<GroupTable> = groups
                            .into_iter()
                            .map(|g| GroupTable {
                                group_id: g.group_id,
                                state: g.state,
                                members: g.members,
                                coordinator: g.coordinator,
                                protocol: g.protocol.unwrap_or_else(|| "-".to_string()),
                            })
                            .collect();
                        
                        output::print_table(table_data);
                    }
                    _ => println!("{}", output::format_output(&groups, format)?),
                }
            }
            GroupSubcommands::Get { id } => {
                let group: ConsumerGroup = client
                    .get(&format!("/api/v1/consumer-groups/{}", id))
                    .await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        println!("Consumer Group: {}", group.group_id);
                        println!("  State:       {}", group.state);
                        println!("  Protocol:    {}", group.protocol_type);
                        println!("  Assignment:  {}", group.protocol.as_deref().unwrap_or("-"));
                        println!("  Members:     {}", group.members);
                        println!("  Coordinator: broker-{}", group.coordinator);
                    }
                    _ => println!("{}", output::format_output(&group, format)?),
                }
            }
            GroupSubcommands::Delete { id, yes } => {
                if !yes {
                    println!("Are you sure you want to delete group '{}'? (y/N)", id);
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    if !input.trim().eq_ignore_ascii_case("y") {
                        output::print_info("Deletion cancelled");
                        return Ok(());
                    }
                }
                
                client
                    .delete(&format!("/api/v1/consumer-groups/{}", id))
                    .await?;
                output::print_success(&format!("Group '{}' deleted successfully", id));
            }
            GroupSubcommands::Offsets { id } => {
                let offsets: Vec<GroupOffset> = client
                    .get(&format!("/api/v1/consumer-groups/{}/offsets", id))
                    .await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        let table_data: Vec<OffsetTable> = offsets
                            .into_iter()
                            .map(|o| OffsetTable {
                                topic: o.topic,
                                partition: o.partition,
                                offset: o.offset,
                                log_end_offset: o.log_end_offset,
                                lag: o.lag,
                            })
                            .collect();
                        
                        output::print_table(table_data);
                    }
                    _ => println!("{}", output::format_output(&offsets, format)?),
                }
            }
        }
        
        Ok(())
    }
}

// Response types

#[derive(Debug, Serialize, Deserialize)]
struct ConsumerGroup {
    group_id: String,
    state: String,
    protocol_type: String,
    protocol: Option<String>,
    members: u32,
    coordinator: i32,
}

#[derive(Debug, Serialize, Deserialize)]
struct GroupOffset {
    topic: String,
    partition: i32,
    offset: i64,
    log_end_offset: i64,
    lag: i64,
}

#[derive(Debug, Tabled)]
struct GroupTable {
    #[tabled(rename = "GROUP ID")]
    group_id: String,
    #[tabled(rename = "STATE")]
    state: String,
    #[tabled(rename = "MEMBERS")]
    members: u32,
    #[tabled(rename = "COORDINATOR")]
    coordinator: i32,
    #[tabled(rename = "PROTOCOL")]
    protocol: String,
}

#[derive(Debug, Tabled)]
struct OffsetTable {
    #[tabled(rename = "TOPIC")]
    topic: String,
    #[tabled(rename = "PARTITION")]
    partition: i32,
    #[tabled(rename = "OFFSET")]
    offset: i64,
    #[tabled(rename = "LOG END OFFSET")]
    log_end_offset: i64,
    #[tabled(rename = "LAG")]
    lag: i64,
}