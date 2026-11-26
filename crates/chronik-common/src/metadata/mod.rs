//! Metadata management module.
//!
//! v2.2.7: All metadata now flows through Raft (see chronik-server/raft_metadata_store.rs)
//! v2.2.9: Added WAL-based metadata store (Option A - Raft-free for performance)

pub mod catalog;
pub mod traits;
pub mod transaction;
pub mod memory;
pub mod events;
pub mod wal_metadata_store;
// v2.2.7 Phase 5: Deleted metalog_store.rs (old WAL-based metadata, replaced by Raft)
// Deleted metrics.rs (consolidated into chronik-monitoring::UnifiedMetrics)


pub use traits::*;
pub use transaction::{TransactionMetadata, TransactionState};
pub use memory::InMemoryMetadataStore;
pub use events::{MetadataEvent, MetadataEventPayload, EventLog, EventApplicator};
pub use wal_metadata_store::{WalMetadataStore, WalAppendFn, EventBusPublishFn, TopicDeletedFn};
// v2.2.7 Phase 5: Removed ChronikMetaLogStore exports (deleted file)
// v2.2.9: Added WalMetadataStore (Option A - WAL-based metadata, Raft-free)
// v2.2.9 Phase 7: Added EventBusPublishFn for metadata replication
// Kept METADATA_TOPIC constant for backward compatibility
pub const METADATA_TOPIC: &str = "__chronik_metadata";
// Removed WalMetadataMetrics, MetricsReport, global_metrics exports (use chronik-monitoring::UnifiedMetrics instead)
