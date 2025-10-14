//! Metadata management module.

pub mod traits;
pub mod memory;
pub mod file_store;
pub mod events;
pub mod metalog_store;
pub mod metrics;
pub mod metadata_uploader;
pub mod object_store_adapter;
#[cfg(test)]
mod tests;

pub use traits::*;
pub use memory::InMemoryMetadataStore;
pub use file_store::FileMetadataStore;
pub use events::{MetadataEvent, MetadataEventPayload, EventLog, EventApplicator};
pub use metalog_store::{ChronikMetaLogStore, MetaLogWalInterface, MetadataState, METADATA_TOPIC, MockWal};
pub use metrics::{WalMetadataMetrics, MetricsReport, global_metrics};
pub use metadata_uploader::{MetadataUploader, MetadataUploaderConfig, ObjectStoreInterface, UploadStats};
pub use object_store_adapter::{ObjectStoreAdapter, ObjectStoreImpl};

// Re-export FileMetadataStore as the default
pub type DefaultMetadataStore = FileMetadataStore;