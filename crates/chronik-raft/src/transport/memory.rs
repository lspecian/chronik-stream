//! In-memory transport implementation for testing.

use crate::error::Result;
use crate::transport::Transport;
use async_trait::async_trait;
use raft::prelude::Message as RaftMessage;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::trace;

/// Shared message router for in-memory transport.
///
/// This allows multiple InMemoryTransport instances to exchange messages
/// without network overhead. All transports sharing the same router can
/// communicate with each other.
#[derive(Clone)]
pub struct InMemoryRouter {
    /// Map of node_id -> message queue
    /// Each entry is a list of (topic, partition, RaftMessage) tuples
    queues: Arc<RwLock<HashMap<u64, Vec<(String, i32, RaftMessage)>>>>,
}

impl InMemoryRouter {
    /// Create a new in-memory router.
    pub fn new() -> Self {
        Self {
            queues: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Send a message to a node's queue.
    async fn send(&self, to: u64, topic: String, partition: i32, msg: RaftMessage) {
        let mut queues = self.queues.write().await;
        queues.entry(to).or_insert_with(Vec::new).push((topic, partition, msg));
    }

    /// Retrieve all pending messages for a node.
    pub async fn receive(&self, node_id: u64) -> Vec<(String, i32, RaftMessage)> {
        let mut queues = self.queues.write().await;
        queues.entry(node_id).or_insert_with(Vec::new).drain(..).collect()
    }

    /// Check if there are pending messages for a node.
    pub async fn has_messages(&self, node_id: u64) -> bool {
        let queues = self.queues.read().await;
        queues.get(&node_id).map(|q| !q.is_empty()).unwrap_or(false)
    }

    /// Get the count of pending messages for a node.
    pub async fn message_count(&self, node_id: u64) -> usize {
        let queues = self.queues.read().await;
        queues.get(&node_id).map(|q| q.len()).unwrap_or(0)
    }
}

impl Default for InMemoryRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// In-memory transport for testing without network overhead.
///
/// Messages are routed through a shared InMemoryRouter instead of being
/// sent over the network. This allows fast, reliable testing of Raft logic
/// without gRPC infrastructure.
///
/// # Example
/// ```ignore
/// let router = InMemoryRouter::new();
///
/// let transport1 = InMemoryTransport::new(1, router.clone());
/// let transport2 = InMemoryTransport::new(2, router.clone());
///
/// // Messages sent by transport1 will be queued for transport2
/// transport1.send_message("topic", 0, 2, raft_msg).await?;
///
/// // Retrieve messages for node 2
/// let messages = router.receive(2).await;
/// ```
pub struct InMemoryTransport {
    /// This node's ID
    node_id: u64,

    /// Shared message router
    router: InMemoryRouter,

    /// Registered peer IDs (for validation)
    peers: Arc<RwLock<HashMap<u64, String>>>,
}

impl InMemoryTransport {
    /// Create a new in-memory transport for a specific node.
    ///
    /// # Arguments
    /// * `node_id` - This node's ID
    /// * `router` - Shared router for message exchange
    pub fn new(node_id: u64, router: InMemoryRouter) -> Self {
        Self {
            node_id,
            router,
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the shared router (useful for tests to poll messages).
    pub fn router(&self) -> &InMemoryRouter {
        &self.router
    }
}

#[async_trait]
impl Transport for InMemoryTransport {
    async fn send_message(
        &self,
        topic: &str,
        partition: i32,
        to: u64,
        msg: RaftMessage,
    ) -> Result<()> {
        trace!(
            from = self.node_id,
            to = to,
            topic = %topic,
            partition = partition,
            msg_type = ?msg.get_msg_type(),
            term = msg.term,
            "Routing Raft message via in-memory transport"
        );

        // Send message to destination node's queue
        self.router.send(to, topic.to_string(), partition, msg).await;

        Ok(())
    }

    async fn add_peer(&self, node_id: u64, addr: String) -> Result<()> {
        trace!(
            self_node = self.node_id,
            peer_node = node_id,
            addr = %addr,
            "Registering peer in in-memory transport"
        );

        self.peers.write().await.insert(node_id, addr);
        Ok(())
    }

    async fn remove_peer(&self, node_id: u64) {
        trace!(
            self_node = self.node_id,
            peer_node = node_id,
            "Removing peer from in-memory transport"
        );

        self.peers.write().await.remove(&node_id);
    }
}
