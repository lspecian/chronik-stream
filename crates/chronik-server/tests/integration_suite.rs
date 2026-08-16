//! Wires `tests/integration/` into cargo so those files are compiled and run.
//!
//! They were not merely skipped by CI — they were unreachable by any command.
//! `tests/integration/` is not a workspace member, no crate references it, no
//! `[[test]]` target declares it, and the workspace root has no `[package]`, so
//! cargo never discovered them. They read as coverage on a release checklist and
//! provided none, for however long.
//!
//! Triaged 2026-08-16 by compiling each file alone: **3 of 27 still built.** The
//! other 24 had rotted against APIs that moved underneath them, which is what
//! happens to tests nothing can run.
//!
//! Files are included one at a time, never by glob, so a file that stops
//! compiling is commented out here with its reason rather than silently
//! disappearing again.
#[path = "../../../tests/integration/common.rs"]
mod common;

#[path = "../../../tests/integration/canonical_crc_test.rs"]
mod canonical_crc_test;

#[path = "../../../tests/integration/test_batch_round_trip.rs"]
mod test_batch_round_trip;

#[path = "../../../tests/integration/vector_search_test.rs"]
mod vector_search_test;

// Rotted, to be repaired or removed — error counts from the 2026-08-16 triage:
//
//   admin_api_test (2)      partition_assignment_persistence (2)
//   raft_cluster_bootstrap (2)  raft_multi_partition (2)
//   raft_single_partition (2)   real_kafka_clients_test (2)
//   raft_single_node_debug (3)  storage_test (4)
//   consumer_groups (5)         search_integration_test (5)
//   wal_lifecycle_test (6)      wal_recovery_test (6)
//   failure_recovery_test (7)   kafka_protocol (8)
//   multi_language_client_test (8)  cluster_broker_discovery_test (11)
//   kafka_compatibility_test (12)   performance_test (12)
//   wal_replication_test (17)   data_flow (26)
//   cluster (33)                search (43)
//
// end_to_end_test (17) and kafka_protocol_test (29) import `chronik_ingest` and
// `chronik_controller`, crates that no longer exist — they test deleted
// functionality and should go rather than be repaired.
