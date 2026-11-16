//! Give every partition a leader.
//!
//! A topic can exist with partitions the catalog assigns to nobody — Thunderbird
//! has topics whose partition 0 is assigned and partitions 1 and 2 are not.
//! Older brokers answered Metadata for such a partition with *themselves* as
//! leader, so each broker named a different one: producers wrote the partition
//! to whichever node they asked, nothing replicated it, and each node now holds
//! its partition of the topic as the only copy. Metadata now reports those
//! partitions as LEADER_NOT_AVAILABLE, which stops new writes going astray but
//! leaves the records already written unreachable — and a partition nobody
//! leads can never be written again.
//!
//! This pass repairs the catalog:
//!
//! - A node that holds records for an unassigned partition assigns it to
//!   itself, with the other nodes as followers, and replication copies the
//!   records to them. The node holding the data must be the leader: a follower
//!   whose log is ahead of its leader's is truncated to match, so naming any
//!   other node would destroy the only copy.
//! - A partition no node holds anything for is assigned by the Raft leader,
//!   once every node has had its turn to claim it.
//!
//! Claims are staggered by node id: node N claims only after the partition has
//! been seen unassigned on N passes. A claim is replicated in seconds and passes
//! are a minute apart, so a lower-id node's claim is visible before a higher-id
//! node's turn comes — two nodes holding divergent copies do not both win.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chronik_common::metadata::{MetadataStore, PartitionAssignment};
use dashmap::DashMap;
use tracing::{debug, info, warn};

/// How long after boot the first pass runs. The catalog anti-entropy pass
/// needs a moment to deliver assignments this node missed while down; acting
/// before it would "repair" partitions that are only unassigned here.
const FIRST_PASS_DELAY: Duration = Duration::from_secs(120);
const PASS_INTERVAL: Duration = Duration::from_secs(60);

/// What one node decides about one unassigned partition this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// Assign it: `replicas[0]` leads.
    Assign { replicas: Vec<u64>, reason: &'static str },
    /// Not this node's turn, or not its call.
    Wait,
}

/// Decide what this node does about an unassigned partition.
///
/// `nodes` is every node in the cluster; `local_end` is one past the highest
/// offset this node holds for the partition; `missing_passes` is how many
/// passes in a row (including this one) it has been seen unassigned.
pub fn plan_repair(
    node_id: u64,
    nodes: &[u64],
    replication_factor: usize,
    is_raft_leader: bool,
    partition: u32,
    local_end: i64,
    missing_passes: u32,
) -> RepairAction {
    let mut sorted: Vec<u64> = nodes.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        return RepairAction::Wait;
    }
    let rf = replication_factor.clamp(1, sorted.len());

    if local_end > 0 {
        if (missing_passes as u64) < node_id {
            return RepairAction::Wait;
        }
        // This node first, then the others in ring order after it.
        let start = sorted.iter().position(|&n| n == node_id).unwrap_or(0);
        let replicas: Vec<u64> = (0..sorted.len())
            .map(|i| sorted[(start + i) % sorted.len()])
            .take(rf)
            .collect();
        if replicas.first() != Some(&node_id) {
            // This node is not a cluster member; it cannot lead.
            return RepairAction::Wait;
        }
        return RepairAction::Assign { replicas, reason: "this node holds its records" };
    }

    // Nobody has claimed it after every node's turn: it is empty everywhere
    // that would have said otherwise. Place it like a new partition.
    let max_id = *sorted.last().unwrap_or(&0);
    if is_raft_leader && (missing_passes as u64) > max_id {
        let start = partition as usize % sorted.len();
        let replicas: Vec<u64> = (0..rf).map(|i| sorted[(start + i) % sorted.len()]).collect();
        return RepairAction::Assign { replicas, reason: "no node holds records for it" };
    }
    RepairAction::Wait
}

/// How much of a partition this node holds. `ProduceHandler` in production;
/// a map in tests.
#[async_trait::async_trait]
pub trait LocalLog: Send + Sync {
    /// One past the highest offset this node holds for the partition, from
    /// local evidence only (WAL, archive) — never the leader's watermark.
    async fn local_end(&self, topic: &str, partition: i32) -> i64;
}

#[async_trait::async_trait]
impl LocalLog for crate::produce_handler::ProduceHandler {
    /// The WAL's last record and the archive — what is on this node's disk.
    /// Not the in-memory partition state: that can be built from the metadata
    /// store's watermark, and a node claiming a partition on the strength of
    /// a number it was told would get the real holders truncated to nothing.
    async fn local_end(&self, topic: &str, partition: i32) -> i64 {
        self.local_log_end_on_disk(topic, partition).await
    }
}

pub struct AssignmentRepair {
    node_id: u64,
    nodes: Vec<u64>,
    replication_factor: usize,
    metadata_store: Arc<dyn MetadataStore>,
    local_log: Arc<dyn LocalLog>,
    raft_leader: Arc<AtomicBool>,
    missing: DashMap<(String, u32), u32>,
    shutdown: Arc<AtomicBool>,
    repaired: Arc<AtomicU64>,
}

impl AssignmentRepair {
    pub fn new(
        node_id: u64,
        nodes: Vec<u64>,
        replication_factor: usize,
        metadata_store: Arc<dyn MetadataStore>,
        local_log: Arc<dyn LocalLog>,
        raft_leader: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node_id,
            nodes,
            replication_factor,
            metadata_store,
            local_log,
            raft_leader,
            missing: DashMap::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            repaired: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(FIRST_PASS_DELAY).await;
            while !this.shutdown.load(Ordering::Relaxed) {
                if let Err(e) = this.repair_once().await {
                    warn!("Assignment repair pass failed: {}", e);
                }
                tokio::time::sleep(PASS_INTERVAL).await;
            }
        });
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn repaired(&self) -> u64 {
        self.repaired.load(Ordering::Relaxed)
    }

    pub(crate) async fn repair_once(&self) -> chronik_common::Result<()> {
        let is_raft_leader = self.raft_leader.load(Ordering::Relaxed);
        let mut still_missing: HashSet<(String, u32)> = HashSet::new();

        for topic in self.metadata_store.list_topics().await? {
            if topic.name.starts_with("__") {
                continue;
            }
            let assigned: HashSet<u32> = self
                .metadata_store
                .get_partition_assignments(&topic.name)
                .await?
                .into_iter()
                .map(|a| a.partition)
                .collect();

            for partition in 0..topic.config.partition_count {
                if assigned.contains(&partition) {
                    continue;
                }
                let key = (topic.name.clone(), partition);
                still_missing.insert(key.clone());
                let passes = {
                    let mut entry = self.missing.entry(key).or_insert(0);
                    *entry += 1;
                    *entry
                };

                let local_end = self.local_log.local_end(&topic.name, partition as i32).await;

                match plan_repair(
                    self.node_id,
                    &self.nodes,
                    self.replication_factor,
                    is_raft_leader,
                    partition,
                    local_end,
                    passes,
                ) {
                    RepairAction::Wait => {
                        debug!(
                            "{}-{} has no leader (seen {} pass(es)); not this node's turn",
                            topic.name, partition, passes
                        );
                    }
                    RepairAction::Assign { replicas, reason } => {
                        let leader = replicas[0];
                        warn!(
                            "{}-{} has no leader in the catalog; assigning it to node {} with replicas \
                             {:?} ({}; this node holds offsets up to {})",
                            topic.name, partition, leader, replicas, reason, local_end
                        );
                        self.metadata_store
                            .assign_partition(PartitionAssignment {
                                topic: topic.name.clone(),
                                partition,
                                broker_id: leader as i32,
                                is_leader: true,
                                replicas,
                                leader_id: leader,
                                leader_epoch: 0, // assigned by the metadata store
                                isr: vec![leader],
                            })
                            .await?;
                        self.repaired.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // Forget partitions that got a leader, so a later loss starts counting afresh.
        self.missing.retain(|key, _| still_missing.contains(key));
        if !still_missing.is_empty() {
            info!(
                "Assignment repair: {} partition(s) without a leader in the catalog",
                still_missing.len()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODES: [u64; 3] = [1, 2, 3];

    #[test]
    fn the_node_holding_the_records_leads_with_the_others_following() {
        let action = plan_repair(2, &NODES, 3, false, 1, 142, 2);
        assert_eq!(
            action,
            RepairAction::Assign { replicas: vec![2, 3, 1], reason: "this node holds its records" }
        );
    }

    #[test]
    fn claims_wait_for_the_node_s_turn() {
        // Node 3 holds records but must let nodes 1 and 2 claim first.
        assert_eq!(plan_repair(3, &NODES, 3, false, 2, 10, 1), RepairAction::Wait);
        assert_eq!(plan_repair(3, &NODES, 3, false, 2, 10, 2), RepairAction::Wait);
        assert!(matches!(plan_repair(3, &NODES, 3, false, 2, 10, 3), RepairAction::Assign { .. }));
        // Node 1's turn is the first pass.
        assert!(matches!(plan_repair(1, &NODES, 3, false, 0, 10, 1), RepairAction::Assign { .. }));
    }

    #[test]
    fn an_empty_partition_is_left_to_the_raft_leader_after_every_turn() {
        // A non-leader with nothing never assigns.
        assert_eq!(plan_repair(2, &NODES, 3, false, 1, 0, 50), RepairAction::Wait);
        // The Raft leader waits until every node had its turn (max id 3).
        assert_eq!(plan_repair(1, &NODES, 3, true, 1, 0, 3), RepairAction::Wait);
        assert_eq!(
            plan_repair(1, &NODES, 3, true, 1, 0, 4),
            RepairAction::Assign { replicas: vec![2, 3, 1], reason: "no node holds records for it" }
        );
    }

    #[test]
    fn replication_factor_is_capped_by_the_cluster_size() {
        assert_eq!(
            plan_repair(1, &[1, 2], 3, false, 0, 5, 1),
            RepairAction::Assign { replicas: vec![1, 2], reason: "this node holds its records" }
        );
        assert_eq!(
            plan_repair(3, &NODES, 1, false, 0, 5, 3),
            RepairAction::Assign { replicas: vec![3], reason: "this node holds its records" }
        );
    }

    #[test]
    fn a_node_outside_the_cluster_never_claims() {
        assert_eq!(plan_repair(9, &NODES, 3, false, 0, 5, 20), RepairAction::Wait);
    }

    struct Holds(std::collections::HashMap<(String, i32), i64>);

    #[async_trait::async_trait]
    impl LocalLog for Holds {
        async fn local_end(&self, topic: &str, partition: i32) -> i64 {
            self.0.get(&(topic.to_string(), partition)).copied().unwrap_or(0)
        }
    }

    /// Thunderbird's catalog: a three-partition topic with only partition 0
    /// assigned, partition 1's records on node 2 alone, partition 2 empty.
    async fn half_assigned_catalog() -> Arc<chronik_common::metadata::InMemoryMetadataStore> {
        use chronik_common::metadata::TopicConfig;
        let store = Arc::new(chronik_common::metadata::InMemoryMetadataStore::new());
        store
            .create_topic("t", TopicConfig { partition_count: 1, replication_factor: 3, ..Default::default() })
            .await
            .unwrap();
        store
            .update_topic("t", TopicConfig { partition_count: 3, replication_factor: 3, ..Default::default() })
            .await
            .unwrap();
        store
    }

    fn repair_on(
        node: u64,
        store: Arc<chronik_common::metadata::InMemoryMetadataStore>,
        holds: &[(&str, i32, i64)],
        raft_leader: bool,
    ) -> Arc<AssignmentRepair> {
        let holds = Holds(holds.iter().map(|(t, p, end)| ((t.to_string(), *p), *end)).collect());
        AssignmentRepair::new(
            node,
            NODES.to_vec(),
            3,
            store,
            Arc::new(holds),
            Arc::new(AtomicBool::new(raft_leader)),
        )
    }

    async fn leader_of(store: &chronik_common::metadata::InMemoryMetadataStore, p: u32) -> Option<(u64, Vec<u64>)> {
        store
            .get_partition_assignments("t")
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.partition == p)
            .map(|a| (a.leader_id, a.replicas))
    }

    #[tokio::test]
    async fn the_holder_claims_on_its_turn_and_the_raft_leader_places_the_empty_one() {
        let store = half_assigned_catalog().await;
        // Node 1 is the Raft leader and holds nothing; node 2 holds partition 1.
        let node1 = repair_on(1, store.clone(), &[], true);
        let node2 = repair_on(2, store.clone(), &[("t", 1, 142)], false);

        // Pass 1: not node 2's turn yet; node 1 has nothing to claim.
        node1.repair_once().await.unwrap();
        node2.repair_once().await.unwrap();
        assert_eq!(leader_of(&store, 1).await, None);

        // Pass 2: node 2 claims partition 1 and leads it.
        node1.repair_once().await.unwrap();
        node2.repair_once().await.unwrap();
        assert_eq!(leader_of(&store, 1).await, Some((2, vec![2, 3, 1])));
        assert_eq!(node2.repaired(), 1);
        assert_eq!(leader_of(&store, 2).await, None, "partition 2 waits for every node's turn");

        // Passes 3–4: once every node has had its turn, the Raft leader places
        // the partition nobody holds.
        node1.repair_once().await.unwrap();
        assert_eq!(leader_of(&store, 2).await, None);
        node1.repair_once().await.unwrap();
        assert_eq!(leader_of(&store, 2).await, Some((3, vec![3, 1, 2])));

        // Partition 0 was assigned all along and is untouched.
        assert_eq!(leader_of(&store, 0).await.map(|(_, r)| r.is_empty()), Some(false));
        node1.repair_once().await.unwrap();
        assert_eq!(node1.repaired(), 1, "nothing left to repair");
    }
}
