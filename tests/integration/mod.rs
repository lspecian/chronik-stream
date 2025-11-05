//! Integration tests for Chronik Stream.

mod test_setup;
mod common;
// mod basic_components_test;
mod canonical_crc_test;
mod raft_single_partition;
mod raft_multi_partition;
mod raft_cluster_bootstrap;
// mod raft_cluster_e2e;  // Temporarily disabled
mod raft_network_test;

// Multi-node Raft cluster integration tests
#[cfg(feature = "raft")]
mod raft_cluster_integration;

// Phase 3: Raft-based Produce Path test
#[cfg(feature = "raft")]
mod raft_produce_path_test;

// Single-node Raft debug test
mod raft_single_node_debug;

mod wal_raft_storage;

// Phase 2.2: Partition assignment persistence tests
mod partition_assignment_persistence;

// Raft snapshot support tests
#[cfg(feature = "raft")]
mod raft_snapshot_test;

// Priority 2: Admin API tests (v2.6.0)
#[cfg(feature = "raft")]
mod admin_api_test;

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