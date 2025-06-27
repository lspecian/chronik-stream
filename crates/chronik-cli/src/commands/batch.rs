//! Batch operations commands.

use crate::{client::AdminClient, output};
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Batch operations commands
#[derive(Debug, clap::Parser)]
pub struct BatchCommand {
    #[command(subcommand)]
    command: BatchSubcommands,
}

#[derive(Debug, Subcommand)]
enum BatchSubcommands {
    /// Execute batch operations from a YAML file
    Execute {
        /// Path to the batch operations YAML file
        file: PathBuf,
        
        /// Continue on error
        #[arg(short, long)]
        continue_on_error: bool,
        
        /// Dry run (don't execute, just validate)
        #[arg(short, long)]
        dry_run: bool,
    },
    
    /// Generate example batch file
    Example {
        /// Path to write example file
        #[arg(default_value = "batch-example.yaml")]
        output: PathBuf,
    },
}

impl BatchCommand {
    pub async fn execute(
        &self,
        client: &AdminClient,
        format: super::super::OutputFormat,
    ) -> Result<()> {
        match &self.command {
            BatchSubcommands::Execute { file, continue_on_error, dry_run } => {
                let content = fs::read_to_string(file)?;
                let batch: BatchOperations = serde_yaml::from_str(&content)?;
                
                output::print_info(&format!("Loaded {} operations from {}", 
                    batch.operations.len(), file.display()));
                
                if *dry_run {
                    output::print_info("DRY RUN - Validating operations:");
                    for (idx, op) in batch.operations.iter().enumerate() {
                        println!("  [{}] {} - {}", idx + 1, op.operation_type(), op.description());
                    }
                    output::print_success("All operations validated successfully");
                    return Ok(());
                }
                
                let mut results = Vec::new();
                for (idx, op) in batch.operations.iter().enumerate() {
                    output::print_info(&format!("[{}/{}] Executing: {}", 
                        idx + 1, batch.operations.len(), op.description()));
                    
                    match execute_operation(client, op).await {
                        Ok(result) => {
                            output::print_success(&format!("  ✓ {}", result));
                            results.push(BatchResult::success(op.description(), result));
                        }
                        Err(e) => {
                            output::print_error(&format!("  ✗ Error: {}", e));
                            results.push(BatchResult::error(op.description(), e.to_string()));
                            
                            if !continue_on_error {
                                output::print_error("Batch execution stopped due to error");
                                break;
                            }
                        }
                    }
                }
                
                // Print summary
                let successful = results.iter().filter(|r| r.success).count();
                let failed = results.len() - successful;
                
                println!("\n{}", "=".repeat(60));
                output::print_info(&format!("Batch execution completed: {} successful, {} failed", 
                    successful, failed));
                
                // Output results in requested format
                match format {
                    super::super::OutputFormat::Table => {
                        // Already printed inline
                    }
                    _ => {
                        let report = BatchReport {
                            total: results.len(),
                            successful,
                            failed,
                            results,
                        };
                        println!("\n{}", output::format_output(&report, format)?);
                    }
                }
            }
            
            BatchSubcommands::Example { output } => {
                let example = generate_example_batch();
                let yaml = serde_yaml::to_string(&example)?;
                fs::write(output, yaml)?;
                output::print_success(&format!("Example batch file written to {}", output.display()));
            }
        }
        
        Ok(())
    }
}

async fn execute_operation(client: &AdminClient, operation: &BatchOperation) -> Result<String> {
    match operation {
        BatchOperation::CreateTopic { name, partitions, replication_factor, config } => {
            let request = CreateTopicRequest {
                name: name.clone(),
                partitions: *partitions,
                replication_factor: *replication_factor,
                config: config.clone(),
            };
            client.post::<_, serde_json::Value>("/api/v1/topics", &request).await?;
            Ok(format!("Topic '{}' created", name))
        }
        
        BatchOperation::DeleteTopic { name } => {
            client.delete(&format!("/api/v1/topics/{}", name)).await?;
            Ok(format!("Topic '{}' deleted", name))
        }
        
        BatchOperation::CreateUser { username, password, roles } => {
            let request = CreateUserRequest {
                username: username.clone(),
                password: password.clone(),
                roles: roles.clone(),
            };
            client.post::<_, serde_json::Value>("/api/v1/auth/users", &request).await?;
            Ok(format!("User '{}' created", username))
        }
        
        BatchOperation::DeleteUser { username } => {
            client.delete(&format!("/api/v1/auth/users/{}", username)).await?;
            Ok(format!("User '{}' deleted", username))
        }
        
        BatchOperation::UpdateTopicConfig { name, config } => {
            client.patch::<_, serde_json::Value>(
                &format!("/api/v1/topics/{}/config", name), 
                config
            ).await?;
            Ok(format!("Topic '{}' configuration updated", name))
        }
        
        BatchOperation::CreateConsumerGroup { group_id, topics } => {
            let request = CreateGroupRequest {
                group_id: group_id.clone(),
                topics: topics.clone(),
            };
            client.post::<_, serde_json::Value>("/api/v1/groups", &request).await?;
            Ok(format!("Consumer group '{}' created", group_id))
        }
    }
}

fn generate_example_batch() -> BatchOperations {
    BatchOperations {
        version: "1.0".to_string(),
        description: Some("Example batch operations file".to_string()),
        operations: vec![
            BatchOperation::CreateTopic {
                name: "events-stream".to_string(),
                partitions: 10,
                replication_factor: 3,
                config: Some(TopicConfig {
                    retention_ms: 86400000, // 1 day
                    segment_bytes: 1073741824, // 1GB
                    min_insync_replicas: 2,
                    compression_type: "snappy".to_string(),
                }),
            },
            BatchOperation::CreateTopic {
                name: "logs-stream".to_string(),
                partitions: 5,
                replication_factor: 2,
                config: None,
            },
            BatchOperation::CreateUser {
                username: "app-service".to_string(),
                password: "secure-password".to_string(),
                roles: vec!["producer".to_string(), "consumer".to_string()],
            },
            BatchOperation::CreateConsumerGroup {
                group_id: "analytics-group".to_string(),
                topics: vec!["events-stream".to_string(), "logs-stream".to_string()],
            },
            BatchOperation::UpdateTopicConfig {
                name: "events-stream".to_string(),
                config: serde_json::json!({
                    "retention_ms": 172800000, // 2 days
                }),
            },
        ],
    }
}

// Types

#[derive(Debug, Serialize, Deserialize)]
struct BatchOperations {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    operations: Vec<BatchOperation>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BatchOperation {
    CreateTopic {
        name: String,
        partitions: i32,
        replication_factor: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        config: Option<TopicConfig>,
    },
    DeleteTopic {
        name: String,
    },
    CreateUser {
        username: String,
        password: String,
        roles: Vec<String>,
    },
    DeleteUser {
        username: String,
    },
    UpdateTopicConfig {
        name: String,
        config: serde_json::Value,
    },
    CreateConsumerGroup {
        group_id: String,
        topics: Vec<String>,
    },
}

impl BatchOperation {
    fn operation_type(&self) -> &'static str {
        match self {
            BatchOperation::CreateTopic { .. } => "CREATE_TOPIC",
            BatchOperation::DeleteTopic { .. } => "DELETE_TOPIC",
            BatchOperation::CreateUser { .. } => "CREATE_USER",
            BatchOperation::DeleteUser { .. } => "DELETE_USER",
            BatchOperation::UpdateTopicConfig { .. } => "UPDATE_TOPIC_CONFIG",
            BatchOperation::CreateConsumerGroup { .. } => "CREATE_CONSUMER_GROUP",
        }
    }
    
    fn description(&self) -> String {
        match self {
            BatchOperation::CreateTopic { name, .. } => format!("Create topic '{}'", name),
            BatchOperation::DeleteTopic { name } => format!("Delete topic '{}'", name),
            BatchOperation::CreateUser { username, .. } => format!("Create user '{}'", username),
            BatchOperation::DeleteUser { username } => format!("Delete user '{}'", username),
            BatchOperation::UpdateTopicConfig { name, .. } => format!("Update config for topic '{}'", name),
            BatchOperation::CreateConsumerGroup { group_id, .. } => format!("Create consumer group '{}'", group_id),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchResult {
    operation: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl BatchResult {
    fn success(operation: String, result: String) -> Self {
        Self {
            operation,
            success: true,
            result: Some(result),
            error: None,
        }
    }
    
    fn error(operation: String, error: String) -> Self {
        Self {
            operation,
            success: false,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Serialize)]
struct BatchReport {
    total: usize,
    successful: usize,
    failed: usize,
    results: Vec<BatchResult>,
}

// Request types (shared with other commands)

#[derive(Debug, Serialize)]
struct CreateTopicRequest {
    name: String,
    partitions: i32,
    replication_factor: i32,
    config: Option<TopicConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TopicConfig {
    retention_ms: i64,
    segment_bytes: i64,
    min_insync_replicas: i32,
    compression_type: String,
}

#[derive(Debug, Serialize)]
struct CreateUserRequest {
    username: String,
    password: String,
    roles: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CreateGroupRequest {
    group_id: String,
    topics: Vec<String>,
}