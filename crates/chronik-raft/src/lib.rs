//! Chronik Raft - gRPC transport for Raft consensus
//!
//! This crate provides production-ready gRPC transport for tikv/raft-rs,
//! enabling distributed consensus for Chronik Stream.
//!
//! ## Architecture
//!
//! - **GrpcTransport**: Production gRPC-based message transport
//! - **InMemoryTransport**: Testing transport without network overhead
//! - **prost_bridge**: Compatibility bridge for prost 0.11 (raft) and 0.13 (tonic)

pub mod config;
pub mod error;
pub mod prost_bridge;
pub mod rpc;
pub mod storage;
pub mod transport;

pub use config::RaftConfig;
pub use error::{RaftError, Result};
pub use storage::{RaftEntry, RaftLogStorage, MemoryLogStorage, RAFT_TOPIC, RAFT_PARTITION};
pub use transport::{Transport, GrpcTransport, InMemoryTransport, InMemoryRouter};
