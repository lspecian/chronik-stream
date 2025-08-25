//! Metadata management module.

pub mod traits;
pub mod tikv_store;
pub mod memory;
pub mod file_store;
#[cfg(test)]
mod tests;

pub use traits::*;
pub use tikv_store::TiKVMetadataStore;
pub use memory::InMemoryMetadataStore;
pub use file_store::FileMetadataStore;

// Re-export FileMetadataStore as the default - simpler than TiKV!
pub type DefaultMetadataStore = FileMetadataStore;