//! Chronik Embeddings
//!
//! This crate provides embedding providers for vector search in Chronik Stream.
//! Embeddings convert text content into high-dimensional vectors for semantic search.
//!
//! ## Supported Providers
//!
//! - **OpenAI**: text-embedding-3-small, text-embedding-3-large, text-embedding-ada-002
//! - **External**: Custom HTTP endpoints for embedding services
//! - **Local** (optional): ONNX Runtime with SentenceTransformers (requires `local-models` feature)
//!
//! ## Topic Configuration
//!
//! Enable vector search for a topic:
//!
//! ```bash
//! kafka-topics.sh --create --topic logs \
//!   --config vector.enabled=true \
//!   --config vector.provider=openai \
//!   --config vector.model=text-embedding-3-small \
//!   --config vector.field=value
//! ```
//!
//! ## Architecture
//!
//! Vector processing happens in the background WalIndexer pipeline:
//!
//! ```text
//! Producer → WAL (fsync) → Response
//!                │
//!                ▼ (background, async)
//!           WalIndexer → Extract text → Embed → HNSW Index
//! ```

pub mod config;
pub mod provider;
pub mod openai;
pub mod external;
pub mod factory;

#[cfg(feature = "local-models")]
pub mod local;

// Re-exports
pub use config::{VectorSearchConfig, EmbeddingModelConfig, HnswConfig};
pub use provider::{EmbeddingProvider, EmbeddingBatch, EmbeddingResult};
pub use openai::OpenAIProvider;
pub use external::ExternalProvider;
pub use factory::{create_provider, create_provider_from_model_config};

#[cfg(feature = "local-models")]
pub use local::LocalProvider;
