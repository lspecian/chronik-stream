//! Cluster management CLI commands
//!
//! Provides comprehensive commands for managing Raft clusters:
//! - Cluster status and health
//! - Node management (add/remove/list)
//! - Partition management and information
//! - Rebalancing operations
//! - ISR (In-Sync Replicas) management
//! - Metadata operations

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use super::client::ClusterClient;
use super::output::{FormatTable, OutputFormat};

/// Cluster management commands
#[derive(Parser, Debug, Clone)]
#[command(name = "cluster", about = "Cluster management commands")]
pub struct ClusterCommand {
    /// Server address to connect to
    #[arg(long, env = "CHRONIK_SERVER_ADDR", default_value = "http://localhost:5001")]
    pub addr: String,

    /// Output format (table, json, yaml)
    #[arg(long, short = 'o', default_value = "table")]
    pub output: OutputFormat,

    #[command(subcommand)]
    pub command: ClusterSubCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ClusterSubCommand {
    /// Display cluster status
    Status(StatusCommand),

    /// Add a new node to the cluster
    AddNode(AddNodeCommand),

    /// Remove a node from the cluster
    RemoveNode(RemoveNodeCommand),

    /// List all nodes in the cluster
    ListNodes(ListNodesCommand),

    /// List partitions
    ListPartitions(ListPartitionsCommand),

    /// Show partition details
    PartitionInfo(PartitionInfoCommand),

    /// Rebalance partitions across nodes
    Rebalance(RebalanceCommand),

    /// Show ISR status
    IsrStatus(IsrStatusCommand),

    /// Show metadata status
    MetadataStatus(MetadataStatusCommand),

    /// Replicate metadata
    MetadataReplicate(MetadataReplicateCommand),

    /// Check cluster health
    Health(HealthCommand),

    /// Ping a specific node
    PingNode(PingNodeCommand),
}

/// Cluster status command
#[derive(Parser, Debug, Clone)]
pub struct StatusCommand {
    /// Show detailed statistics
    #[arg(long)]
    pub detailed: bool,
}

/// Add node command
#[derive(Parser, Debug, Clone)]
pub struct AddNodeCommand {
    /// Node ID
    #[arg(long)]
    pub id: u64,

    /// Node address (host:port)
    #[arg(long)]
    pub addr: String,

    /// Raft port (default: 5001)
    #[arg(long, default_value = "5001")]
    pub raft_port: u16,
}

/// Remove node command
#[derive(Parser, Debug, Clone)]
pub struct RemoveNodeCommand {
    /// Node ID to remove
    #[arg(long)]
    pub id: u64,

    /// Force removal even if node is healthy
    #[arg(long)]
    pub force: bool,
}

/// List nodes command
#[derive(Parser, Debug, Clone)]
pub struct ListNodesCommand {
    /// Show only healthy nodes
    #[arg(long)]
    pub healthy_only: bool,

    /// Show only unhealthy nodes
    #[arg(long)]
    pub unhealthy_only: bool,
}

/// List partitions command
#[derive(Parser, Debug, Clone)]
pub struct ListPartitionsCommand {
    /// Filter by topic name
    #[arg(long)]
    pub topic: Option<String>,

    /// Show only partitions on this node
    #[arg(long)]
    pub node: Option<u64>,
}

/// Partition info command
#[derive(Parser, Debug, Clone)]
pub struct PartitionInfoCommand {
    /// Topic name
    #[arg(long)]
    pub topic: String,

    /// Partition number
    #[arg(long)]
    pub partition: i32,
}

/// Rebalance command
#[derive(Parser, Debug, Clone)]
pub struct RebalanceCommand {
    /// Dry run - show what would be rebalanced
    #[arg(long)]
    pub dry_run: bool,

    /// Maximum number of partition moves
    #[arg(long, default_value = "10")]
    pub max_moves: u32,

    /// Target topic (rebalance only this topic)
    #[arg(long)]
    pub topic: Option<String>,
}

/// ISR status command
#[derive(Parser, Debug, Clone)]
pub struct IsrStatusCommand {
    /// Filter by topic
    #[arg(long)]
    pub topic: Option<String>,

    /// Show only under-replicated partitions
    #[arg(long)]
    pub under_replicated_only: bool,
}

/// Metadata status command
#[derive(Parser, Debug, Clone)]
pub struct MetadataStatusCommand {
    /// Show detailed metadata statistics
    #[arg(long)]
    pub detailed: bool,
}

/// Metadata replicate command
#[derive(Parser, Debug, Clone)]
pub struct MetadataReplicateCommand {
    /// Metadata key
    #[arg(long)]
    pub key: String,

    /// Metadata value (JSON)
    #[arg(long)]
    pub value: String,
}

/// Health check command
#[derive(Parser, Debug, Clone)]
pub struct HealthCommand {
    /// Timeout for health checks (seconds)
    #[arg(long, default_value = "5")]
    pub timeout: u64,
}

/// Ping node command
#[derive(Parser, Debug, Clone)]
pub struct PingNodeCommand {
    /// Node ID to ping
    #[arg(long)]
    pub id: u64,

    /// Number of pings
    #[arg(long, short = 'c', default_value = "1")]
    pub count: u32,
}

// Data structures for cluster information

/// Cluster status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterStatus {
    pub total_nodes: u32,
    pub healthy_nodes: u32,
    pub unhealthy_nodes: u32,
    pub metadata_leader: Option<u64>,
    pub nodes: Vec<NodeInfo>,
}

/// Node information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: u64,
    pub address: String,
    pub raft_port: u16,
    pub status: NodeStatus,
    pub partition_count: u32,
}

/// Node status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeStatus {
    Healthy,
    Unhealthy,
    Unknown,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeStatus::Healthy => write!(f, "Healthy"),
            NodeStatus::Unhealthy => write!(f, "Unhealthy"),
            NodeStatus::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Partition information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub topic: String,
    pub partition: i32,
    pub leader: Option<u64>,
    pub replicas: Vec<u64>,
    pub isr: Vec<u64>,
    pub applied_index: u64,
    pub committed_index: u64,
    pub lag: u64,
}

/// Rebalance plan
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebalancePlan {
    pub current_imbalance: f64,
    pub partitions_to_move: u32,
    pub moves: Vec<PartitionMove>,
    pub estimated_duration_secs: u64,
}

/// Partition move operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionMove {
    pub topic: String,
    pub partition: i32,
    pub from_node: u64,
    pub to_node: u64,
}

/// ISR status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsrStatus {
    pub topic: String,
    pub partition: i32,
    pub leader: Option<u64>,
    pub isr: Vec<u64>,
    pub replicas: Vec<u64>,
    pub is_under_replicated: bool,
}

/// Metadata status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataStatus {
    pub leader_node: Option<u64>,
    pub total_entries: u64,
    pub applied_index: u64,
    pub committed_index: u64,
    pub last_updated: String,
}

/// Health check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    pub cluster_healthy: bool,
    pub nodes: Vec<NodeHealth>,
    pub issues: Vec<String>,
}

/// Node health information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHealth {
    pub id: u64,
    pub address: String,
    pub is_alive: bool,
    pub response_time_ms: u64,
}

/// Ping result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub node_id: u64,
    pub address: String,
    pub successful_pings: u32,
    pub failed_pings: u32,
    pub avg_response_time_ms: u64,
}

// Command execution implementations

impl ClusterCommand {
    /// Execute the cluster command
    pub async fn execute(&self) -> Result<()> {
        let client = ClusterClient::connect(&self.addr).await?;

        match &self.command {
            ClusterSubCommand::Status(cmd) => {
                execute_status(&client, cmd, self.output).await
            }
            ClusterSubCommand::AddNode(cmd) => {
                execute_add_node(&client, cmd, self.output).await
            }
            ClusterSubCommand::RemoveNode(cmd) => {
                execute_remove_node(&client, cmd, self.output).await
            }
            ClusterSubCommand::ListNodes(cmd) => {
                execute_list_nodes(&client, cmd, self.output).await
            }
            ClusterSubCommand::ListPartitions(cmd) => {
                execute_list_partitions(&client, cmd, self.output).await
            }
            ClusterSubCommand::PartitionInfo(cmd) => {
                execute_partition_info(&client, cmd, self.output).await
            }
            ClusterSubCommand::Rebalance(cmd) => {
                execute_rebalance(&client, cmd, self.output).await
            }
            ClusterSubCommand::IsrStatus(cmd) => {
                execute_isr_status(&client, cmd, self.output).await
            }
            ClusterSubCommand::MetadataStatus(cmd) => {
                execute_metadata_status(&client, cmd, self.output).await
            }
            ClusterSubCommand::MetadataReplicate(cmd) => {
                execute_metadata_replicate(&client, cmd, self.output).await
            }
            ClusterSubCommand::Health(cmd) => {
                execute_health(&client, cmd, self.output).await
            }
            ClusterSubCommand::PingNode(cmd) => {
                execute_ping_node(&client, cmd, self.output).await
            }
        }
    }
}

// Individual command executors

async fn execute_status(
    client: &ClusterClient,
    cmd: &StatusCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let status = client.get_cluster_status(cmd.detailed).await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&status)?);
        }
        OutputFormat::Table => {
            println!("Cluster Status");
            println!("==============");
            println!("Nodes: {}", status.total_nodes);
            println!("Healthy: {}", status.healthy_nodes);
            println!("Unhealthy: {}", status.unhealthy_nodes);
            println!(
                "Metadata Leader: {}",
                status
                    .metadata_leader
                    .map(|id| format!("Node {}", id))
                    .unwrap_or_else(|| "None".to_string())
            );
            println!();

            status.nodes.format_table()?;
        }
    }

    Ok(())
}

async fn execute_add_node(
    client: &ClusterClient,
    cmd: &AddNodeCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let result = client.add_node(cmd.id, &cmd.addr, cmd.raft_port).await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&result)?);
        }
        OutputFormat::Table => {
            println!("Node added successfully:");
            println!("  ID: {}", cmd.id);
            println!("  Address: {}", cmd.addr);
            println!("  Raft Port: {}", cmd.raft_port);
        }
    }

    Ok(())
}

async fn execute_remove_node(
    client: &ClusterClient,
    cmd: &RemoveNodeCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let result = client.remove_node(cmd.id, cmd.force).await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&result)?);
        }
        OutputFormat::Table => {
            println!("Node {} removed successfully", cmd.id);
        }
    }

    Ok(())
}

async fn execute_list_nodes(
    client: &ClusterClient,
    cmd: &ListNodesCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let nodes = client.list_nodes().await?;

    // Filter based on health status
    let filtered_nodes: Vec<_> = nodes
        .into_iter()
        .filter(|node| {
            if cmd.healthy_only {
                node.status == NodeStatus::Healthy
            } else if cmd.unhealthy_only {
                node.status != NodeStatus::Healthy
            } else {
                true
            }
        })
        .collect();

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&filtered_nodes)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&filtered_nodes)?);
        }
        OutputFormat::Table => {
            filtered_nodes.format_table()?;
        }
    }

    Ok(())
}

async fn execute_list_partitions(
    client: &ClusterClient,
    cmd: &ListPartitionsCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let partitions = client
        .list_partitions(cmd.topic.as_deref(), cmd.node)
        .await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&partitions)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&partitions)?);
        }
        OutputFormat::Table => {
            partitions.format_table()?;
        }
    }

    Ok(())
}

async fn execute_partition_info(
    client: &ClusterClient,
    cmd: &PartitionInfoCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let info = client
        .get_partition_info(&cmd.topic, cmd.partition)
        .await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&info)?);
        }
        OutputFormat::Table => {
            println!("Partition: {}/{}", info.topic, info.partition);
            println!(
                "Leader: {}",
                info.leader
                    .map(|id| format!("Node {}", id))
                    .unwrap_or_else(|| "None".to_string())
            );
            println!("Replicas: {:?}", info.replicas);
            println!("ISR: {:?}", info.isr);
            println!("Applied Index: {}", info.applied_index);
            println!("Committed Index: {}", info.committed_index);
            println!("Lag: {} entries", info.lag);
        }
    }

    Ok(())
}

async fn execute_rebalance(
    client: &ClusterClient,
    cmd: &RebalanceCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let plan = client
        .rebalance(cmd.dry_run, cmd.max_moves, cmd.topic.as_deref())
        .await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&plan)?);
        }
        OutputFormat::Table => {
            if cmd.dry_run {
                println!("Rebalance Plan (Dry Run)");
            } else {
                println!("Rebalance Executed");
            }
            println!("========================");
            println!("Current Imbalance: {:.1}%", plan.current_imbalance * 100.0);
            println!("Partitions to Move: {}", plan.partitions_to_move);
            println!();

            if !plan.moves.is_empty() {
                plan.moves.format_table()?;
                println!();
                println!(
                    "Estimated Duration: {}m {}s",
                    plan.estimated_duration_secs / 60,
                    plan.estimated_duration_secs % 60
                );
            } else {
                println!("No partition moves needed - cluster is balanced");
            }
        }
    }

    Ok(())
}

async fn execute_isr_status(
    client: &ClusterClient,
    cmd: &IsrStatusCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let isr_statuses = client
        .get_isr_status(cmd.topic.as_deref(), cmd.under_replicated_only)
        .await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&isr_statuses)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&isr_statuses)?);
        }
        OutputFormat::Table => {
            isr_statuses.format_table()?;
        }
    }

    Ok(())
}

async fn execute_metadata_status(
    client: &ClusterClient,
    cmd: &MetadataStatusCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let status = client.get_metadata_status(cmd.detailed).await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&status)?);
        }
        OutputFormat::Table => {
            println!("Metadata Status");
            println!("===============");
            println!(
                "Leader Node: {}",
                status
                    .leader_node
                    .map(|id| format!("Node {}", id))
                    .unwrap_or_else(|| "None".to_string())
            );
            println!("Total Entries: {}", status.total_entries);
            println!("Applied Index: {}", status.applied_index);
            println!("Committed Index: {}", status.committed_index);
            println!("Last Updated: {}", status.last_updated);
        }
    }

    Ok(())
}

async fn execute_metadata_replicate(
    client: &ClusterClient,
    cmd: &MetadataReplicateCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let result = client
        .replicate_metadata(&cmd.key, &cmd.value)
        .await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&result)?);
        }
        OutputFormat::Table => {
            println!("Metadata replicated successfully:");
            println!("  Key: {}", cmd.key);
            println!("  Value: {}", cmd.value);
        }
    }

    Ok(())
}

async fn execute_health(
    client: &ClusterClient,
    cmd: &HealthCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let health = client.check_health(cmd.timeout).await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&health)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&health)?);
        }
        OutputFormat::Table => {
            println!("Cluster Health Check");
            println!("====================");
            println!(
                "Overall Status: {}",
                if health.cluster_healthy {
                    "HEALTHY"
                } else {
                    "UNHEALTHY"
                }
            );
            println!();

            if !health.issues.is_empty() {
                println!("Issues:");
                for issue in &health.issues {
                    println!("  - {}", issue);
                }
                println!();
            }

            health.nodes.format_table()?;
        }
    }

    Ok(())
}

async fn execute_ping_node(
    client: &ClusterClient,
    cmd: &PingNodeCommand,
    output_format: OutputFormat,
) -> Result<()> {
    let result = client.ping_node(cmd.id, cmd.count).await?;

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&result)?);
        }
        OutputFormat::Table => {
            println!("Ping Results for Node {}", result.node_id);
            println!("=======================");
            println!("Address: {}", result.address);
            println!("Successful Pings: {}", result.successful_pings);
            println!("Failed Pings: {}", result.failed_pings);
            println!(
                "Average Response Time: {} ms",
                result.avg_response_time_ms
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_parse_status_command() {
        let args = vec!["cluster", "status"];
        let cmd = ClusterCommand::try_parse_from(args);
        assert!(cmd.is_ok());

        let cmd = cmd.unwrap();
        assert!(matches!(cmd.command, ClusterSubCommand::Status(_)));
    }

    #[test]
    fn test_parse_status_detailed() {
        let args = vec!["cluster", "status", "--detailed"];
        let cmd = ClusterCommand::try_parse_from(args);
        assert!(cmd.is_ok());

        let cmd = cmd.unwrap();
        if let ClusterSubCommand::Status(status_cmd) = cmd.command {
            assert!(status_cmd.detailed);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_parse_add_node() {
        let args = vec![
            "cluster",
            "add-node",
            "--id",
            "4",
            "--addr",
            "192.168.1.40:9092",
            "--raft-port",
            "9093",
        ];
        let cmd = ClusterCommand::try_parse_from(args);
        assert!(cmd.is_ok());

        let cmd = cmd.unwrap();
        if let ClusterSubCommand::AddNode(add_cmd) = cmd.command {
            assert_eq!(add_cmd.id, 4);
            assert_eq!(add_cmd.addr, "192.168.1.40:9092");
            assert_eq!(add_cmd.raft_port, 9093);
        } else {
            panic!("Expected AddNode command");
        }
    }

    #[test]
    fn test_parse_rebalance_dry_run() {
        let args = vec!["cluster", "rebalance", "--dry-run", "--max-moves", "5"];
        let cmd = ClusterCommand::try_parse_from(args);
        assert!(cmd.is_ok());

        let cmd = cmd.unwrap();
        if let ClusterSubCommand::Rebalance(rebalance_cmd) = cmd.command {
            assert!(rebalance_cmd.dry_run);
            assert_eq!(rebalance_cmd.max_moves, 5);
        } else {
            panic!("Expected Rebalance command");
        }
    }

    #[test]
    fn test_parse_partition_info() {
        let args = vec![
            "cluster",
            "partition-info",
            "--topic",
            "my-topic",
            "--partition",
            "0",
        ];
        let cmd = ClusterCommand::try_parse_from(args);
        assert!(cmd.is_ok());

        let cmd = cmd.unwrap();
        if let ClusterSubCommand::PartitionInfo(info_cmd) = cmd.command {
            assert_eq!(info_cmd.topic, "my-topic");
            assert_eq!(info_cmd.partition, 0);
        } else {
            panic!("Expected PartitionInfo command");
        }
    }

    #[test]
    fn test_output_format_json() {
        let args = vec!["cluster", "--output", "json", "status"];
        let cmd = ClusterCommand::try_parse_from(args);
        assert!(cmd.is_ok());

        let cmd = cmd.unwrap();
        assert!(matches!(cmd.output, OutputFormat::Json));
    }

    #[test]
    fn test_node_status_display() {
        assert_eq!(format!("{}", NodeStatus::Healthy), "Healthy");
        assert_eq!(format!("{}", NodeStatus::Unhealthy), "Unhealthy");
        assert_eq!(format!("{}", NodeStatus::Unknown), "Unknown");
    }

    #[test]
    fn test_cluster_command_verify() {
        // Verify command structure is valid
        ClusterCommand::command().debug_assert();
    }
}
