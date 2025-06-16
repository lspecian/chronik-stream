//! Broker management commands.

use crate::{client::AdminClient, output};
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use tabled::Tabled;

/// Broker management commands
#[derive(Debug, clap::Parser)]
pub struct BrokerCommand {
    #[command(subcommand)]
    command: BrokerSubcommands,
}

#[derive(Debug, Subcommand)]
enum BrokerSubcommands {
    /// List all brokers
    List,
    
    /// Show broker details
    Get {
        /// Broker ID
        id: i32,
    },
}

impl BrokerCommand {
    pub async fn execute(
        &self,
        client: &AdminClient,
        format: super::super::OutputFormat,
    ) -> Result<()> {
        match &self.command {
            BrokerSubcommands::List => {
                let brokers: Vec<Broker> = client.get("/api/v1/brokers").await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        let table_data: Vec<BrokerTable> = brokers
                            .into_iter()
                            .map(|b| BrokerTable {
                                id: b.id,
                                host: b.host,
                                port: b.port,
                                rack: b.rack.unwrap_or_else(|| "-".to_string()),
                                status: b.status,
                                version: b.version,
                            })
                            .collect();
                        
                        output::print_table(table_data);
                    }
                    _ => println!("{}", output::format_output(&brokers, format)?),
                }
            }
            BrokerSubcommands::Get { id } => {
                let broker: Broker = client.get(&format!("/api/v1/brokers/{}", id)).await?;
                
                match format {
                    super::super::OutputFormat::Table => {
                        println!("Broker {}:", broker.id);
                        println!("  Host:    {}", broker.host);
                        println!("  Port:    {}", broker.port);
                        println!("  Rack:    {}", broker.rack.as_deref().unwrap_or("-"));
                        println!("  Status:  {}", broker.status);
                        println!("  Version: {}", broker.version);
                    }
                    _ => println!("{}", output::format_output(&broker, format)?),
                }
            }
        }
        
        Ok(())
    }
}

// Response types

#[derive(Debug, Serialize, Deserialize)]
struct Broker {
    id: i32,
    host: String,
    port: i32,
    rack: Option<String>,
    status: String,
    version: String,
}

#[derive(Debug, Tabled)]
struct BrokerTable {
    #[tabled(rename = "ID")]
    id: i32,
    #[tabled(rename = "HOST")]
    host: String,
    #[tabled(rename = "PORT")]
    port: i32,
    #[tabled(rename = "RACK")]
    rack: String,
    #[tabled(rename = "STATUS")]
    status: String,
    #[tabled(rename = "VERSION")]
    version: String,
}