//! Metadata management module.

pub mod traits;
pub mod memory;
pub mod file_store;
pub mod events;
pub mod metalog_store;
pub mod metrics;
#[cfg(test)]
mod tests;

pub use traits::*;
pub use memory::InMemoryMetadataStore;
pub use file_store::FileMetadataStore;
pub use events::{MetadataEvent, MetadataEventPayload, EventLog, EventApplicator};
pub use metalog_store::{ChronikMetaLogStore, MetaLogWalInterface, MetadataState, METADATA_TOPIC, MockWal};
pub use metrics::{WalMetadataMetrics, MetricsReport, global_metrics};

// Re-export FileMetadataStore as the default
pub type DefaultMetadataStore = FileMetadataStore;