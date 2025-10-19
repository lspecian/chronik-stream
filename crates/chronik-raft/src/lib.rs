//! Chronik Raft - Distributed consensus for partition replication.
//!
//! This crate provides Raft-based replication for Chronik Stream partitions,
//! enabling high availability and fault tolerance across a cluster of nodes.
//!
//! # Architecture
//!
//! - **PartitionReplica**: Manages Raft consensus for a single partition
//! - **RaftLogStorage**: Persistent storage for Raft log entries
//! - **StateMachine**: Applies committed entries to partition state
//! - **RaftService**: gRPC service for inter-node communication
//!
//! # Example
//!
//! ```no_run
//! use chronik_raft::{
//!     config::RaftConfig,
//!     replica::PartitionReplica,
//!     storage::MemoryLogStorage,
//! };
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create configuration
//! let config = RaftConfig {
//!     node_id: 1,
//!     listen_addr: "0.0.0.0:5001".to_string(),
//!     ..Default::default()
//! };
//!
//! // Create storage and state machine
//! let log_storage = Arc::new(MemoryLogStorage::new());
//! // let state_machine = Arc::new(YourStateMachine::new());
//!
//! // Create partition replica
//! // let replica = PartitionReplica::new(
//! //     "my-topic".to_string(),
//! //     0,
//! //     config,
//! //     log_storage,
//! //     state_machine,
//! // )?;
//!
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod cluster_coordinator;
pub mod config;
pub mod error;
pub mod gossip;
pub mod graceful_shutdown;
pub mod group_manager;
pub mod isr;
pub mod lease;
pub mod membership;
pub mod multi_dc;
// pub mod metadata_state_machine;  // Moved to chronik-common
pub mod partition_assigner;
pub mod prost_bridge; // Bridge for prost 0.11/0.13 compatibility
pub mod raft_meta_log;
pub mod read_index;
pub mod rebalancer;
pub mod replica;
pub mod rpc;
pub mod snapshot;
pub mod snapshot_bootstrap;
pub mod state_machine;
pub mod storage;
// Note: wal_storage moved to tests/integration to avoid circular dependency with chronik-wal

pub use client::RaftClient;
// Re-export raft types needed by users
pub use raft::prelude::{ConfChange, ConfChangeType};
pub use cluster_coordinator::{ClusterConfig, ClusterCoordinator, PeerConfig, PeerHealth};
pub use config::RaftConfig;
pub use error::{RaftError, Result};
pub use gossip::{
    BootstrapDecision, BootstrapEvent, ClusterStatus, GossipBootstrap,
    HealthCheckBootstrap, HealthCheckConfig, NodeMetadata,
};
pub use graceful_shutdown::{GracefulShutdownManager, ShutdownConfig, ShutdownState};
pub use group_manager::{GroupHealth, HealthStatus, RaftGroupManager};
pub use isr::{IsrConfig, IsrManager, IsrSet, IsrStats};
pub use lease::{LeaseConfig, LeaseManager, PartitionKey};
pub use membership::{MembershipManager, NodeConfig};
pub use multi_dc::{
    DatacenterConfig, DatacenterInfo, DatacenterTopology, MultiDCManager, MultiDCPartitionAssignment,
    RaftConfig as MultiDCRaftConfig, ReplicaPlacement, ReplicationMode,
};
pub use partition_assigner::{AssignmentConfig, PartitionAssigner};
pub use raft_meta_log::{MetadataOp, MetadataState, MetadataStateMachine, RaftMetaLog};
pub use read_index::{ReadIndexManager, ReadIndexRequest, ReadIndexResponse};
pub use rebalancer::{
    ImbalanceReport, MigrationStatus, PartitionMove, PartitionRebalancer, RebalanceConfig,
    RebalancePlan,
};
pub use replica::PartitionReplica;
pub use snapshot::{SnapshotCompression, SnapshotConfig, SnapshotManager, SnapshotMetadata};
pub use snapshot_bootstrap::{BootstrapConfig, SnapshotBootstrap};
pub use state_machine::{MemoryStateMachine, SnapshotData, StateMachine};
pub use storage::{MemoryLogStorage, RaftEntry, RaftLogStorage, RAFT_PARTITION, RAFT_TOPIC};
