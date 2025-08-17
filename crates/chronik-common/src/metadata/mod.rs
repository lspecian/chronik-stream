//! Metadata management module.

pub mod traits;
pub mod tikv_store;
pub mod memory;
#[cfg(test)]
mod tests;

pub use traits::*;
pub use tikv_store::TiKVMetadataStore;
pub use memory::InMemoryMetadataStore;

// Re-export TiKVMetadataStore as the default MetadataStore implementation
pub type DefaultMetadataStore = TiKVMetadataStore;