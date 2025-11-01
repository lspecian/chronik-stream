//! Bridge crate for prost 0.11 / 0.13 compatibility
//!
//! This crate ONLY depends on raft (which uses prost 0.11). It provides
//! encode/decode functions that work with raft's Message types.
//!
//! By isolating this in a separate crate with its own Cargo.toml, we ensure
//! that prost 0.11's Message trait is the only one in scope, avoiding the
//! trait conflict with prost 0.13 used by tonic.
//!
//! CRITICAL: We use prost::Message trait explicitly to avoid the protobuf v2
//! trait that raft-proto also includes. The prost implementation is what
//! raft uses when built with prost-codec feature.

use bytes::{Bytes, BytesMut};
// Explicitly use prost's Message trait, not protobuf's
use prost::Message as ProstMessage;
use raft::prelude::{ConfChange, Message as RaftMessage};

/// Encode a raft::Message to protobuf bytes using prost 0.11
pub fn encode_raft_message(msg: &RaftMessage) -> Result<Vec<u8>, String> {
    let mut buf = BytesMut::with_capacity(ProstMessage::encoded_len(msg));
    ProstMessage::encode(msg, &mut buf)
        .map_err(|e| format!("Failed to encode raft message: {}", e))?;
    Ok(buf.to_vec())
}

/// Decode a raft::Message from protobuf bytes using prost 0.11
pub fn decode_raft_message(bytes: &[u8]) -> Result<RaftMessage, String> {
    // Explicitly use prost::Message::decode, not protobuf's decode
    ProstMessage::decode(Bytes::from(bytes.to_vec()))
        .map_err(|e| format!("Failed to decode raft message: {}", e))
}

/// Decode a raft::ConfChange from protobuf bytes using prost 0.11
pub fn decode_conf_change(bytes: &[u8]) -> Result<ConfChange, String> {
    // Explicitly use prost::Message::decode, not protobuf's decode
    ProstMessage::decode(Bytes::from(bytes.to_vec()))
        .map_err(|e| format!("Failed to decode conf change: {}", e))
}
