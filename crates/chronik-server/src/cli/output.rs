//! Output formatting utilities for CLI commands
//!
//! Provides table formatting, JSON, and YAML output for cluster data

use anyhow::Result;
use clap::ValueEnum;
use tabled::{
    settings::{object::Rows, Alignment, Modify, Style},
    Table, Tabled,
};

use super::cluster::{
    IsrStatus, NodeHealth, NodeInfo, PartitionInfo, PartitionMove,
};

/// Output format for CLI commands
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table format
    Table,
    /// JSON format
    Json,
    /// YAML format
    Yaml,
}

/// Trait for types that can be formatted as tables
pub trait FormatTable {
    fn format_table(&self) -> Result<()>;
}

// Implement Tabled for NodeInfo
impl Tabled for NodeInfo {
    const LENGTH: usize = 5;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.id.to_string().into(),
            self.address.clone().into(),
            self.raft_port.to_string().into(),
            self.status.to_string().into(),
            self.partition_count.to_string().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Node".into(),
            "Address".into(),
            "Raft Port".into(),
            "Status".into(),
            "Partitions".into(),
        ]
    }
}

impl FormatTable for Vec<NodeInfo> {
    fn format_table(&self) -> Result<()> {
        if self.is_empty() {
            println!("No nodes found");
            return Ok(());
        }

        let mut table = Table::new(self);
        table
            .with(Style::modern())
            .with(Modify::new(Rows::first()).with(Alignment::center()));

        println!("{}", table);
        Ok(())
    }
}

// Implement Tabled for PartitionInfo
impl Tabled for PartitionInfo {
    const LENGTH: usize = 7;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.topic.clone().into(),
            self.partition.to_string().into(),
            self.leader
                .map(|id| id.to_string())
                .unwrap_or_else(|| "None".to_string())
                .into(),
            format!("{:?}", self.replicas).into(),
            format!("{:?}", self.isr).into(),
            self.applied_index.to_string().into(),
            self.lag.to_string().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Topic".into(),
            "Partition".into(),
            "Leader".into(),
            "Replicas".into(),
            "ISR".into(),
            "Applied Index".into(),
            "Lag".into(),
        ]
    }
}

impl FormatTable for Vec<PartitionInfo> {
    fn format_table(&self) -> Result<()> {
        if self.is_empty() {
            println!("No partitions found");
            return Ok(());
        }

        let mut table = Table::new(self);
        table
            .with(Style::modern())
            .with(Modify::new(Rows::first()).with(Alignment::center()));

        println!("{}", table);
        Ok(())
    }
}

// Implement Tabled for PartitionMove
impl Tabled for PartitionMove {
    const LENGTH: usize = 4;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.topic.clone().into(),
            self.partition.to_string().into(),
            self.from_node.to_string().into(),
            self.to_node.to_string().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Topic".into(),
            "Partition".into(),
            "From Node".into(),
            "To Node".into(),
        ]
    }
}

impl FormatTable for Vec<PartitionMove> {
    fn format_table(&self) -> Result<()> {
        if self.is_empty() {
            println!("No partition moves");
            return Ok(());
        }

        let mut table = Table::new(self);
        table
            .with(Style::modern())
            .with(Modify::new(Rows::first()).with(Alignment::center()));

        println!("{}", table);
        Ok(())
    }
}

// Implement Tabled for IsrStatus
impl Tabled for IsrStatus {
    const LENGTH: usize = 5;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.topic.clone().into(),
            self.partition.to_string().into(),
            self.leader
                .map(|id| id.to_string())
                .unwrap_or_else(|| "None".to_string())
                .into(),
            format!("{:?}", self.isr).into(),
            if self.is_under_replicated {
                "YES".to_string()
            } else {
                "NO".to_string()
            }
            .into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Topic".into(),
            "Partition".into(),
            "Leader".into(),
            "ISR".into(),
            "Under-Replicated".into(),
        ]
    }
}

impl FormatTable for Vec<IsrStatus> {
    fn format_table(&self) -> Result<()> {
        if self.is_empty() {
            println!("No ISR status information available");
            return Ok(());
        }

        let mut table = Table::new(self);
        table
            .with(Style::modern())
            .with(Modify::new(Rows::first()).with(Alignment::center()));

        println!("{}", table);
        Ok(())
    }
}

// Implement Tabled for NodeHealth
impl Tabled for NodeHealth {
    const LENGTH: usize = 4;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.id.to_string().into(),
            self.address.clone().into(),
            if self.is_alive {
                "Alive".to_string()
            } else {
                "Dead".to_string()
            }
            .into(),
            format!("{} ms", self.response_time_ms).into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Node".into(),
            "Address".into(),
            "Status".into(),
            "Response Time".into(),
        ]
    }
}

impl FormatTable for Vec<NodeHealth> {
    fn format_table(&self) -> Result<()> {
        if self.is_empty() {
            println!("No node health information available");
            return Ok(());
        }

        let mut table = Table::new(self);
        table
            .with(Style::modern())
            .with(Modify::new(Rows::first()).with(Alignment::center()));

        println!("{}", table);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::cluster::NodeStatus;

    #[test]
    fn test_format_node_info_table() {
        let nodes = vec![
            NodeInfo {
                id: 1,
                address: "192.168.1.10:9092".to_string(),
                raft_port: 5001,
                status: NodeStatus::Healthy,
                partition_count: 12,
            },
            NodeInfo {
                id: 2,
                address: "192.168.1.11:9092".to_string(),
                raft_port: 5001,
                status: NodeStatus::Healthy,
                partition_count: 12,
            },
        ];

        // Should not panic
        assert!(nodes.format_table().is_ok());
    }

    #[test]
    fn test_format_empty_nodes() {
        let nodes: Vec<NodeInfo> = vec![];
        // Should handle empty list gracefully
        assert!(nodes.format_table().is_ok());
    }

    #[test]
    fn test_format_partition_info_table() {
        let partitions = vec![PartitionInfo {
            topic: "test-topic".to_string(),
            partition: 0,
            leader: Some(1),
            replicas: vec![1, 2, 3],
            isr: vec![1, 2, 3],
            applied_index: 1000,
            committed_index: 1000,
            lag: 0,
        }];

        assert!(partitions.format_table().is_ok());
    }

    #[test]
    fn test_format_partition_moves() {
        let moves = vec![
            PartitionMove {
                topic: "my-topic".to_string(),
                partition: 2,
                from_node: 1,
                to_node: 3,
            },
            PartitionMove {
                topic: "other-topic".to_string(),
                partition: 5,
                from_node: 1,
                to_node: 2,
            },
        ];

        assert!(moves.format_table().is_ok());
    }

    #[test]
    fn test_format_isr_status() {
        let isr_statuses = vec![IsrStatus {
            topic: "test-topic".to_string(),
            partition: 0,
            leader: Some(1),
            isr: vec![1, 2],
            replicas: vec![1, 2, 3],
            is_under_replicated: true,
        }];

        assert!(isr_statuses.format_table().is_ok());
    }

    #[test]
    fn test_format_node_health() {
        let health = vec![
            NodeHealth {
                id: 1,
                address: "192.168.1.10:9092".to_string(),
                is_alive: true,
                response_time_ms: 5,
            },
            NodeHealth {
                id: 2,
                address: "192.168.1.11:9092".to_string(),
                is_alive: false,
                response_time_ms: 0,
            },
        ];

        assert!(health.format_table().is_ok());
    }

    #[test]
    fn test_output_format_variants() {
        // Ensure all variants are accessible
        let _ = OutputFormat::Table;
        let _ = OutputFormat::Json;
        let _ = OutputFormat::Yaml;
    }
}
