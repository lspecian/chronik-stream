//! Metadata management module.

pub mod traits;
pub mod tikv_store;
#[cfg(test)]
mod tests;

pub use traits::*;
pub use tikv_store::TiKVMetadataStore;

// Re-export TiKVMetadataStore as the default MetadataStore implementation
pub type DefaultMetadataStore = TiKVMetadataStore;