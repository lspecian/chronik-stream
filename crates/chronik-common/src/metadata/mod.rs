//! Metadata management module.
//!
//! v2.2.7: All metadata now flows through Raft (see chronik-server/raft_metadata_store.rs)

pub mod traits;
pub mod memory;
pub mod events;
// v2.2.7 Phase 5: Deleted metalog_store.rs (old WAL-based metadata, replaced by Raft)
pub mod metrics;
pub mod metadata_uploader;
pub mod object_store_adapter;

// Raft-replicated metadata store (requires raft feature)
#[cfg(feature = "raft")]
pub mod raft_meta_log;
#[cfg(feature = "raft")]
pub mod raft_state_machine;

pub use traits::*;
pub use memory::InMemoryMetadataStore;
pub use events::{MetadataEvent, MetadataEventPayload, EventLog, EventApplicator};
// v2.2.7 Phase 5: Removed ChronikMetaLogStore exports (deleted file)
// Kept METADATA_TOPIC constant for backward compatibility
pub const METADATA_TOPIC: &str = "__chronik_metadata";
pub use metrics::{WalMetadataMetrics, MetricsReport, global_metrics};
pub use metadata_uploader::{MetadataUploader, MetadataUploaderConfig, ObjectStoreInterface, UploadStats};
pub use object_store_adapter::{ObjectStoreAdapter, ObjectStoreImpl};

// Raft metadata replication (only when raft feature is enabled)
#[cfg(feature = "raft")]
pub use raft_meta_log::{RaftMetaLog, RaftReplicaManager, METADATA_TOPIC as RAFT_METADATA_TOPIC, METADATA_PARTITION};
#[cfg(feature = "raft")]
pub use raft_state_machine::MetadataStateMachine;