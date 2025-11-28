//! Integration tests for Chronik Stream.

mod test_setup;
mod common;
// mod basic_components_test;
mod canonical_crc_test;
// mod raft_single_partition;  // Compilation errors
// mod raft_multi_partition;  // Compilation errors
// mod raft_cluster_bootstrap;  // Compilation errors
// mod raft_cluster_e2e;  // Temporarily disabled
// mod raft_network_test;  // File not found

// Multi-node Raft cluster integration tests
#[cfg(feature = "raft")]
mod raft_cluster_integration;

// Phase 3: Raft-based Produce Path test
#[cfg(feature = "raft")]
mod raft_produce_path_test;

// Single-node Raft debug test
// mod raft_single_node_debug;  // Compilation errors

// mod wal_raft_storage;  // File not found

// Phase 2.3: WAL replication integration tests
// NOTE: Full cluster replication tests require ./tests/cluster/start.sh
// The refactored modules (connection_state, frame_reader, record_processor)
// are validated through the cluster scripts: tests/cluster/start.sh + test_node_ready.py
// mod wal_replication_test;  // Requires cluster infrastructure

// Phase 2.2: Partition assignment persistence tests
// mod partition_assignment_persistence;  // Compilation errors

// Raft snapshot support tests
#[cfg(feature = "raft")]
mod raft_snapshot_test;

// Priority 2: Admin API tests (v2.6.0)
#[cfg(feature = "raft")]
mod admin_api_test;

// v2.2.1: Cluster broker discovery test (critical bug fix)
#[cfg(feature = "raft")]
mod cluster_broker_discovery_test;

// Tests that require full cluster setup
// mod cluster;
// mod kafka_protocol;
// mod consumer_groups;
// mod data_flow;
// mod search;

// Additional test modules (to be implemented)
// mod testcontainers_setup;
// mod kafka_protocol_test;
// mod storage_test;
// mod end_to_end_test;
// mod kafka_compatibility_test;
// mod search_integration_test;
// mod failure_recovery_test;
// mod performance_test;
// mod multi_language_client_test;