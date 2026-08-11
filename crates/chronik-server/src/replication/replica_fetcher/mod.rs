//! Follower-pull replication (RP-2.4).
//!
//! A follower issues Fetch to its partition leader with `replica_id >= 0`, and
//! the offset it asks from *is* its position: progress, liveness and resume
//! point in a single value. That is the property the push stack had to
//! reconstruct from three separate mechanisms — an ACK channel, heartbeat
//! replies, and connection pruning — each of which needed a live cluster to
//! find its bug.
//!
//! See `docs/ROADMAP_REPLICATION.md` for the phase plan.

pub mod apply;
pub mod connection;
pub mod fetcher;
pub mod protocol;

pub use apply::{apply_fetched_records, plan_batches, ApplyRefusal, BatchAction, BatchFrame};
pub use connection::LeaderConnection;
pub use fetcher::{
    plan_assignments, FollowedPartition, ReplicaFetcher, ReplicaFetcherConfig, ReplicationMode,
};
