//! Storage backend implementations for different cloud providers and local storage.

pub mod s3;
pub mod gcs;
pub mod azure;
pub mod local;

pub use s3::S3Backend;
pub use gcs::GcsBackend;
pub use azure::AzureBackend;
pub use local::LocalBackend;