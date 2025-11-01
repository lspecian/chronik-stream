//! Raft configuration types
//!
//! This module defines configuration for Raft consensus nodes.
//! STUB: To be fully implemented in Phase 2.

use serde::{Deserialize, Serialize};

/// Configuration for a Raft node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaftConfig {
    /// Unique node ID
    pub node_id: u64,

    /// gRPC listen address (e.g., "0.0.0.0:5001")
    pub listen_addr: String,

    /// Election timeout in milliseconds (150-300ms recommended)
    pub election_timeout_ms: u64,

    /// Heartbeat interval in milliseconds (typically 1/10 of election_timeout)
    pub heartbeat_interval_ms: u64,

    /// Maximum number of entries per AppendEntries RPC
    pub max_entries_per_batch: usize,

    /// Snapshot threshold (number of log entries before triggering snapshot)
    pub snapshot_threshold: u64,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            node_id: 0,
            listen_addr: "0.0.0.0:5001".to_string(),
            election_timeout_ms: 300,
            heartbeat_interval_ms: 30,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        }
    }
}
