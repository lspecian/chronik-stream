//! gRPC client for Raft peer communication.

use crate::error::{Result, RaftError};
use crate::rpc::proto;
use chronik_monitoring::RaftMetrics;
use raft::prelude::Message as RaftMessage_;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, warn};

/// Raft client for sending messages to peer nodes.
pub struct RaftClient {
    /// Map of node_id -> gRPC client
    clients: Arc<RwLock<HashMap<u64, proto::raft_service_client::RaftServiceClient<Channel>>>>,

    /// Map of node_id -> address
    peer_addrs: Arc<RwLock<HashMap<u64, String>>>,

    /// Raft metrics collector
    metrics: RaftMetrics,
}

impl RaftClient {
    /// Create a new Raft client.
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            peer_addrs: Arc::new(RwLock::new(HashMap::new())),
            metrics: RaftMetrics::new(),
        }
    }

    /// Add a peer node with its gRPC address.
    pub async fn add_peer(&self, node_id: u64, addr: String) -> Result<()> {
        info!("Adding peer {} at {}", node_id, addr);

        // Store the address
        self.peer_addrs.write().await.insert(node_id, addr.clone());

        // Connect to the peer
        self.connect(node_id, &addr).await?;

        Ok(())
    }

    /// Register a peer node's address without connecting.
    /// Connection will happen lazily on first message send.
    pub async fn register_peer_address(&self, node_id: u64, addr: String) {
        info!("Registering peer {} address: {}", node_id, addr);

        // Just store the address - connection happens lazily
        self.peer_addrs.write().await.insert(node_id, addr);
    }

    /// Remove a peer node.
    pub async fn remove_peer(&self, node_id: u64) {
        info!("Removing peer {}", node_id);

        self.clients.write().await.remove(&node_id);
        self.peer_addrs.write().await.remove(&node_id);
    }

    /// Send a Raft message to a peer node.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `to` - Target node ID
    /// * `msg` - Raft message to send
    pub async fn send_message(
        &self,
        topic: &str,
        partition: i32,
        to: u64,
        msg: RaftMessage_,
    ) -> Result<()> {
        let start = Instant::now();
        let msg_type_debug = format!("{:?}", msg.get_msg_type());

        debug!(
            topic = %topic,
            partition = partition,
            to = to,
            from = msg.from,
            msg_type = %msg_type_debug,
            term = msg.term,
            "Sending Raft message"
        );

        // Get or create client for the peer
        let mut client = self.get_or_connect(to).await?;

        // Serialize the raft::Message manually using protobuf wire format
        // CRITICAL FIX: raft::Message uses prost 0.11 internally, but we can't use the trait
        // from our prost 0.13 context. We use a bridge module that has access to prost 0.11.
        // The protobuf wire format is version-independent, so bytes are compatible.
        let msg_bytes = crate::prost_bridge::encode_raft_message(&msg)
            .map_err(|e| RaftError::Config(e))?;

        let size_bytes = msg_bytes.len() as u64;

        // Create the RaftMessage wrapper
        // Store bytes for potential retry
        let msg_bytes_for_retry = msg_bytes.clone();
        let raft_msg = proto::RaftMessage {
            topic: topic.to_string(),
            partition,
            message: msg_bytes,
        };

        // Determine RPC type for metrics (simplified)
        let rpc_type = if msg_type_debug.contains("AppendEntries") || msg_type_debug.contains("MsgAppend") {
            "AppendEntries"
        } else if msg_type_debug.contains("RequestVote") || msg_type_debug.contains("MsgRequestVote") {
            "RequestVote"
        } else if msg_type_debug.contains("Snapshot") {
            "InstallSnapshot"
        } else {
            "Other"
        };

        // Send via gRPC with automatic retry on transient failures
        let mut result = match client.step(raft_msg).await {
            Ok(response) => {
                let resp = response.into_inner();
                if !resp.success {
                    warn!(
                        topic = %topic,
                        partition = partition,
                        to = to,
                        msg_type = %msg_type_debug,
                        error = %resp.error,
                        "Raft RPC rejected by peer"
                    );
                    Err(RaftError::Grpc(tonic::Status::internal(resp.error)))
                } else {
                    debug!(
                        topic = %topic,
                        partition = partition,
                        to = to,
                        msg_type = %msg_type_debug,
                        "Raft RPC succeeded"
                    );
                    Ok(())
                }
            }
            Err(e) => {
                // CRITICAL FIX: Only remove client on connection errors, not on application errors
                // Check if this is a transient connection error that warrants retry
                let is_connection_error = matches!(
                    e.code(),
                    tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
                );

                if is_connection_error {
                    warn!(
                        topic = %topic,
                        partition = partition,
                        to = to,
                        msg_type = %msg_type_debug,
                        error = %e,
                        "Raft RPC connection error, will reconnect and retry"
                    );

                    // Remove stale client
                    self.clients.write().await.remove(&to);

                    // Try to reconnect and send once more (immediate retry)
                    if let Ok(mut new_client) = self.get_or_connect(to).await {
                        // Recreate RaftMessage with the saved bytes
                        let retry_raft_msg = proto::RaftMessage {
                            topic: topic.to_string(),
                            partition,
                            message: msg_bytes_for_retry.clone(),
                        };

                        match new_client.step(retry_raft_msg).await {
                            Ok(response) => {
                                let resp = response.into_inner();
                                if resp.success {
                                    debug!(topic = %topic, partition = partition, to = to, "Raft RPC succeeded after reconnect");
                                    Ok(())
                                } else {
                                    Err(RaftError::Grpc(tonic::Status::internal(resp.error)))
                                }
                            }
                            Err(retry_err) => {
                                warn!(topic = %topic, partition = partition, to = to, error = %retry_err, "Raft RPC failed after reconnect");
                                Err(RaftError::Grpc(retry_err))
                            }
                        }
                    } else {
                        Err(RaftError::Grpc(e))
                    }
                } else {
                    // Application error (not connection), don't remove client
                    warn!(
                        topic = %topic,
                        partition = partition,
                        to = to,
                        msg_type = %msg_type_debug,
                        error = %e,
                        "Raft RPC application error (keeping connection)"
                    );
                    Err(RaftError::Grpc(e))
                }
            }
        };

        // Record metrics
        let latency_ms = start.elapsed().as_millis() as f64;
        let target_node = to.to_string();
        let success = result.is_ok();

        self.metrics.record_rpc_call(rpc_type, &target_node, success, latency_ms);

        // Record AppendEntries-specific metrics
        if rpc_type == "AppendEntries" && success {
            self.metrics.record_append_entries_sent(topic, partition, &target_node, size_bytes);
        }

        // Record RPC errors
        if let Err(ref e) = result {
            let error_type = match e {
                RaftError::Grpc(status) if status.code() == tonic::Code::DeadlineExceeded => "timeout",
                RaftError::Grpc(status) if status.code() == tonic::Code::Unavailable => "connection_refused",
                RaftError::Grpc(_) => "network_error",
                _ => "unknown",
            };
            self.metrics.record_rpc_error(rpc_type, &target_node, error_type);
        }

        result
    }

    /// Send messages to multiple peers in parallel.
    pub async fn broadcast_messages(
        &self,
        topic: &str,
        partition: i32,
        messages: Vec<(u64, RaftMessage_)>,
    ) -> Vec<Result<()>> {
        let mut handles = Vec::new();

        for (to, msg) in messages {
            let topic = topic.to_string();
            let client = self.clone();
            let handle = tokio::spawn(async move {
                client.send_message(&topic, partition, to, msg).await
            });
            handles.push(handle);
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(Err(RaftError::Config(format!("Task failed: {}", e)))),
            }
        }

        results
    }

    /// Propose a metadata operation to a specific node (typically the leader).
    ///
    /// This is used for leader forwarding when a follower receives a metadata
    /// operation request.
    pub async fn propose_metadata_to_leader(
        &self,
        leader_id: u64,
        data: Vec<u8>,
    ) -> Result<u64> {
        debug!("Forwarding metadata proposal to leader {}", leader_id);

        let mut client = self.get_or_connect(leader_id).await?;

        let request = proto::ProposeMetadataRequest { data };

        let response = client
            .propose_metadata(request)
            .await
            .map_err(|e| RaftError::Config(format!("ProposeMetadata RPC failed: {}", e)))?
            .into_inner();

        if response.success {
            debug!("Metadata proposal succeeded at index {}", response.index);
            Ok(response.index)
        } else {
            Err(RaftError::Config(format!(
                "Leader rejected metadata proposal: {}",
                response.error
            )))
        }
    }

    /// Get an existing client or connect to the peer.
    async fn get_or_connect(&self, node_id: u64) -> Result<proto::raft_service_client::RaftServiceClient<Channel>> {
        // Check if we already have a client
        {
            let clients = self.clients.read().await;
            if let Some(client) = clients.get(&node_id) {
                return Ok(client.clone());
            }
        }

        // Need to connect - get the address
        let addr = {
            let addrs = self.peer_addrs.read().await;
            addrs
                .get(&node_id)
                .cloned()
                .ok_or_else(|| RaftError::Config(format!("No address for peer {}", node_id)))?
        };

        // Connect
        self.connect(node_id, &addr).await?;

        // Return the client
        let clients = self.clients.read().await;
        clients
            .get(&node_id)
            .cloned()
            .ok_or_else(|| RaftError::Config(format!("Failed to connect to peer {}", node_id)))
    }

    /// Connect to a peer and store the client.
    async fn connect(&self, node_id: u64, addr: &str) -> Result<()> {
        debug!("Connecting to peer {} at {}", node_id, addr);

        // addr is expected to already have the "http://" prefix from the caller
        let endpoint = Endpoint::from_shared(addr.to_string())
            .map_err(|e| RaftError::Config(format!("Invalid endpoint: {}", e)))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))  // Increased for Raft heartbeats
            // CRITICAL FIX: Add HTTP2 keepalive to prevent connection drops
            .http2_keep_alive_interval(std::time::Duration::from_secs(10))
            .keep_alive_timeout(std::time::Duration::from_secs(20))
            .keep_alive_while_idle(true)  // Send keepalive even when idle
            // Retry configuration for transient failures
            .tcp_nodelay(true)  // Disable Nagle's algorithm for low latency
            .tcp_keepalive(Some(std::time::Duration::from_secs(10)));

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| RaftError::Config(format!("Failed to connect to {}: {}", addr, e)))?;

        let client = proto::raft_service_client::RaftServiceClient::new(channel);

        self.clients.write().await.insert(node_id, client);

        info!("Connected to peer {} at {}", node_id, addr);

        Ok(())
    }

    /// Clone the client (shares the same connection pool).
    pub fn clone(&self) -> Self {
        Self {
            clients: self.clients.clone(),
            peer_addrs: self.peer_addrs.clone(),
            metrics: RaftMetrics::new(), // Create new metrics instance
        }
    }
}

impl Default for RaftClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_client_creation() {
        let client = RaftClient::new();
        assert_eq!(client.clients.read().await.len(), 0);
        assert_eq!(client.peer_addrs.read().await.len(), 0);
    }

    #[tokio::test]
    async fn test_add_peer() {
        let client = RaftClient::new();

        // Note: This will fail to connect, but should store the address
        let result = client.add_peer(1, "localhost:5001".to_string()).await;
        assert!(result.is_err()); // No server running

        // Address should be stored even if connection failed
        assert_eq!(client.peer_addrs.read().await.len(), 1);
    }

    #[tokio::test]
    async fn test_remove_peer() {
        let client = RaftClient::new();

        // Add and remove
        let _ = client.add_peer(1, "localhost:5001".to_string()).await;
        client.remove_peer(1).await;

        assert_eq!(client.peer_addrs.read().await.len(), 0);
    }
}
