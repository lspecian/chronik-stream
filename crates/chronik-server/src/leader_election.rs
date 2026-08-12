//! Superseded by [`crate::partition_failover`] (RP-5).
//!
//! This module used to claim to elect a new partition leader when a follower
//! noticed its leader had stopped sending. It did not. `elect_leader_from_isr`:
//!
//! - ignored ISR, despite the name — its own comment read *"For now, treat all
//!   replicas as in-sync (ISR = replicas). Proper ISR tracking will be added
//!   later"*;
//! - returned `replicas[0]`, which is the incumbent leader by construction, so
//!   the "new" leader was always the old one;
//! - never checked whether that node was alive;
//! - **never persisted the result** — the Raft proposal was commented out in
//!   favour of *"let the system self-heal via produce requests"*;
//! - and logged `✅ Elected new leader for {topic}-{partition}: node N`
//!   regardless.
//!
//! Measured on a 3-node cluster with the leader's node cordoned: the election
//! fired, logged that checkmark naming the node that was down, and leadership
//! never moved. Writes to that partition failed for as long as the node was
//! away. Reproduced identically under push and pull.
//!
//! Failover now lives in [`crate::partition_failover::PartitionFailoverController`],
//! which takes liveness from Raft heartbeats (the only source that can observe a
//! dead *leader*), picks a replica that is actually alive, and persists the
//! change through `assign_partition` so the leader epoch bumps and followers
//! reconcile.
//!
//! The type survives only because the push replication stack still wires it up.
//! RP-4 deletes both together.

use crate::raft_cluster::RaftCluster;
use anyhow::Result;
use chronik_common::metadata::MetadataStore;
use std::sync::Arc;
use tracing::debug;

/// Retained so the push stack's wiring still compiles. Does nothing.
pub struct LeaderElector {
    _raft_cluster: Arc<RaftCluster>,
    _metadata_store: Arc<dyn MetadataStore>,
}

impl LeaderElector {
    pub fn new(raft_cluster: Arc<RaftCluster>, metadata_store: Arc<dyn MetadataStore>) -> Self {
        Self {
            _raft_cluster: raft_cluster,
            _metadata_store: metadata_store,
        }
    }

    /// Always fails, deliberately.
    ///
    /// The push stack calls this on a replication-stream timeout and already
    /// logs failures at debug ("this is normal if not leader"), so returning an
    /// error here is silent. That is the point: a timeout on one replication
    /// stream is not evidence a node is down, and acting on it was how a dead
    /// leader got re-elected. `PartitionFailoverController` decides instead,
    /// from Raft's cluster-wide view.
    pub async fn trigger_election_on_timeout(
        &self,
        topic: &str,
        partition: i32,
        reason: &str,
    ) -> Result<u64> {
        debug!(
            "Ignoring election trigger for {}-{} ({}): partition failover is owned by \
             PartitionFailoverController (RP-5)",
            topic, partition, reason
        );
        anyhow::bail!("leader election superseded by PartitionFailoverController")
    }
}
