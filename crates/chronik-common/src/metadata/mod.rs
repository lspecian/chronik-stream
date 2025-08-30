//! Metadata management module.

pub mod traits;
// pub mod tikv_store;  // Removed - tikv dependency removed
pub mod memory;
pub mod file_store;
#[cfg(test)]
mod tests;

pub use traits::*;
// pub use tikv_store::TiKVMetadataStore;  // Removed - tikv dependency removed
pub use memory::InMemoryMetadataStore;
pub use file_store::FileMetadataStore;

// Re-export FileMetadataStore as the default - simpler than TiKV!
pub type DefaultMetadataStore = FileMetadataStore;