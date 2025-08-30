//! Ingest node implementation for Chronik Stream.

pub mod server;
pub mod indexer;
pub mod handler;
pub mod consumer_group;
pub mod produce_handler;
pub mod storage;
pub mod kafka_handler;
pub mod fetch_handler;
pub mod health;
pub mod metrics;
pub mod offset_storage;
// pub mod offset_cleanup;  // Removed - uses tikv_client directly
pub mod coordinator_manager;
// pub mod distributed_lock;  // Removed - uses tikv_client directly
pub mod heartbeat_monitor;
pub mod protocol_metadata;

pub use server::{IngestServer, ServerConfig, TlsConfig, ConnectionPoolConfig};
pub use indexer::{Indexer, IndexerConfig, IndexRecord, IndexerStats};
pub use handler::{RequestHandler, HandlerConfig};
pub use consumer_group::{GroupManager, ConsumerGroup, GroupState};
pub use produce_handler::{ProduceHandler, ProduceHandlerConfig};