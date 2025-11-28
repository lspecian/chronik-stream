//! Cluster mode initialization and management
//!
//! This module contains all cluster-related initialization logic extracted from
//! `run_cluster_mode()` to reduce complexity from 288 to <25 per function.
//!
//! **Module Structure:**
//! - `config`: Configuration parsing and validation
//! - `raft_setup`: Raft cluster bootstrapping and gRPC server
//! - `server_setup`: IntegratedKafkaServer creation with builder pattern
//! - `listener_setup`: Kafka, Search, and Admin API listener startup
//! - `broker_registration`: Broker registration and partition assignment
//!
//! **Refactoring Goal:**
//! Extract 335-line monolithic `run_cluster_mode()` into focused, testable modules.

pub mod config;
pub mod raft_setup;
pub mod server_setup;
pub mod listener_setup;
pub mod broker_registration;

// Re-export key types for convenience
pub use config::ClusterInitConfig;
pub use raft_setup::{bootstrap_raft_cluster, start_raft_grpc_server};
pub use server_setup::create_integrated_server;
pub use listener_setup::{start_kafka_listener, start_search_api, start_admin_api, log_listener_status};
pub use broker_registration::register_brokers_and_assign_partitions;
