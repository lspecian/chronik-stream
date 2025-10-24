//! gRPC service implementation for Raft consensus.

use crate::error::{Result, RaftError};
use crate::replica::PartitionReplica;
use raft::prelude::Message as RaftMessage; // Struct from raft (with prost-codec)
use std::sync::Arc;
use std::time::{Duration, Instant};
use tonic::{Request, Response, Status};
use tracing::{debug, error, warn};

// NOTE: We cannot import prost::Message trait because raft uses prost 0.11 and we use prost 0.13
// The trait implementations are incompatible even though the wire format is the same
// Solution: Use the prost_bridge module which has access to both prost versions

// Re-export generated protobuf types
pub mod proto {
    tonic::include_proto!("chronik.raft");
}

use proto::*;

// Re-export ReadIndex types for convenience
pub use proto::{ReadIndexRequest as ProtoReadIndexRequest, ReadIndexResponse as ProtoReadIndexResponse};

/// gRPC service implementation for Raft protocol.
///
/// This service handles AppendEntries, RequestVote, and InstallSnapshot RPCs
/// between Raft nodes in the cluster.
#[derive(Clone)]
pub struct RaftServiceImpl {
    /// Map of (topic, partition) -> PartitionReplica
    replicas: Arc<dashmap::DashMap<(String, i32), Arc<PartitionReplica>>>,
    /// Startup time for grace period handling
    startup_time: Arc<Instant>,
}

impl RaftServiceImpl {
    /// Create a new Raft service.
    pub fn new() -> Self {
        Self {
            replicas: Arc::new(dashmap::DashMap::new()),
            startup_time: Arc::new(Instant::now()),
        }
    }

    /// Check if we're within the startup grace period (first 10 seconds).
    /// During this period, "replica not found" errors are logged as debug instead of error.
    fn within_startup_grace_period(&self) -> bool {
        self.startup_time.elapsed() < Duration::from_secs(10)
    }

    /// Register a partition replica with the service.
    pub fn register_replica(&self, replica: Arc<PartitionReplica>) {
        let (topic, partition) = replica.info();
        self.replicas.insert((topic.to_string(), partition), replica);
    }

    /// Unregister a partition replica.
    pub fn unregister_replica(&self, topic: &str, partition: i32) {
        self.replicas.remove(&(topic.to_string(), partition));
    }

    /// Get a replica for the given topic and partition.
    fn get_replica(&self, topic: &str, partition: i32) -> Result<Arc<PartitionReplica>> {
        self.replicas
            .get(&(topic.to_string(), partition))
            .map(|r| r.value().clone())
            .ok_or_else(|| RaftError::Config(format!(
                "Replica not found for topic {} partition {}",
                topic, partition
            )))
    }
}

impl Default for RaftServiceImpl {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl raft_service_server::RaftService for RaftServiceImpl {
    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> std::result::Result<Response<AppendEntriesResponse>, Status> {
        let req = request.into_inner();
        
        debug!(
            "AppendEntries from leader {} term {} prev_log_index {} entries={}",
            req.leader_id,
            req.term,
            req.prev_log_index,
            req.entries.len()
        );

        // TODO: Extract topic/partition from request or context
        // For now, return a placeholder response
        let response = AppendEntriesResponse {
            term: req.term,
            success: false,
            conflict_index: 0,
            conflict_term: 0,
        };

        Ok(Response::new(response))
    }

    async fn request_vote(
        &self,
        request: Request<RequestVoteRequest>,
    ) -> std::result::Result<Response<RequestVoteResponse>, Status> {
        let req = request.into_inner();
        
        debug!(
            "RequestVote from candidate {} term {} last_log_index {} last_log_term {}",
            req.candidate_id,
            req.term,
            req.last_log_index,
            req.last_log_term
        );

        // TODO: Extract topic/partition from request or context
        let response = RequestVoteResponse {
            term: req.term,
            vote_granted: false,
        };

        Ok(Response::new(response))
    }

    async fn install_snapshot(
        &self,
        request: Request<tonic::Streaming<InstallSnapshotRequest>>,
    ) -> std::result::Result<Response<InstallSnapshotResponse>, Status> {
        let mut stream = request.into_inner();

        // Read snapshot chunks
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.message().await? {
            debug!(
                "InstallSnapshot from leader {} term {} offset {}",
                chunk.leader_id,
                chunk.term,
                chunk.offset
            );
            chunks.push(chunk);
        }

        // TODO: Assemble and apply snapshot
        let response = InstallSnapshotResponse {
            term: 0,
            success: false,
        };

        Ok(Response::new(response))
    }

    async fn read_index(
        &self,
        request: Request<ReadIndexRequest>,
    ) -> std::result::Result<Response<ReadIndexResponse>, Status> {
        let req = request.into_inner();

        debug!(
            "ReadIndex request read_id={} topic={} partition={}",
            req.read_id,
            req.topic,
            req.partition
        );

        // Get replica for the partition
        let replica = match self.get_replica(&req.topic, req.partition) {
            Ok(r) => r,
            Err(e) => {
                // During startup, replicas may not be registered yet - log as debug to reduce noise
                if self.within_startup_grace_period() {
                    debug!("ReadIndex: replica not yet registered (startup grace period): {}", e);
                } else {
                    error!("ReadIndex: replica not found: {}", e);
                }
                return Err(Status::not_found(format!(
                    "Replica not found for {}-{}",
                    req.topic, req.partition
                )));
            }
        };

        // Check if we're the leader
        let is_leader = replica.is_leader();
        let commit_index = replica.commit_index();

        let response = ReadIndexResponse {
            read_id: req.read_id,
            commit_index,
            is_leader,
        };

        Ok(Response::new(response))
    }

    async fn step(
        &self,
        request: Request<proto::RaftMessage>,
    ) -> std::result::Result<Response<StepResponse>, Status> {
        let req = request.into_inner();

        debug!(
            "Step message for {}-{}: {} bytes",
            req.topic,
            req.partition,
            req.message.len()
        );

        // Get replica for the partition
        let replica = match self.get_replica(&req.topic, req.partition) {
            Ok(r) => r,
            Err(e) => {
                // During startup, replicas may not be registered yet - log as debug to reduce noise
                if self.within_startup_grace_period() {
                    debug!("Step: replica not yet registered (startup grace period): {}", e);
                } else {
                    error!("Step: replica not found: {}", e);
                }
                return Ok(Response::new(StepResponse {
                    success: false,
                    error: format!("Replica not found: {}", e),
                }));
            }
        };

        // Deserialize raft::Message manually from protobuf wire format
        // CRITICAL FIX: raft::Message uses prost 0.11 internally, but we can't use the trait
        // from our prost 0.13 context. We use the prost_bridge module instead.
        // The protobuf wire format is version-independent, so bytes are compatible.
        let raft_msg = match crate::prost_bridge::decode_raft_message(&req.message) {
            Ok(msg) => msg,
            Err(e) => {
                error!("Step: failed to decode message: {}", e);
                return Ok(Response::new(StepResponse {
                    success: false,
                    error: format!("Failed to decode message: {}", e),
                }));
            }
        };

        // Step the message through Raft
        match replica.step(raft_msg).await {
            Ok(_) => Ok(Response::new(StepResponse {
                success: true,
                error: String::new(),
            })),
            Err(e) => {
                error!("Step: failed to process message: {}", e);
                Ok(Response::new(StepResponse {
                    success: false,
                    error: format!("Failed to step: {}", e),
                }))
            }
        }
    }

    /// Process multiple Raft messages in a single RPC call.
    ///
    /// OPTIMIZATION (v1.3.66): Batch message processing reduces HTTP/2 frame overhead
    /// and improves throughput for high-volume Raft traffic.
    async fn step_batch(
        &self,
        request: Request<proto::RaftMessageBatch>,
    ) -> std::result::Result<Response<StepBatchResponse>, Status> {
        let req = request.into_inner();

        debug!("StepBatch: processing {} messages", req.messages.len());

        let mut responses = Vec::with_capacity(req.messages.len());

        // Process each message sequentially (maintains order)
        for raft_msg in req.messages {
            // Get replica for the partition
            let replica = match self.get_replica(&raft_msg.topic, raft_msg.partition) {
                Ok(r) => r,
                Err(e) => {
                    // During startup, replicas may not be registered yet - log as debug to reduce noise
                    if self.within_startup_grace_period() {
                        debug!("StepBatch: replica not yet registered for {}-{} (startup grace period): {}", raft_msg.topic, raft_msg.partition, e);
                    } else {
                        error!("StepBatch: replica not found for {}-{}: {}", raft_msg.topic, raft_msg.partition, e);
                    }
                    responses.push(StepResponse {
                        success: false,
                        error: format!("Replica not found: {}", e),
                    });
                    continue;
                }
            };

            // Deserialize raft::Message
            let decoded_msg = match crate::prost_bridge::decode_raft_message(&raft_msg.message) {
                Ok(msg) => msg,
                Err(e) => {
                    error!("StepBatch: failed to decode message for {}-{}: {}", raft_msg.topic, raft_msg.partition, e);
                    responses.push(StepResponse {
                        success: false,
                        error: format!("Failed to decode message: {}", e),
                    });
                    continue;
                }
            };

            // Step the message through Raft
            match replica.step(decoded_msg).await {
                Ok(_) => responses.push(StepResponse {
                    success: true,
                    error: String::new(),
                }),
                Err(e) => {
                    error!("StepBatch: failed to process message for {}-{}: {}", raft_msg.topic, raft_msg.partition, e);
                    responses.push(StepResponse {
                        success: false,
                        error: format!("Failed to step: {}", e),
                    });
                }
            }
        }

        Ok(Response::new(StepBatchResponse { responses }))
    }

    async fn propose_metadata(
        &self,
        request: Request<proto::ProposeMetadataRequest>,
    ) -> std::result::Result<Response<proto::ProposeMetadataResponse>, Status> {
        let req = request.into_inner();

        debug!("ProposeMetadata request: {} bytes", req.data.len());

        // Get the __meta partition replica
        let replica = match self.get_replica("__meta", 0) {
            Ok(r) => r,
            Err(e) => {
                // During startup, replicas may not be registered yet - log as debug to reduce noise
                if self.within_startup_grace_period() {
                    debug!("ProposeMetadata: __meta replica not yet registered (startup grace period): {}", e);
                } else {
                    error!("ProposeMetadata: __meta replica not found: {}", e);
                }
                return Ok(Response::new(proto::ProposeMetadataResponse {
                    success: false,
                    error: format!("__meta replica not found: {}", e),
                    index: 0,
                }));
            }
        };

        // Propose the operation to Raft
        match replica.propose_and_wait(req.data).await {
            Ok(index) => {
                debug!("ProposeMetadata succeeded at index {}", index);
                Ok(Response::new(proto::ProposeMetadataResponse {
                    success: true,
                    error: String::new(),
                    index,
                }))
            }
            Err(e) => {
                error!("ProposeMetadata failed: {}", e);
                Ok(Response::new(proto::ProposeMetadataResponse {
                    success: false,
                    error: format!("Failed to propose: {}", e),
                    index: 0,
                }))
            }
        }
    }
}

/// Start a gRPC server for Raft communication.
pub async fn start_raft_server(
    listen_addr: String,
    service: RaftServiceImpl,
) -> Result<()> {
    use tonic::transport::Server;
    use std::time::Duration;

    let addr = listen_addr.parse()
        .map_err(|e| RaftError::Config(format!("Invalid listen address: {}", e)))?;

    // CRITICAL FIX: Configure server-side keepalive and timeouts to match client settings
    // This prevents connection drops and ensures long-lived connections for Raft consensus
    Server::builder()
        // HTTP2 keepalive - send pings every 10 seconds to keep connections alive
        .http2_keepalive_interval(Some(Duration::from_secs(10)))
        // Timeout for keepalive pings - must be > than interval
        .http2_keepalive_timeout(Some(Duration::from_secs(20)))
        // Allow keepalive pings even when there are no active streams
        .http2_adaptive_window(Some(true))
        // TCP keepalive at the socket level
        .tcp_keepalive(Some(Duration::from_secs(10)))
        // Disable Nagle's algorithm for low-latency Raft messages
        .tcp_nodelay(true)
        // Increase timeout for long-running Raft operations
        .timeout(Duration::from_secs(30))
        // Concurrency limits to prevent resource exhaustion
        .concurrency_limit_per_connection(256)
        .add_service(raft_service_server::RaftServiceServer::new(service))
        .serve(addr)
        .await
        .map_err(|e| RaftError::Grpc(Status::internal(e.to_string())))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_creation() {
        let service = RaftServiceImpl::new();
        assert_eq!(service.replicas.len(), 0);
    }
}
