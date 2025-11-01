//! gRPC service implementation for Raft consensus.
//!
//! Simplified version that works with RaftCluster's message receiver.

use tonic::{Request, Response, Status};
use tracing::{debug, error};

// Re-export generated protobuf types
pub mod proto {
    tonic::include_proto!("chronik.raft");
}

pub use proto::*;

/// Message handler callback type.
/// The callback receives the deserialized Raft message and processes it.
pub type MessageHandler = Arc<dyn Fn(raft::prelude::Message) -> Result<(), String> + Send + Sync>;

use std::sync::Arc;

/// gRPC service implementation for Raft protocol.
///
/// This service handles incoming Raft messages and forwards them to the message handler.
#[derive(Clone)]
pub struct RaftServiceImpl {
    /// Message handler that processes incoming Raft messages
    message_handler: MessageHandler,
}

impl RaftServiceImpl {
    /// Create a new Raft service with the given message handler.
    pub fn new(message_handler: MessageHandler) -> Self {
        Self { message_handler }
    }
}

#[tonic::async_trait]
impl raft_service_server::RaftService for RaftServiceImpl {
    /// Process a single Raft message (generic message passing).
    async fn step(
        &self,
        request: Request<RaftMessage>,
    ) -> Result<Response<StepResponse>, Status> {
        let msg = request.into_inner();

        debug!(
            topic = %msg.topic,
            partition = msg.partition,
            "Received Raft message via gRPC"
        );

        // Deserialize the raft::Message using prost_bridge
        let raft_msg = crate::prost_bridge::decode_raft_message(&msg.message)
            .map_err(|e| Status::invalid_argument(format!("Failed to decode Raft message: {}", e)))?;

        // Forward to message handler
        match (self.message_handler)(raft_msg) {
            Ok(()) => Ok(Response::new(StepResponse {
                success: true,
                error: String::new(),
            })),
            Err(e) => {
                error!(error = %e, "Failed to process Raft message");
                Ok(Response::new(StepResponse {
                    success: false,
                    error: e,
                }))
            }
        }
    }

    /// Process multiple Raft messages in a single RPC (batch optimization).
    async fn step_batch(
        &self,
        request: Request<RaftMessageBatch>,
    ) -> Result<Response<StepBatchResponse>, Status> {
        let batch = request.into_inner();
        let mut responses = Vec::with_capacity(batch.messages.len());

        for msg in batch.messages {
            // Deserialize
            let raft_msg = match crate::prost_bridge::decode_raft_message(&msg.message) {
                Ok(m) => m,
                Err(e) => {
                    responses.push(StepResponse {
                        success: false,
                        error: format!("Failed to decode: {}", e),
                    });
                    continue;
                }
            };

            // Process
            match (self.message_handler)(raft_msg) {
                Ok(()) => responses.push(StepResponse {
                    success: true,
                    error: String::new(),
                }),
                Err(e) => responses.push(StepResponse {
                    success: false,
                    error: e,
                }),
            }
        }

        Ok(Response::new(StepBatchResponse { responses }))
    }

    // Stub implementations for other RPCs (not needed yet)
    async fn append_entries(
        &self,
        _request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        Err(Status::unimplemented("Use Step RPC instead"))
    }

    async fn request_vote(
        &self,
        _request: Request<RequestVoteRequest>,
    ) -> Result<Response<RequestVoteResponse>, Status> {
        Err(Status::unimplemented("Use Step RPC instead"))
    }

    async fn install_snapshot(
        &self,
        _request: Request<tonic::Streaming<InstallSnapshotRequest>>,
    ) -> Result<Response<InstallSnapshotResponse>, Status> {
        Err(Status::unimplemented("Use Step RPC instead"))
    }

    async fn read_index(
        &self,
        _request: Request<ReadIndexRequest>,
    ) -> Result<Response<ReadIndexResponse>, Status> {
        Err(Status::unimplemented("Not implemented yet"))
    }

    async fn propose_metadata(
        &self,
        _request: Request<ProposeMetadataRequest>,
    ) -> Result<Response<ProposeMetadataResponse>, Status> {
        Err(Status::unimplemented("Not implemented yet"))
    }
}
