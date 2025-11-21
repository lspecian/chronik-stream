//! Metadata management module.
//!
//! v2.2.7: All metadata now flows through Raft (see chronik-server/raft_metadata_store.rs)
//! v2.2.9: Added WAL-based metadata store (Option A - Raft-free for performance)

pub mod traits;
pub mod memory;
pub mod events;
pub mod wal_metadata_store;
// v2.2.7 Phase 5: Deleted metalog_store.rs (old WAL-based metadata, replaced by Raft)
// Deleted metrics.rs (consolidated into chronik-monitoring::UnifiedMetrics)
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
pub use wal_metadata_store::{WalMetadataStore, WalAppendFn, EventBusPublishFn};
// v2.2.7 Phase 5: Removed ChronikMetaLogStore exports (deleted file)
// v2.2.9: Added WalMetadataStore (Option A - WAL-based metadata, Raft-free)
// v2.2.9 Phase 7: Added EventBusPublishFn for metadata replication
// Kept METADATA_TOPIC constant for backward compatibility
pub const METADATA_TOPIC: &str = "__chronik_metadata";
// Removed WalMetadataMetrics, MetricsReport, global_metrics exports (use chronik-monitoring::UnifiedMetrics instead)
pub use metadata_uploader::{MetadataUploader, MetadataUploaderConfig, ObjectStoreInterface, UploadStats};
pub use object_store_adapter::{ObjectStoreAdapter, ObjectStoreImpl};

// Raft metadata replication (only when raft feature is enabled)
#[cfg(feature = "raft")]
pub use raft_meta_log::{RaftMetaLog, RaftReplicaManager, METADATA_TOPIC as RAFT_METADATA_TOPIC, METADATA_PARTITION};
#[cfg(feature = "raft")]
pub use raft_state_machine::MetadataStateMachine;