//! Raft consensus metrics for multi-node replication monitoring.
//!
//! This module provides comprehensive Prometheus metrics for Raft cluster health,
//! replication lag, elections, commit latency, snapshots, and network performance.

use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge_vec, register_histogram_vec,
    CounterVec, GaugeVec, HistogramVec,
};

lazy_static! {
    // ==================== Cluster Health Metrics ====================

    /// Number of leader partitions on this node
    static ref RAFT_LEADER_COUNT: GaugeVec = register_gauge_vec!(
        "chronik_raft_leader_count",
        "Number of partitions where this node is the Raft leader",
        &["node_id"]
    ).unwrap();

    /// Number of follower partitions on this node
    static ref RAFT_FOLLOWER_COUNT: GaugeVec = register_gauge_vec!(
        "chronik_raft_follower_count",
        "Number of partitions where this node is a Raft follower",
        &["node_id"]
    ).unwrap();

    /// Current size of the In-Sync Replica set
    static ref RAFT_ISR_SIZE: GaugeVec = register_gauge_vec!(
        "chronik_raft_isr_size",
        "Number of replicas in the In-Sync Replica set",
        &["topic", "partition"]
    ).unwrap();

    /// Current Raft term for this partition
    static ref RAFT_CURRENT_TERM: GaugeVec = register_gauge_vec!(
        "chronik_raft_current_term",
        "Current Raft term number for this partition",
        &["topic", "partition"]
    ).unwrap();

    /// Raft node state (1=leader, 2=candidate, 3=follower, 0=unknown)
    static ref RAFT_NODE_STATE: GaugeVec = register_gauge_vec!(
        "chronik_raft_node_state",
        "Current Raft state (1=leader, 2=candidate, 3=follower, 0=unknown)",
        &["topic", "partition", "node_id"]
    ).unwrap();

    /// Number of committed Raft log entries
    static ref RAFT_COMMIT_INDEX: GaugeVec = register_gauge_vec!(
        "chronik_raft_commit_index",
        "Index of the highest log entry known to be committed",
        &["topic", "partition"]
    ).unwrap();

    /// Number of applied Raft log entries
    static ref RAFT_LAST_APPLIED: GaugeVec = register_gauge_vec!(
        "chronik_raft_last_applied",
        "Index of the highest log entry applied to state machine",
        &["topic", "partition"]
    ).unwrap();

    // ==================== Replication Lag Metrics ====================

    /// Replication lag in number of log entries
    static ref RAFT_FOLLOWER_LAG_ENTRIES: GaugeVec = register_gauge_vec!(
        "chronik_raft_follower_lag_entries",
        "Number of log entries a follower is behind the leader",
        &["topic", "partition", "follower_id"]
    ).unwrap();

    /// Replication lag in milliseconds
    static ref RAFT_FOLLOWER_LAG_MS: GaugeVec = register_gauge_vec!(
        "chronik_raft_follower_lag_ms",
        "Time in milliseconds a follower is behind the leader",
        &["topic", "partition", "follower_id"]
    ).unwrap();

    /// Maximum replication lag across all followers
    static ref RAFT_MAX_FOLLOWER_LAG: GaugeVec = register_gauge_vec!(
        "chronik_raft_max_follower_lag_entries",
        "Maximum replication lag across all followers for this partition",
        &["topic", "partition"]
    ).unwrap();

    /// Last heartbeat timestamp from follower (Unix epoch ms)
    static ref RAFT_LAST_HEARTBEAT: GaugeVec = register_gauge_vec!(
        "chronik_raft_last_heartbeat_timestamp_ms",
        "Timestamp of last successful heartbeat from follower",
        &["topic", "partition", "follower_id"]
    ).unwrap();

    // ==================== Election Metrics ====================

    /// Total number of Raft elections
    static ref RAFT_ELECTION_COUNT: CounterVec = register_counter_vec!(
        "chronik_raft_election_count",
        "Total number of Raft leader elections",
        &["topic", "partition", "result"]
    ).unwrap();

    /// Election duration in milliseconds
    static ref RAFT_ELECTION_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_raft_election_latency_ms",
        "Time taken for Raft leader election in milliseconds",
        &["topic", "partition"],
        vec![10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0]
    ).unwrap();

    /// Timestamp of last election
    static ref RAFT_LAST_ELECTION_TIMESTAMP: GaugeVec = register_gauge_vec!(
        "chronik_raft_last_election_timestamp_ms",
        "Timestamp of last Raft election (Unix epoch ms)",
        &["topic", "partition"]
    ).unwrap();

    /// Number of votes received in current/last election
    static ref RAFT_VOTES_RECEIVED: GaugeVec = register_gauge_vec!(
        "chronik_raft_votes_received",
        "Number of votes received in current/last election",
        &["topic", "partition"]
    ).unwrap();

    // ==================== Commit Latency Metrics ====================

    /// Time from proposal to commit (end-to-end write latency)
    static ref RAFT_COMMIT_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_raft_commit_latency_ms",
        "Time from log entry proposal to commit in milliseconds",
        &["topic", "partition"],
        vec![1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0]
    ).unwrap();

    /// Total number of committed log entries
    static ref RAFT_COMMITS_TOTAL: CounterVec = register_counter_vec!(
        "chronik_raft_commits_total",
        "Total number of committed Raft log entries",
        &["topic", "partition"]
    ).unwrap();

    /// Number of batched commits
    static ref RAFT_BATCH_COMMITS_TOTAL: CounterVec = register_counter_vec!(
        "chronik_raft_batch_commits_total",
        "Total number of batched commit operations",
        &["topic", "partition"]
    ).unwrap();

    /// Size of commit batches (number of entries per batch)
    static ref RAFT_COMMIT_BATCH_SIZE: HistogramVec = register_histogram_vec!(
        "chronik_raft_commit_batch_size",
        "Number of log entries committed in a single batch",
        &["topic", "partition"],
        vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]
    ).unwrap();

    // ==================== Snapshot Metrics ====================

    /// Total number of snapshots created
    static ref RAFT_SNAPSHOT_COUNT: CounterVec = register_counter_vec!(
        "chronik_raft_snapshot_count",
        "Total number of Raft snapshots created",
        &["topic", "partition", "trigger"]
    ).unwrap();

    /// Size of snapshots in bytes
    static ref RAFT_SNAPSHOT_SIZE: GaugeVec = register_gauge_vec!(
        "chronik_raft_snapshot_size_bytes",
        "Size of the latest Raft snapshot in bytes",
        &["topic", "partition"]
    ).unwrap();

    /// Time to create a snapshot
    static ref RAFT_SNAPSHOT_CREATE_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_raft_snapshot_create_latency_ms",
        "Time to create a Raft snapshot in milliseconds",
        &["topic", "partition"],
        vec![100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0]
    ).unwrap();

    /// Time to apply a snapshot to state machine
    static ref RAFT_SNAPSHOT_APPLY_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_raft_snapshot_apply_latency_ms",
        "Time to apply a Raft snapshot in milliseconds",
        &["topic", "partition"],
        vec![100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0]
    ).unwrap();

    /// Index of last included log entry in snapshot
    static ref RAFT_SNAPSHOT_INDEX: GaugeVec = register_gauge_vec!(
        "chronik_raft_snapshot_last_index",
        "Index of last log entry included in the latest snapshot",
        &["topic", "partition"]
    ).unwrap();

    /// Number of snapshot transfers (sending/receiving)
    static ref RAFT_SNAPSHOT_TRANSFERS: CounterVec = register_counter_vec!(
        "chronik_raft_snapshot_transfers_total",
        "Total number of snapshot transfers",
        &["topic", "partition", "direction", "result"]
    ).unwrap();

    // ==================== Network/RPC Metrics ====================

    /// RPC call latency by type
    static ref RAFT_RPC_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_raft_rpc_latency_ms",
        "Raft RPC call latency in milliseconds",
        &["rpc_type", "target_node"],
        vec![1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]
    ).unwrap();

    /// Total RPC calls by type
    static ref RAFT_RPC_CALLS_TOTAL: CounterVec = register_counter_vec!(
        "chronik_raft_rpc_calls_total",
        "Total number of Raft RPC calls",
        &["rpc_type", "target_node", "result"]
    ).unwrap();

    /// RPC errors by type
    static ref RAFT_RPC_ERRORS: CounterVec = register_counter_vec!(
        "chronik_raft_rpc_errors_total",
        "Total number of Raft RPC errors",
        &["rpc_type", "target_node", "error_type"]
    ).unwrap();

    /// Number of AppendEntries RPCs sent
    static ref RAFT_APPEND_ENTRIES_SENT: CounterVec = register_counter_vec!(
        "chronik_raft_append_entries_sent_total",
        "Total number of AppendEntries RPCs sent",
        &["topic", "partition", "target_node"]
    ).unwrap();

    /// Number of AppendEntries RPCs received
    static ref RAFT_APPEND_ENTRIES_RECEIVED: CounterVec = register_counter_vec!(
        "chronik_raft_append_entries_received_total",
        "Total number of AppendEntries RPCs received",
        &["topic", "partition", "from_node"]
    ).unwrap();

    /// Size of AppendEntries RPC payload in bytes
    static ref RAFT_APPEND_ENTRIES_SIZE: HistogramVec = register_histogram_vec!(
        "chronik_raft_append_entries_size_bytes",
        "Size of AppendEntries RPC payload in bytes",
        &["topic", "partition"],
        vec![100.0, 1000.0, 10000.0, 100000.0, 1000000.0, 10000000.0]
    ).unwrap();

    /// Number of rejected AppendEntries RPCs (stale term, log inconsistency)
    static ref RAFT_APPEND_ENTRIES_REJECTED: CounterVec = register_counter_vec!(
        "chronik_raft_append_entries_rejected_total",
        "Total number of rejected AppendEntries RPCs",
        &["topic", "partition", "reason"]
    ).unwrap();

    // ==================== Log Metrics ====================

    /// Current size of Raft log (number of entries)
    static ref RAFT_LOG_SIZE: GaugeVec = register_gauge_vec!(
        "chronik_raft_log_size_entries",
        "Number of entries in the Raft log",
        &["topic", "partition"]
    ).unwrap();

    /// Size of Raft log on disk in bytes
    static ref RAFT_LOG_SIZE_BYTES: GaugeVec = register_gauge_vec!(
        "chronik_raft_log_size_bytes",
        "Size of Raft log on disk in bytes",
        &["topic", "partition"]
    ).unwrap();

    /// Number of log compactions
    static ref RAFT_LOG_COMPACTIONS: CounterVec = register_counter_vec!(
        "chronik_raft_log_compactions_total",
        "Total number of Raft log compactions",
        &["topic", "partition"]
    ).unwrap();

    // ==================== Quorum Metrics ====================

    /// Number of nodes in the Raft configuration
    static ref RAFT_CLUSTER_SIZE: GaugeVec = register_gauge_vec!(
        "chronik_raft_cluster_size",
        "Number of nodes in the Raft cluster configuration",
        &["topic", "partition"]
    ).unwrap();

    /// Required quorum size (majority)
    static ref RAFT_QUORUM_SIZE: GaugeVec = register_gauge_vec!(
        "chronik_raft_quorum_size",
        "Required quorum size for this partition",
        &["topic", "partition"]
    ).unwrap();

    /// Number of configuration changes
    static ref RAFT_CONFIG_CHANGES: CounterVec = register_counter_vec!(
        "chronik_raft_config_changes_total",
        "Total number of Raft configuration changes",
        &["topic", "partition", "change_type"]
    ).unwrap();
}

/// Raft metrics for cluster health and replication monitoring
pub struct RaftMetrics;

impl RaftMetrics {
    /// Create new Raft metrics instance
    pub fn new() -> Self {
        Self
    }

    // ==================== Cluster Health Methods ====================

    /// Set leader partition count for this node
    pub fn set_leader_count(&self, node_id: &str, count: usize) {
        RAFT_LEADER_COUNT.with_label_values(&[node_id]).set(count as f64);
    }

    /// Set follower partition count for this node
    pub fn set_follower_count(&self, node_id: &str, count: usize) {
        RAFT_FOLLOWER_COUNT.with_label_values(&[node_id]).set(count as f64);
    }

    /// Set ISR size for a partition
    pub fn set_isr_size(&self, topic: &str, partition: i32, size: usize) {
        RAFT_ISR_SIZE
            .with_label_values(&[topic, &partition.to_string()])
            .set(size as f64);
    }

    /// Set current Raft term
    pub fn set_current_term(&self, topic: &str, partition: i32, term: u64) {
        RAFT_CURRENT_TERM
            .with_label_values(&[topic, &partition.to_string()])
            .set(term as f64);
    }

    /// Set Raft node state (1=leader, 2=candidate, 3=follower, 0=unknown)
    pub fn set_node_state(&self, topic: &str, partition: i32, node_id: &str, state: u8) {
        RAFT_NODE_STATE
            .with_label_values(&[topic, &partition.to_string(), node_id])
            .set(state as f64);
    }

    /// Set commit index
    pub fn set_commit_index(&self, topic: &str, partition: i32, index: u64) {
        RAFT_COMMIT_INDEX
            .with_label_values(&[topic, &partition.to_string()])
            .set(index as f64);
    }

    /// Set last applied index
    pub fn set_last_applied(&self, topic: &str, partition: i32, index: u64) {
        RAFT_LAST_APPLIED
            .with_label_values(&[topic, &partition.to_string()])
            .set(index as f64);
    }

    // ==================== Replication Lag Methods ====================

    /// Set follower lag in entries
    pub fn set_follower_lag_entries(&self, topic: &str, partition: i32, follower_id: &str, lag: u64) {
        RAFT_FOLLOWER_LAG_ENTRIES
            .with_label_values(&[topic, &partition.to_string(), follower_id])
            .set(lag as f64);
    }

    /// Set follower lag in milliseconds
    pub fn set_follower_lag_ms(&self, topic: &str, partition: i32, follower_id: &str, lag_ms: u64) {
        RAFT_FOLLOWER_LAG_MS
            .with_label_values(&[topic, &partition.to_string(), follower_id])
            .set(lag_ms as f64);
    }

    /// Set maximum follower lag
    pub fn set_max_follower_lag(&self, topic: &str, partition: i32, max_lag: u64) {
        RAFT_MAX_FOLLOWER_LAG
            .with_label_values(&[topic, &partition.to_string()])
            .set(max_lag as f64);
    }

    /// Update last heartbeat timestamp
    pub fn update_last_heartbeat(&self, topic: &str, partition: i32, follower_id: &str, timestamp_ms: u64) {
        RAFT_LAST_HEARTBEAT
            .with_label_values(&[topic, &partition.to_string(), follower_id])
            .set(timestamp_ms as f64);
    }

    // ==================== Election Methods ====================

    /// Record an election attempt
    pub fn record_election(&self, topic: &str, partition: i32, success: bool, latency_ms: f64) {
        let result = if success { "success" } else { "failure" };
        RAFT_ELECTION_COUNT
            .with_label_values(&[topic, &partition.to_string(), result])
            .inc();

        if success {
            RAFT_ELECTION_LATENCY
                .with_label_values(&[topic, &partition.to_string()])
                .observe(latency_ms);
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;

        RAFT_LAST_ELECTION_TIMESTAMP
            .with_label_values(&[topic, &partition.to_string()])
            .set(now_ms);
    }

    /// Set number of votes received
    pub fn set_votes_received(&self, topic: &str, partition: i32, votes: usize) {
        RAFT_VOTES_RECEIVED
            .with_label_values(&[topic, &partition.to_string()])
            .set(votes as f64);
    }

    // ==================== Commit Latency Methods ====================

    /// Record commit latency
    pub fn record_commit_latency(&self, topic: &str, partition: i32, latency_ms: f64, batch_size: usize) {
        RAFT_COMMIT_LATENCY
            .with_label_values(&[topic, &partition.to_string()])
            .observe(latency_ms);

        RAFT_COMMITS_TOTAL
            .with_label_values(&[topic, &partition.to_string()])
            .inc_by(batch_size as f64);

        if batch_size > 1 {
            RAFT_BATCH_COMMITS_TOTAL
                .with_label_values(&[topic, &partition.to_string()])
                .inc();

            RAFT_COMMIT_BATCH_SIZE
                .with_label_values(&[topic, &partition.to_string()])
                .observe(batch_size as f64);
        }
    }

    // ==================== Snapshot Methods ====================

    /// Record snapshot creation
    pub fn record_snapshot_create(&self, topic: &str, partition: i32, trigger: &str, size_bytes: u64, latency_ms: f64, last_index: u64) {
        RAFT_SNAPSHOT_COUNT
            .with_label_values(&[topic, &partition.to_string(), trigger])
            .inc();

        RAFT_SNAPSHOT_SIZE
            .with_label_values(&[topic, &partition.to_string()])
            .set(size_bytes as f64);

        RAFT_SNAPSHOT_CREATE_LATENCY
            .with_label_values(&[topic, &partition.to_string()])
            .observe(latency_ms);

        RAFT_SNAPSHOT_INDEX
            .with_label_values(&[topic, &partition.to_string()])
            .set(last_index as f64);
    }

    /// Record snapshot apply
    pub fn record_snapshot_apply(&self, topic: &str, partition: i32, latency_ms: f64) {
        RAFT_SNAPSHOT_APPLY_LATENCY
            .with_label_values(&[topic, &partition.to_string()])
            .observe(latency_ms);
    }

    /// Record snapshot transfer
    pub fn record_snapshot_transfer(&self, topic: &str, partition: i32, direction: &str, success: bool) {
        let result = if success { "success" } else { "failure" };
        RAFT_SNAPSHOT_TRANSFERS
            .with_label_values(&[topic, &partition.to_string(), direction, result])
            .inc();
    }

    // ==================== Network/RPC Methods ====================

    /// Record RPC call
    pub fn record_rpc_call(&self, rpc_type: &str, target_node: &str, success: bool, latency_ms: f64) {
        let result = if success { "success" } else { "failure" };

        RAFT_RPC_CALLS_TOTAL
            .with_label_values(&[rpc_type, target_node, result])
            .inc();

        if success {
            RAFT_RPC_LATENCY
                .with_label_values(&[rpc_type, target_node])
                .observe(latency_ms);
        }
    }

    /// Record RPC error
    pub fn record_rpc_error(&self, rpc_type: &str, target_node: &str, error_type: &str) {
        RAFT_RPC_ERRORS
            .with_label_values(&[rpc_type, target_node, error_type])
            .inc();
    }

    /// Record AppendEntries sent
    pub fn record_append_entries_sent(&self, topic: &str, partition: i32, target_node: &str, size_bytes: u64) {
        RAFT_APPEND_ENTRIES_SENT
            .with_label_values(&[topic, &partition.to_string(), target_node])
            .inc();

        RAFT_APPEND_ENTRIES_SIZE
            .with_label_values(&[topic, &partition.to_string()])
            .observe(size_bytes as f64);
    }

    /// Record AppendEntries received
    pub fn record_append_entries_received(&self, topic: &str, partition: i32, from_node: &str) {
        RAFT_APPEND_ENTRIES_RECEIVED
            .with_label_values(&[topic, &partition.to_string(), from_node])
            .inc();
    }

    /// Record AppendEntries rejection
    pub fn record_append_entries_rejected(&self, topic: &str, partition: i32, reason: &str) {
        RAFT_APPEND_ENTRIES_REJECTED
            .with_label_values(&[topic, &partition.to_string(), reason])
            .inc();
    }

    // ==================== Log Methods ====================

    /// Set Raft log size
    pub fn set_log_size(&self, topic: &str, partition: i32, entries: u64, bytes: u64) {
        RAFT_LOG_SIZE
            .with_label_values(&[topic, &partition.to_string()])
            .set(entries as f64);

        RAFT_LOG_SIZE_BYTES
            .with_label_values(&[topic, &partition.to_string()])
            .set(bytes as f64);
    }

    /// Record log compaction
    pub fn record_log_compaction(&self, topic: &str, partition: i32) {
        RAFT_LOG_COMPACTIONS
            .with_label_values(&[topic, &partition.to_string()])
            .inc();
    }

    // ==================== Quorum Methods ====================

    /// Set cluster configuration size
    pub fn set_cluster_size(&self, topic: &str, partition: i32, size: usize) {
        RAFT_CLUSTER_SIZE
            .with_label_values(&[topic, &partition.to_string()])
            .set(size as f64);

        let quorum = (size / 2) + 1;
        RAFT_QUORUM_SIZE
            .with_label_values(&[topic, &partition.to_string()])
            .set(quorum as f64);
    }

    /// Record configuration change
    pub fn record_config_change(&self, topic: &str, partition: i32, change_type: &str) {
        RAFT_CONFIG_CHANGES
            .with_label_values(&[topic, &partition.to_string(), change_type])
            .inc();
    }
}

impl Default for RaftMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raft_metrics_creation() {
        let metrics = RaftMetrics::new();

        // Test cluster health metrics
        metrics.set_leader_count("node1", 3);
        metrics.set_follower_count("node1", 6);
        metrics.set_isr_size("orders", 0, 3);

        // Test replication lag metrics
        metrics.set_follower_lag_entries("orders", 0, "node2", 15);
        metrics.set_follower_lag_ms("orders", 0, "node2", 45);

        // Test election metrics
        metrics.record_election("orders", 0, true, 1200.0);
        metrics.set_votes_received("orders", 0, 2);

        // Test commit metrics
        metrics.record_commit_latency("orders", 0, 8.0, 10);

        // Test snapshot metrics
        metrics.record_snapshot_create("orders", 0, "size", 67108864, 2500.0, 1000);
        metrics.record_snapshot_apply("orders", 0, 2500.0);

        // Test RPC metrics
        metrics.record_rpc_call("AppendEntries", "node2", true, 12.0);
        metrics.record_rpc_error("AppendEntries", "node2", "timeout");

        // Test log metrics
        metrics.set_log_size("orders", 0, 5000, 10485760);

        // Test quorum metrics
        metrics.set_cluster_size("orders", 0, 5);
    }
}
