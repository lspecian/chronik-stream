//! Metadata management module.

pub mod traits;
pub mod sled_store;
#[cfg(test)]
mod tests;

pub use traits::*;
pub use sled_store::SledMetadataStore;