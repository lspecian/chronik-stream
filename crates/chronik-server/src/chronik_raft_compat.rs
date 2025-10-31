//! Compatibility shim for old chronik_raft references
//!
//! v2.2.0 removed the chronik-raft crate, but there are still references in
//! fetch_handler.rs, produce_handler.rs, etc. This module provides stub types
//! to keep the code compiling while we incrementally migrate to the new
//! raft_metadata module (v2.5.0 Phase 2).
//!
//! This is a TEMPORARY compatibility layer. Future phases will properly integrate
//! the new Raft metadata system.

#![allow(dead_code)]

use serde::{Serialize, Deserialize};

/// Stub ReadIndexRequest (referenced in fetch_handler.rs)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadIndexRequest {
    pub index: u64,
}

/// Stub ReadIndexResponse
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadIndexResponse {
    pub read_index: u64,
}

/// Stub RaftBatchProposer (referenced in produce_handler.rs)
pub struct RaftBatchProposer;

impl RaftBatchProposer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RaftBatchProposer {
    fn default() -> Self {
        Self::new()
    }
}
