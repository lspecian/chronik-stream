//! gRPC client for Raft peer communication with connection pooling.

use crate::error::{Result, RaftError};
use crate::rpc::proto;
use chronik_monitoring::RaftMetrics;
use raft::prelude::Message as RaftMessage_;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, info, warn};

/// Connection pool entry with last access tracking for LRU eviction
struct PooledClient {
    client: proto::raft_service_client::RaftServiceClient<Channel>,
    last_used: Instant,
}

/// Raft client for sending messages to peer nodes with connection pooling.
///
/// OPTIMIZATION (v1.3.66): Implements bounded connection pool with LRU eviction
/// to prevent unbounded memory growth and improve cluster scalability.
pub struct RaftClient {
    /// Map of node_id -> pooled gRPC client with access tracking
    clients: Arc<RwLock<HashMap<u64, PooledClient>>>,

    /// Map of node_id -> address
    peer_addrs: Arc<RwLock<HashMap<u64, String>>>,

    /// Maximum number of concurrent connections (default: 1000)
    max_pool_size: usize,

    /// Raft metrics collector
    metrics: RaftMetrics,
}

impl RaftClient {
    /// Create a new Raft client with default connection pool size (1000).
    pub fn new() -> Self {
        Self::with_pool_size(1000)
    }

    /// Create a new Raft client with custom connection pool size.
    ///
    /// OPTIMIZATION (v1.3.66): Configurable pool size for different deployment scales.
    /// Recommended values:
    /// - Small clusters (3-10 nodes): 100-500
    /// - Medium clusters (10-100 nodes): 500-1000
    /// - Large clusters (100+ nodes): 1000-5000
    pub fn with_pool_size(max_pool_size: usize) -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            peer_addrs: Arc::new(RwLock::new(HashMap::new())),
            max_pool_size,
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

    /// Send multiple Raft messages to a single peer in one RPC call.
    ///
    /// OPTIMIZATION (v1.3.66): Batch sending reduces HTTP/2 frame overhead
    /// and improves throughput by sending multiple messages in a single gRPC call.
    ///
    /// # Arguments
    /// * `to` - Target node ID
    /// * `messages` - Vec of (topic, partition, RaftMessage) tuples
    pub async fn send_message_batch(
        &self,
        to: u64,
        messages: Vec<(&str, i32, RaftMessage_)>,
    ) -> Result<Vec<Result<()>>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }

        let start = Instant::now();
        debug!("Sending batch of {} messages to peer {}", messages.len(), to);

        // Get or create client for the peer
        let mut client = self.get_or_connect(to).await?;

        // Convert messages to protobuf format
        let mut proto_messages = Vec::with_capacity(messages.len());
        for (topic, partition, msg) in &messages {
            let msg_bytes = crate::prost_bridge::encode_raft_message(msg)
                .map_err(|e| RaftError::Config(e))?;

            proto_messages.push(proto::RaftMessage {
                topic: topic.to_string(),
                partition: *partition,
                message: msg_bytes,
            });
        }

        // Send batch via gRPC
        let batch_request = proto::RaftMessageBatch {
            messages: proto_messages,
        };

        let response = match client.step_batch(batch_request).await {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                // Connection error - remove client and retry
                if matches!(
                    e.code(),
                    tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
                ) {
                    warn!("Batch RPC connection error to peer {}, reconnecting...", to);
                    self.clients.write().await.remove(&to);

                    // Retry once
                    if let Ok(mut new_client) = self.get_or_connect(to).await {
                        // Recreate batch request
                        let retry_messages: Vec<proto::RaftMessage> = messages.iter()
                            .map(|(topic, partition, msg)| {
                                let msg_bytes = crate::prost_bridge::encode_raft_message(msg)
                                    .unwrap_or_default();
                                proto::RaftMessage {
                                    topic: topic.to_string(),
                                    partition: *partition,
                                    message: msg_bytes,
                                }
                            })
                            .collect();

                        let retry_batch = proto::RaftMessageBatch { messages: retry_messages };
                        match new_client.step_batch(retry_batch).await {
                            Ok(resp) => resp.into_inner(),
                            Err(retry_err) => {
                                warn!("Batch RPC failed after reconnect to peer {}: {}", to, retry_err);
                                return Err(RaftError::Grpc(retry_err));
                            }
                        }
                    } else {
                        return Err(RaftError::Grpc(e));
                    }
                } else {
                    return Err(RaftError::Grpc(e));
                }
            }
        };

        // Convert responses
        let mut results = Vec::with_capacity(response.responses.len());
        for step_resp in response.responses {
            if step_resp.success {
                results.push(Ok(()));
            } else {
                results.push(Err(RaftError::Config(step_resp.error)));
            }
        }

        // Record metrics
        let latency_ms = start.elapsed().as_millis() as f64;
        let target_node = to.to_string();
        let success_count = results.iter().filter(|r| r.is_ok()).count();
        let success_rate = success_count as f64 / results.len() as f64;

        // Record as a single "Batch" RPC with success rate
        self.metrics.record_rpc_call("Batch", &target_node, success_rate > 0.5, latency_ms);

        debug!(
            "Batch of {} messages to peer {} completed: {}/{} successful, latency: {:.2}ms",
            messages.len(), to, success_count, results.len(), latency_ms
        );

        Ok(results)
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
    ///
    /// OPTIMIZATION (v1.3.66): Updates last_used timestamp for LRU tracking.
    async fn get_or_connect(&self, node_id: u64) -> Result<proto::raft_service_client::RaftServiceClient<Channel>> {
        // Check if we already have a client and update its last_used timestamp
        {
            let mut clients = self.clients.write().await;
            if let Some(pooled) = clients.get_mut(&node_id) {
                pooled.last_used = Instant::now();
                return Ok(pooled.client.clone());
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

        // Check if we need to evict connections to stay within pool size limit
        self.evict_if_needed().await;

        // Connect
        self.connect(node_id, &addr).await?;

        // Return the client
        let clients = self.clients.read().await;
        clients
            .get(&node_id)
            .map(|pooled| pooled.client.clone())
            .ok_or_else(|| RaftError::Config(format!("Failed to connect to peer {}", node_id)))
    }

    /// Evict least recently used connections if pool is at capacity.
    ///
    /// OPTIMIZATION (v1.3.66): LRU eviction to prevent unbounded connection growth.
    async fn evict_if_needed(&self) {
        let mut clients = self.clients.write().await;

        if clients.len() >= self.max_pool_size {
            // Find the least recently used connection
            if let Some((&lru_node_id, _)) = clients.iter()
                .min_by_key(|(_, pooled)| pooled.last_used) {

                info!(
                    "Connection pool at capacity ({}/{}), evicting LRU connection to peer {}",
                    clients.len(),
                    self.max_pool_size,
                    lru_node_id
                );
                clients.remove(&lru_node_id);
            }
        }
    }

    /// Connect to a peer and store the client with pooling.
    ///
    /// OPTIMIZATION (v1.3.66): Wraps client in PooledClient with last_used tracking.
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

        let pooled = PooledClient {
            client,
            last_used: Instant::now(),
        };

        self.clients.write().await.insert(node_id, pooled);

        info!("Connected to peer {} at {} (pool: {}/{})",
              node_id, addr,
              self.clients.read().await.len(),
              self.max_pool_size);

        Ok(())
    }

    /// Clone the client (shares the same connection pool).
    ///
    /// OPTIMIZATION (v1.3.66): Preserves max_pool_size from original instance.
    pub fn clone(&self) -> Self {
        Self {
            clients: self.clients.clone(),
            peer_addrs: self.peer_addrs.clone(),
            max_pool_size: self.max_pool_size,
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
