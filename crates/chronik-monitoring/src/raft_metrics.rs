//! Raft-specific metrics
//!
//! Simple placeholder for Raft metrics. This module provides a minimal
//! implementation that can be expanded later with proper Prometheus integration.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Metrics for Raft consensus operations
#[derive(Clone)]
pub struct RaftMetrics {
    /// Node ID
    pub node_id: u64,

    /// Total number of leader elections
    pub leader_elections_total: Arc<AtomicU64>,

    /// Total number of append entries
    pub append_entries_total: Arc<AtomicU64>,

    /// Total number of vote requests
    pub vote_requests_total: Arc<AtomicU64>,

    /// Total number of heartbeats
    pub heartbeats_total: Arc<AtomicU64>,

    /// Current number of log entries
    pub log_entries: Arc<AtomicU64>,

    /// Current Raft term
    pub current_term: Arc<AtomicU64>,

    /// Current commit index
    pub commit_index: Arc<AtomicU64>,
}

impl RaftMetrics {
    /// Create new Raft metrics (default node_id = 0)
    pub fn new() -> Self {
        Self::with_node_id(0)
    }

    /// Create new Raft metrics for a given node
    pub fn with_node_id(node_id: u64) -> Self {
        Self {
            node_id,
            leader_elections_total: Arc::new(AtomicU64::new(0)),
            append_entries_total: Arc::new(AtomicU64::new(0)),
            vote_requests_total: Arc::new(AtomicU64::new(0)),
            heartbeats_total: Arc::new(AtomicU64::new(0)),
            log_entries: Arc::new(AtomicU64::new(0)),
            current_term: Arc::new(AtomicU64::new(0)),
            commit_index: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Increment leader elections counter
    pub fn inc_leader_elections(&self) {
        self.leader_elections_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment append entries counter
    pub fn inc_append_entries(&self) {
        self.append_entries_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment vote requests counter
    pub fn inc_vote_requests(&self) {
        self.vote_requests_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment heartbeats counter
    pub fn inc_heartbeats(&self) {
        self.heartbeats_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Set log entries gauge
    pub fn set_log_entries(&self, value: u64) {
        self.log_entries.store(value, Ordering::Relaxed);
    }

    /// Set current term gauge
    pub fn set_current_term(&self, _topic: &str, _partition: i32, value: u64) {
        self.current_term.store(value, Ordering::Relaxed);
    }

    /// Set commit index gauge
    pub fn set_commit_index(&self, _topic: &str, _partition: i32, value: u64) {
        self.commit_index.store(value, Ordering::Relaxed);
    }

    /// Get current values as a snapshot
    pub fn snapshot(&self) -> RaftMetricsSnapshot {
        RaftMetricsSnapshot {
            node_id: self.node_id,
            leader_elections_total: self.leader_elections_total.load(Ordering::Relaxed),
            append_entries_total: self.append_entries_total.load(Ordering::Relaxed),
            vote_requests_total: self.vote_requests_total.load(Ordering::Relaxed),
            heartbeats_total: self.heartbeats_total.load(Ordering::Relaxed),
            log_entries: self.log_entries.load(Ordering::Relaxed),
            current_term: self.current_term.load(Ordering::Relaxed),
            commit_index: self.commit_index.load(Ordering::Relaxed),
        }
    }

    // Additional methods needed by chronik-raft (no-op implementations for now)

    /// Record an RPC call
    pub fn record_rpc_call(&self, _rpc_type: &str, _target: &str, _success: bool, _latency_ms: f64) {
        // No-op for now - will be implemented when we add detailed metrics
    }

    /// Record append entries sent
    pub fn record_append_entries_sent(&self, _topic: &str, _partition: i32, _target: &str, _size_bytes: u64) {
        self.inc_append_entries();
    }

    /// Record RPC error
    pub fn record_rpc_error(&self, _rpc_type: &str, _target: &str, _error_type: &str) {
        // No-op for now
    }

    /// Set node state
    pub fn set_node_state(&self, _topic: &str, _partition: i32, _node_id: &str, _state: u64) {
        // No-op for now
    }

    /// Set last applied index
    pub fn set_last_applied(&self, _topic: &str, _partition: i32, _last_applied: u64) {
        // No-op for now
    }

    /// Record commit latency
    pub fn record_commit_latency(&self, _topic: &str, _partition: i32, _latency_ms: f64, _count: usize) {
        // No-op for now
    }

    /// Set leader count for this node
    pub fn set_leader_count(&self, _node_id: &str, _count: usize) {
        // No-op for now
    }

    /// Set follower count for this node
    pub fn set_follower_count(&self, _node_id: &str, _count: usize) {
        // No-op for now
    }
}

impl Default for RaftMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of Raft metrics at a point in time
#[derive(Debug, Clone)]
pub struct RaftMetricsSnapshot {
    pub node_id: u64,
    pub leader_elections_total: u64,
    pub append_entries_total: u64,
    pub vote_requests_total: u64,
    pub heartbeats_total: u64,
    pub log_entries: u64,
    pub current_term: u64,
    pub commit_index: u64,
}
