//! CLI module for Chronik Server
//!
//! Provides command-line interfaces for:
//! - Cluster management (status, nodes, partitions, rebalancing)
//! - Health checks and diagnostics
//! - Administrative operations

pub mod cluster;
pub mod output;
pub mod client;

pub use cluster::ClusterCommand;
pub use output::{OutputFormat, FormatTable};
pub use client::ClusterClient;
