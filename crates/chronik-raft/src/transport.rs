//! Transport abstraction for Raft message delivery.
//!
//! This module provides a pluggable transport layer for Raft messages,
//! allowing different implementations for production (gRPC) and testing (in-memory).

use crate::error::Result;
use async_trait::async_trait;
use raft::prelude::Message as RaftMessage;

/// Transport layer for sending Raft messages between nodes.
///
/// This trait abstracts the underlying transport mechanism, allowing:
/// - Production: gRPC over TCP/IP
/// - Testing: In-memory message routing without network overhead
/// - Future: QUIC, Unix sockets, shared memory, etc.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send a Raft message to a peer node.
    ///
    /// # Arguments
    /// * `topic` - Topic name for the partition
    /// * `partition` - Partition ID
    /// * `to` - Target node ID
    /// * `msg` - Raft message to send
    ///
    /// # Returns
    /// * `Ok(())` if the message was delivered successfully
    /// * `Err(RaftError)` if delivery failed
    async fn send_message(
        &self,
        topic: &str,
        partition: i32,
        to: u64,
        msg: RaftMessage,
    ) -> Result<()>;

    /// Register a peer node's address.
    ///
    /// For gRPC transport: stores the gRPC endpoint URL
    /// For in-memory transport: registers the peer ID for routing
    async fn add_peer(&self, node_id: u64, addr: String) -> Result<()>;

    /// Remove a peer node.
    async fn remove_peer(&self, node_id: u64);
}

// Re-export transport implementations
pub mod grpc;
pub mod memory;

pub use grpc::GrpcTransport;
pub use memory::{InMemoryTransport, InMemoryRouter};
