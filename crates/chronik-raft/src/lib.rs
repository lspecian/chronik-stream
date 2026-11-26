//! Chronik Raft: the Raft group that replicates the cluster catalog.
//!
//! - `driver`: the event-driven Raft driver (its own thread; proposals resolve
//!   with the applied result)
//! - `log_store`: the durable Raft log (segments, torn-tail cut)
//! - `net`: the `CatalogRaft` gRPC service (message batches, forwarded
//!   proposals)

pub mod driver;
pub mod log_store;
pub mod net;

pub use driver::{Applied, BootstrapSnapshot, DriverConfig, Outbox, ProposeError, RaftDriver, RaftHandle, RaftStatus, StateMachine};
pub use log_store::{LogStoreOptions, RaftLogStore};
