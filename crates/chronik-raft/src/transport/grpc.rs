//! gRPC transport implementation for production use.

use crate::error::{Result, RaftError};
use crate::rpc::proto;
use crate::transport::Transport;
use async_trait::async_trait;
use chronik_monitoring::RaftMetrics;
use raft::prelude::Message as RaftMessage;
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

/// gRPC-based transport for production Raft clusters.
///
/// Features:
/// - Connection pooling with LRU eviction
/// - Automatic retry on transient failures
/// - Metrics collection
/// - Lazy connection establishment
pub struct GrpcTransport {
    /// Map of node_id -> pooled gRPC client with access tracking
    clients: Arc<RwLock<HashMap<u64, PooledClient>>>,

    /// Map of node_id -> address
    peer_addrs: Arc<RwLock<HashMap<u64, String>>>,

    /// Maximum number of concurrent connections (default: 1000)
    max_pool_size: usize,

    /// Raft metrics collector
    metrics: RaftMetrics,
}

impl GrpcTransport {
    /// Create a new gRPC transport with default connection pool size (1000).
    pub fn new() -> Self {
        Self::with_pool_size(1000)
    }

    /// Create a new gRPC transport with custom connection pool size.
    ///
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

    /// Get or create a gRPC client for a peer node.
    async fn get_or_connect(&self, node_id: u64) -> Result<proto::raft_service_client::RaftServiceClient<Channel>> {
        // Check if we already have a connection
        {
            let mut clients = self.clients.write().await;
            if let Some(pooled) = clients.get_mut(&node_id) {
                pooled.last_used = Instant::now();
                return Ok(pooled.client.clone());
            }
        }

        // Need to establish a new connection
        let addr = {
            let addrs = self.peer_addrs.read().await;
            addrs.get(&node_id)
                .ok_or_else(|| RaftError::Config(format!("Peer {} not registered", node_id)))?
                .clone()
        };

        self.connect(node_id, &addr).await
    }

    /// Establish a gRPC connection to a peer node.
    async fn connect(&self, node_id: u64, addr: &str) -> Result<proto::raft_service_client::RaftServiceClient<Channel>> {
        debug!("Connecting to peer {} at {}", node_id, addr);

        // Check pool size and evict LRU if needed
        {
            let mut clients = self.clients.write().await;
            if clients.len() >= self.max_pool_size {
                // Find LRU client and evict it
                if let Some((lru_id, _)) = clients
                    .iter()
                    .min_by_key(|(_, pooled)| pooled.last_used)
                    .map(|(id, pooled)| (*id, pooled.last_used))
                {
                    debug!("Evicting LRU connection to node {} (pool size limit: {})", lru_id, self.max_pool_size);
                    clients.remove(&lru_id);
                }
            }
        }

        // Create new connection
        let endpoint = Endpoint::from_shared(addr.to_string())
            .map_err(|e| RaftError::Config(format!("Invalid gRPC endpoint {}: {}", addr, e)))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10));

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| RaftError::Grpc(tonic::Status::unavailable(format!("Failed to connect to {}: {}", addr, e))))?;

        let client = proto::raft_service_client::RaftServiceClient::new(channel);

        // Store in pool
        {
            let mut clients = self.clients.write().await;
            clients.insert(node_id, PooledClient {
                client: client.clone(),
                last_used: Instant::now(),
            });
        }

        info!("Connected to peer {} at {}", node_id, addr);
        Ok(client)
    }
}

impl Default for GrpcTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for GrpcTransport {
    async fn send_message(
        &self,
        topic: &str,
        partition: i32,
        to: u64,
        msg: RaftMessage,
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
            "Sending Raft message via gRPC"
        );

        // Get or create client for the peer
        let mut client = self.get_or_connect(to).await?;

        // Serialize the raft::Message using protobuf wire format
        let msg_bytes = crate::prost_bridge::encode_raft_message(&msg)
            .map_err(|e| RaftError::Config(e))?;

        let size_bytes = msg_bytes.len() as u64;

        // Create the RaftMessage wrapper
        let msg_bytes_for_retry = msg_bytes.clone();
        let raft_msg = proto::RaftMessage {
            topic: topic.to_string(),
            partition,
            message: msg_bytes,
        };

        // Determine RPC type for metrics
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
                // Check if it's a transient failure
                if e.code() == tonic::Code::Unavailable || e.code() == tonic::Code::DeadlineExceeded {
                    warn!(
                        topic = %topic,
                        partition = partition,
                        to = to,
                        msg_type = %msg_type_debug,
                        error = %e,
                        "Raft RPC failed with transient error, retrying once"
                    );

                    // Remove stale connection
                    self.clients.write().await.remove(&to);

                    // Retry once with fresh connection
                    let mut fresh_client = self.get_or_connect(to).await?;
                    let retry_msg = proto::RaftMessage {
                        topic: topic.to_string(),
                        partition,
                        message: msg_bytes_for_retry,
                    };

                    match fresh_client.step(retry_msg).await {
                        Ok(response) => {
                            let resp = response.into_inner();
                            if !resp.success {
                                Err(RaftError::Grpc(tonic::Status::internal(resp.error)))
                            } else {
                                debug!(
                                    topic = %topic,
                                    partition = partition,
                                    to = to,
                                    msg_type = %msg_type_debug,
                                    "Raft RPC succeeded on retry"
                                );
                                Ok(())
                            }
                        }
                        Err(retry_err) => {
                            warn!(
                                topic = %topic,
                                partition = partition,
                                to = to,
                                msg_type = %msg_type_debug,
                                error = %retry_err,
                                "Raft RPC failed on retry"
                            );
                            Err(RaftError::Grpc(retry_err))
                        }
                    }
                } else {
                    warn!(
                        topic = %topic,
                        partition = partition,
                        to = to,
                        msg_type = %msg_type_debug,
                        error = %e,
                        "Raft RPC failed with non-transient error"
                    );
                    Err(RaftError::Grpc(e))
                }
            }
        };

        // Record metrics
        let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
        let success = result.is_ok();
        let target_node = to.to_string();

        self.metrics.record_rpc_call(rpc_type, &target_node, success, latency_ms);

        if !success {
            let error_type = match &result {
                Err(RaftError::Grpc(status)) => {
                    if status.code() == tonic::Code::Unavailable {
                        "unavailable"
                    } else if status.code() == tonic::Code::DeadlineExceeded {
                        "timeout"
                    } else {
                        "other_grpc"
                    }
                }
                Err(_) => "other",
                Ok(_) => unreachable!(),
            };
            self.metrics.record_rpc_error(rpc_type, &target_node, error_type);
        }

        result
    }

    async fn add_peer(&self, node_id: u64, addr: String) -> Result<()> {
        info!("Adding peer {} at {}", node_id, addr);

        // Store the address
        self.peer_addrs.write().await.insert(node_id, addr.clone());

        // Connect to the peer
        self.connect(node_id, &addr).await?;

        Ok(())
    }

    async fn remove_peer(&self, node_id: u64) {
        info!("Removing peer {}", node_id);

        self.clients.write().await.remove(&node_id);
        self.peer_addrs.write().await.remove(&node_id);
    }
}
