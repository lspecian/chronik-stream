//! Bridge module for prost version compatibility
//!
//! This module re-exports functions from chronik-raft-bridge, which is a separate
//! crate that ONLY depends on raft (prost 0.11), avoiding the conflict with our
//! prost 0.13 dependency used for tonic/grpc.

use raft::prelude::{ConfChange, Message as RaftMessage};

/// Encode a raft::Message to protobuf bytes
pub fn encode_raft_message(msg: &RaftMessage) -> Result<Vec<u8>, String> {
    chronik_raft_bridge::encode_raft_message(msg)
}

/// Decode a raft::Message from protobuf bytes
pub fn decode_raft_message(bytes: &[u8]) -> Result<RaftMessage, String> {
    chronik_raft_bridge::decode_raft_message(bytes)
}

/// Decode a raft::ConfChange from protobuf bytes
pub fn decode_conf_change(bytes: &[u8]) -> Result<ConfChange, String> {
    chronik_raft_bridge::decode_conf_change(bytes)
}
