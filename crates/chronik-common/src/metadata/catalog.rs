//! The cluster catalog as a deterministic state machine.
//!
//! Every change to cluster shape — topics, partition assignments, leader
//! epochs, ISR, brokers, ACLs, SCRAM credentials — is a [`CatalogCommand`].
//! Commands are ordered by the Raft log and applied by [`CatalogState::apply`]
//! on every node. Validation happens inside `apply`, against the state every
//! earlier command produced, so "create X if absent" is decided once, in order:
//! two concurrent creates of the same name both commit, and exactly one
//! succeeds.
//!
//! `apply` must be a pure function of (state, index, term, command). It reads
//! no clock, no environment and no randomness: timestamps travel in the
//! command, topic ids are derived from the entry's position, and replica
//! placement uses only the catalog's own state.
//!
//! This module has no Raft dependency; the server adapts it to the driver.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::traits::{
    AclBindingRecord, BrokerMetadata, BrokerStatus, PartitionAssignment, ScramCredentialRecord, TopicConfig,
    TopicMetadata,
};

/// Encoding version of commands and snapshots. Bumped on any incompatible
/// change; there is no upgrade path (clusters are recreated).
pub const CATALOG_FORMAT_VERSION: u8 = 1;

/// A config change for an existing topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigChange {
    /// AlterConfigs: the given entries replace every dynamic config.
    Replace(BTreeMap<String, String>),
    /// IncrementalAlterConfigs: set some keys, remove others.
    Incremental { set: BTreeMap<String, String>, delete: Vec<String> },
}

/// One partition's change in a [`CatalogCommand::SetAssignments`] batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentChange {
    pub topic: String,
    pub partition: u32,
    /// New replica list (preferred leader first). `None` keeps the current one.
    pub replicas: Option<Vec<u64>>,
    /// New leader. `None` keeps the current leader if it is still a replica,
    /// otherwise the first replica.
    pub leader: Option<u64>,
    /// New ISR. `None` keeps the current one restricted to the replicas.
    pub isr: Option<Vec<u64>>,
    /// Apply only if the partition's leader epoch is still this one. A
    /// decision made from an older view of the catalog is refused instead of
    /// undoing whatever changed since.
    pub expected_epoch: Option<i32>,
}

/// An ISR report from a partition leader, fenced by its leader epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsrUpdate {
    pub topic: String,
    pub partition: u32,
    pub leader_id: u64,
    pub leader_epoch: i32,
    pub isr: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CatalogCommand {
    CreateTopic {
        name: String,
        config: TopicConfig,
        auto_created: bool,
        /// Explicit replica lists, one per partition. `None` places them.
        replicas: Option<Vec<Vec<u64>>>,
        timestamp_ms: i64,
    },
    DeleteTopic { name: String },
    AlterTopicConfig { name: String, change: ConfigChange, timestamp_ms: i64 },
    CreatePartitions {
        name: String,
        new_count: u32,
        replicas: Option<Vec<Vec<u64>>>,
        timestamp_ms: i64,
    },
    /// Placement and leadership changes: failover, preferred-leader return,
    /// rebalancing, admin reassignment, repair of unassigned partitions.
    SetAssignments { changes: Vec<AssignmentChange> },
    /// ISR reports, batched.
    UpdateIsr { updates: Vec<IsrUpdate> },
    RegisterBroker { broker: BrokerMetadata },
    SetBrokerStatus { broker_id: i32, status: BrokerStatus, timestamp_ms: i64 },
    UnregisterBroker { broker_id: i32 },
    CreateAcl { binding: AclBindingRecord },
    DeleteAcl { binding: AclBindingRecord },
    UpsertScram { credential: ScramCredentialRecord },
    DeleteScram { username: String, mechanism: i8 },
}

impl CatalogCommand {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![CATALOG_FORMAT_VERSION];
        out.extend(bincode::serialize(self).expect("catalog command serializes"));
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes.split_first() {
            Some((&CATALOG_FORMAT_VERSION, rest)) => bincode::deserialize(rest).map_err(|e| e.to_string()),
            Some((v, _)) => Err(format!("catalog command format {} is not supported (expected {})", v, CATALOG_FORMAT_VERSION)),
            None => Err("empty catalog command".into()),
        }
    }
}

/// Why a command (or one item of a batch) was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatalogError {
    TopicAlreadyExists,
    UnknownTopic,
    UnknownPartition,
    InvalidPartitions(String),
    InvalidReplicaAssignment(String),
    InvalidConfig(String),
    /// No broker is registered, so nothing can be placed.
    NoBrokers,
    /// The partition's leader epoch moved on since the request was made.
    StaleEpoch { current: i32 },
    /// The reporter is not the partition's current leader.
    NotLeader { current: u64 },
    NotFound,
    /// The command could not be decoded.
    Malformed(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::TopicAlreadyExists => write!(f, "topic already exists"),
            CatalogError::UnknownTopic => write!(f, "unknown topic"),
            CatalogError::UnknownPartition => write!(f, "unknown partition"),
            CatalogError::InvalidPartitions(m) => write!(f, "invalid partitions: {}", m),
            CatalogError::InvalidReplicaAssignment(m) => write!(f, "invalid replica assignment: {}", m),
            CatalogError::InvalidConfig(m) => write!(f, "invalid config: {}", m),
            CatalogError::NoBrokers => write!(f, "no brokers registered"),
            CatalogError::StaleEpoch { current } => write!(f, "stale leader epoch (current {})", current),
            CatalogError::NotLeader { current } => write!(f, "not the partition leader (current {})", current),
            CatalogError::NotFound => write!(f, "not found"),
            CatalogError::Malformed(m) => write!(f, "malformed command: {}", m),
        }
    }
}

impl std::error::Error for CatalogError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CatalogResult {
    Ok,
    Topic { topic: TopicMetadata, assignments: Vec<PartitionAssignment> },
    /// One outcome per item of a batch, in order.
    Items(Vec<Result<(), CatalogError>>),
    Err(CatalogError),
}

impl CatalogResult {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("catalog result serializes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        bincode::deserialize(bytes).map_err(|e| e.to_string())
    }
}

/// What an applied command changed, for local listeners (the replica
/// fetcher re-plans, deletion cleanup purges data, security caches reload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogChange {
    TopicCreated(String),
    TopicDeleted(String),
    TopicConfigChanged(String),
    PartitionsAdded { topic: String, from: u32, to: u32 },
    AssignmentChanged { topic: String, partition: u32 },
    BrokersChanged,
    AclsChanged,
    ScramChanged,
}

/// The applied catalog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogState {
    topics: BTreeMap<String, TopicMetadata>,
    assignments: BTreeMap<(String, u32), PartitionAssignment>,
    brokers: BTreeMap<i32, BrokerMetadata>,
    acls: BTreeSet<AclKey>,
    scram: BTreeMap<(String, i8), ScramCredentialRecord>,
    /// Rotates the first replica of newly placed topics across brokers, so
    /// single-partition topics do not all lead on the lowest broker id.
    placement_cursor: u64,
    /// Index of the last applied command.
    applied_index: u64,
}

/// `AclBindingRecord` ordered, for a deterministic set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct AclKey(i8, String, i8, String, String, i8, i8);

impl From<&AclBindingRecord> for AclKey {
    fn from(b: &AclBindingRecord) -> Self {
        AclKey(
            b.resource_type,
            b.resource_name.clone(),
            b.pattern_type,
            b.principal.clone(),
            b.host.clone(),
            b.operation,
            b.permission_type,
        )
    }
}

impl From<&AclKey> for AclBindingRecord {
    fn from(k: &AclKey) -> Self {
        AclBindingRecord {
            resource_type: k.0,
            resource_name: k.1.clone(),
            pattern_type: k.2,
            principal: k.3.clone(),
            host: k.4.clone(),
            operation: k.5,
            permission_type: k.6,
        }
    }
}

fn timestamp(ms: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

/// A topic id derived from the entry that created it: identical on every
/// node, unique per creation, different for a later topic with the same name.
fn topic_id(term: u64, index: u64) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&term.to_be_bytes());
    bytes[8..].copy_from_slice(&index.to_be_bytes());
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

impl CatalogState {
    // ---- reads ---------------------------------------------------------------

    pub fn applied_index(&self) -> u64 {
        self.applied_index
    }

    pub fn topic(&self, name: &str) -> Option<&TopicMetadata> {
        self.topics.get(name)
    }

    pub fn topics(&self) -> impl Iterator<Item = &TopicMetadata> {
        self.topics.values()
    }

    pub fn assignment(&self, topic: &str, partition: u32) -> Option<&PartitionAssignment> {
        self.assignments.get(&(topic.to_string(), partition))
    }

    pub fn assignments_for(&self, topic: &str) -> Vec<PartitionAssignment> {
        self.assignments
            .range((topic.to_string(), 0)..=(topic.to_string(), u32::MAX))
            .map(|(_, a)| a.clone())
            .collect()
    }

    pub fn all_assignments(&self) -> impl Iterator<Item = &PartitionAssignment> {
        self.assignments.values()
    }

    pub fn brokers(&self) -> impl Iterator<Item = &BrokerMetadata> {
        self.brokers.values()
    }

    pub fn broker(&self, id: i32) -> Option<&BrokerMetadata> {
        self.brokers.get(&id)
    }

    pub fn acls(&self) -> Vec<AclBindingRecord> {
        self.acls.iter().map(AclBindingRecord::from).collect()
    }

    pub fn scram_credentials(&self) -> Vec<ScramCredentialRecord> {
        self.scram.values().cloned().collect()
    }

    // ---- snapshots -----------------------------------------------------------

    pub fn to_snapshot(&self) -> Vec<u8> {
        let mut out = vec![CATALOG_FORMAT_VERSION];
        out.extend(bincode::serialize(self).expect("catalog state serializes"));
        out
    }

    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, String> {
        match bytes.split_first() {
            Some((&CATALOG_FORMAT_VERSION, rest)) => bincode::deserialize(rest).map_err(|e| e.to_string()),
            Some((v, _)) => Err(format!("catalog snapshot format {} is not supported", v)),
            None => Err("empty catalog snapshot".into()),
        }
    }

    // ---- apply ---------------------------------------------------------------

    /// Apply one committed entry. Never fails: an undecodable or invalid
    /// command yields an error result and leaves the state unchanged (beyond
    /// the applied index), identically on every node.
    pub fn apply_entry(&mut self, index: u64, term: u64, bytes: &[u8]) -> (CatalogResult, Vec<CatalogChange>) {
        let out = match CatalogCommand::decode(bytes) {
            Ok(cmd) => self.apply(index, term, cmd),
            Err(e) => (CatalogResult::Err(CatalogError::Malformed(e)), Vec::new()),
        };
        self.applied_index = index;
        out
    }

    pub fn apply(&mut self, index: u64, term: u64, cmd: CatalogCommand) -> (CatalogResult, Vec<CatalogChange>) {
        let mut changes = Vec::new();
        let result = match cmd {
            CatalogCommand::CreateTopic { name, config, auto_created, replicas, timestamp_ms } => {
                self.create_topic(index, term, name, config, auto_created, replicas, timestamp_ms, &mut changes)
            }
            CatalogCommand::DeleteTopic { name } => self.delete_topic(&name, &mut changes),
            CatalogCommand::AlterTopicConfig { name, change, timestamp_ms } => {
                self.alter_config(&name, change, timestamp_ms, &mut changes)
            }
            CatalogCommand::CreatePartitions { name, new_count, replicas, timestamp_ms } => {
                self.create_partitions(&name, new_count, replicas, timestamp_ms, &mut changes)
            }
            CatalogCommand::SetAssignments { changes: items } => {
                CatalogResult::Items(items.into_iter().map(|c| self.set_assignment(c, &mut changes)).collect())
            }
            CatalogCommand::UpdateIsr { updates } => {
                CatalogResult::Items(updates.into_iter().map(|u| self.update_isr(u, &mut changes)).collect())
            }
            CatalogCommand::RegisterBroker { mut broker } => {
                if let Some(existing) = self.brokers.get(&broker.broker_id) {
                    broker.created_at = existing.created_at;
                }
                if self.brokers.get(&broker.broker_id) != Some(&broker) {
                    self.brokers.insert(broker.broker_id, broker);
                    changes.push(CatalogChange::BrokersChanged);
                }
                CatalogResult::Ok
            }
            CatalogCommand::SetBrokerStatus { broker_id, status, timestamp_ms } => match self.brokers.get_mut(&broker_id) {
                Some(b) => {
                    if b.status != status {
                        b.status = status;
                        b.updated_at = timestamp(timestamp_ms);
                        changes.push(CatalogChange::BrokersChanged);
                    }
                    CatalogResult::Ok
                }
                None => CatalogResult::Err(CatalogError::NotFound),
            },
            CatalogCommand::UnregisterBroker { broker_id } => {
                if self.brokers.remove(&broker_id).is_some() {
                    changes.push(CatalogChange::BrokersChanged);
                    CatalogResult::Ok
                } else {
                    CatalogResult::Err(CatalogError::NotFound)
                }
            }
            CatalogCommand::CreateAcl { binding } => {
                if self.acls.insert(AclKey::from(&binding)) {
                    changes.push(CatalogChange::AclsChanged);
                }
                CatalogResult::Ok
            }
            CatalogCommand::DeleteAcl { binding } => {
                if self.acls.remove(&AclKey::from(&binding)) {
                    changes.push(CatalogChange::AclsChanged);
                    CatalogResult::Ok
                } else {
                    CatalogResult::Err(CatalogError::NotFound)
                }
            }
            CatalogCommand::UpsertScram { credential } => {
                self.scram.insert((credential.username.clone(), credential.mechanism), credential);
                changes.push(CatalogChange::ScramChanged);
                CatalogResult::Ok
            }
            CatalogCommand::DeleteScram { username, mechanism } => {
                if self.scram.remove(&(username, mechanism)).is_some() {
                    changes.push(CatalogChange::ScramChanged);
                    CatalogResult::Ok
                } else {
                    CatalogResult::Err(CatalogError::NotFound)
                }
            }
        };
        self.applied_index = self.applied_index.max(index);
        (result, changes)
    }

    // ---- topics --------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn create_topic(
        &mut self,
        index: u64,
        term: u64,
        name: String,
        config: TopicConfig,
        auto_created: bool,
        replicas: Option<Vec<Vec<u64>>>,
        timestamp_ms: i64,
        changes: &mut Vec<CatalogChange>,
    ) -> CatalogResult {
        if self.topics.contains_key(&name) {
            return CatalogResult::Err(CatalogError::TopicAlreadyExists);
        }
        if name.is_empty() {
            return CatalogResult::Err(CatalogError::InvalidConfig("empty topic name".into()));
        }
        if config.partition_count == 0 {
            return CatalogResult::Err(CatalogError::InvalidPartitions("a topic needs at least one partition".into()));
        }
        let placement = match self.place(&name, 0, config.partition_count, config.replication_factor, replicas) {
            Ok(p) => p,
            Err(e) => return CatalogResult::Err(e),
        };
        let now = timestamp(timestamp_ms);
        let topic = TopicMetadata {
            id: topic_id(term, index),
            name: name.clone(),
            config,
            created_at: now,
            updated_at: now,
            auto_created,
        };
        let mut created = Vec::with_capacity(placement.len());
        for (partition, reps) in placement.into_iter().enumerate() {
            let a = new_assignment(&name, partition as u32, reps);
            self.assignments.insert((name.clone(), partition as u32), a.clone());
            created.push(a);
        }
        self.topics.insert(name.clone(), topic.clone());
        changes.push(CatalogChange::TopicCreated(name));
        CatalogResult::Topic { topic, assignments: created }
    }

    fn delete_topic(&mut self, name: &str, changes: &mut Vec<CatalogChange>) -> CatalogResult {
        let Some(topic) = self.topics.remove(name) else {
            return CatalogResult::Err(CatalogError::UnknownTopic);
        };
        for p in 0..topic.config.partition_count {
            self.assignments.remove(&(name.to_string(), p));
        }
        changes.push(CatalogChange::TopicDeleted(name.to_string()));
        CatalogResult::Ok
    }

    fn alter_config(
        &mut self,
        name: &str,
        change: ConfigChange,
        timestamp_ms: i64,
        changes: &mut Vec<CatalogChange>,
    ) -> CatalogResult {
        let Some(topic) = self.topics.get(name) else {
            return CatalogResult::Err(CatalogError::UnknownTopic);
        };
        let mut config = topic.config.clone();
        let result = match change {
            ConfigChange::Replace(entries) => {
                config.retention_ms = None;
                config.segment_bytes = TopicConfig::default().segment_bytes;
                config.config = HashMap::new();
                entries.into_iter().try_for_each(|(k, v)| set_config(&mut config, k, v))
            }
            ConfigChange::Incremental { set, delete } => {
                for key in delete {
                    match key.as_str() {
                        "retention.ms" => config.retention_ms = None,
                        "segment.bytes" => config.segment_bytes = TopicConfig::default().segment_bytes,
                        _ => {}
                    }
                    config.config.remove(&key);
                }
                set.into_iter().try_for_each(|(k, v)| set_config(&mut config, k, v))
            }
        };
        if let Err(e) = result {
            return CatalogResult::Err(e);
        }
        let topic = self.topics.get_mut(name).expect("checked above");
        topic.config = config;
        topic.updated_at = timestamp(timestamp_ms);
        changes.push(CatalogChange::TopicConfigChanged(name.to_string()));
        CatalogResult::Ok
    }

    fn create_partitions(
        &mut self,
        name: &str,
        new_count: u32,
        replicas: Option<Vec<Vec<u64>>>,
        timestamp_ms: i64,
        changes: &mut Vec<CatalogChange>,
    ) -> CatalogResult {
        let Some(topic) = self.topics.get(name) else {
            return CatalogResult::Err(CatalogError::UnknownTopic);
        };
        let current = topic.config.partition_count;
        if new_count <= current {
            return CatalogResult::Err(CatalogError::InvalidPartitions(format!(
                "topic has {} partitions; the new count must be larger, got {}",
                current, new_count
            )));
        }
        let rf = topic.config.replication_factor;
        let placement = match self.place(name, current, new_count - current, rf, replicas) {
            Ok(p) => p,
            Err(e) => return CatalogResult::Err(e),
        };
        for (i, reps) in placement.into_iter().enumerate() {
            let p = current + i as u32;
            self.assignments.insert((name.to_string(), p), new_assignment(name, p, reps));
        }
        let topic = self.topics.get_mut(name).expect("checked above");
        topic.config.partition_count = new_count;
        topic.updated_at = timestamp(timestamp_ms);
        changes.push(CatalogChange::PartitionsAdded { topic: name.to_string(), from: current, to: new_count });
        CatalogResult::Ok
    }

    /// Replica lists for `count` partitions starting at `first`: the explicit
    /// ones, validated, or round-robin over the registered brokers.
    fn place(
        &mut self,
        topic: &str,
        first: u32,
        count: u32,
        replication_factor: u32,
        explicit: Option<Vec<Vec<u64>>>,
    ) -> Result<Vec<Vec<u64>>, CatalogError> {
        let brokers: Vec<u64> = self.brokers.keys().map(|&b| b as u64).collect();
        if let Some(lists) = explicit {
            if lists.len() != count as usize {
                return Err(CatalogError::InvalidReplicaAssignment(format!(
                    "{} replica lists for {} partitions",
                    lists.len(),
                    count
                )));
            }
            for (i, reps) in lists.iter().enumerate() {
                validate_replicas(reps, &brokers)
                    .map_err(|m| CatalogError::InvalidReplicaAssignment(format!("{}-{}: {}", topic, first + i as u32, m)))?;
            }
            return Ok(lists);
        }
        if brokers.is_empty() {
            return Err(CatalogError::NoBrokers);
        }
        let n = brokers.len();
        let rf = (replication_factor.max(1) as usize).min(n);
        let start = (self.placement_cursor % n as u64) as usize;
        self.placement_cursor = self.placement_cursor.wrapping_add(1);
        Ok((0..count as usize)
            .map(|p| (0..rf).map(|r| brokers[(start + first as usize + p + r) % n]).collect())
            .collect())
    }

    // ---- assignments ---------------------------------------------------------

    fn set_assignment(&mut self, c: AssignmentChange, changes: &mut Vec<CatalogChange>) -> Result<(), CatalogError> {
        let Some(topic) = self.topics.get(&c.topic) else {
            return Err(CatalogError::UnknownTopic);
        };
        if c.partition >= topic.config.partition_count {
            return Err(CatalogError::UnknownPartition);
        }
        let key = (c.topic.clone(), c.partition);
        let current = self.assignments.get(&key).cloned();
        if let (Some(expected), Some(cur)) = (c.expected_epoch, &current) {
            if cur.leader_epoch != expected {
                return Err(CatalogError::StaleEpoch { current: cur.leader_epoch });
            }
        }
        let brokers: Vec<u64> = self.brokers.keys().map(|&b| b as u64).collect();
        let replicas = match (&c.replicas, &current) {
            (Some(r), _) => {
                validate_replicas(r, &brokers).map_err(CatalogError::InvalidReplicaAssignment)?;
                r.clone()
            }
            (None, Some(cur)) => cur.replicas.clone(),
            (None, None) => {
                return Err(CatalogError::InvalidReplicaAssignment(
                    "an unassigned partition needs a replica list".into(),
                ))
            }
        };
        let leader = match c.leader {
            Some(l) if replicas.contains(&l) => l,
            Some(l) => {
                return Err(CatalogError::InvalidReplicaAssignment(format!("leader {} is not a replica", l)));
            }
            None => match &current {
                Some(cur) if replicas.contains(&cur.leader_id) => cur.leader_id,
                _ => replicas[0],
            },
        };
        let mut isr: Vec<u64> = match (&c.isr, &current) {
            (Some(isr), _) => isr.clone(),
            (None, Some(cur)) => cur.isr.clone(),
            (None, None) => replicas.clone(),
        };
        isr.retain(|r| replicas.contains(r));
        if !isr.contains(&leader) {
            isr.insert(0, leader);
        }
        // The epoch moves if and only if the leader changes.
        let epoch = match &current {
            Some(cur) if cur.leader_id == leader => cur.leader_epoch,
            Some(cur) => cur.leader_epoch.saturating_add(1),
            None => 0,
        };
        let next = PartitionAssignment {
            topic: c.topic.clone(),
            partition: c.partition,
            broker_id: leader as i32,
            is_leader: true,
            replicas,
            leader_id: leader,
            leader_epoch: epoch,
            isr,
        };
        if current.as_ref() != Some(&next) {
            self.assignments.insert(key, next);
            changes.push(CatalogChange::AssignmentChanged { topic: c.topic, partition: c.partition });
        }
        Ok(())
    }

    fn update_isr(&mut self, u: IsrUpdate, changes: &mut Vec<CatalogChange>) -> Result<(), CatalogError> {
        let key = (u.topic.clone(), u.partition);
        let Some(cur) = self.assignments.get_mut(&key) else {
            return Err(if self.topics.contains_key(&u.topic) {
                CatalogError::UnknownPartition
            } else {
                CatalogError::UnknownTopic
            });
        };
        if cur.leader_id != u.leader_id {
            return Err(CatalogError::NotLeader { current: cur.leader_id });
        }
        if cur.leader_epoch != u.leader_epoch {
            return Err(CatalogError::StaleEpoch { current: cur.leader_epoch });
        }
        let mut isr = u.isr;
        isr.retain(|r| cur.replicas.contains(r));
        if !isr.contains(&cur.leader_id) {
            return Err(CatalogError::InvalidReplicaAssignment("the leader must be in its ISR".into()));
        }
        // Keep replica order, so equal sets compare equal.
        isr.sort_by_key(|r| cur.replicas.iter().position(|x| x == r));
        isr.dedup();
        if cur.isr != isr {
            cur.isr = isr;
            changes.push(CatalogChange::AssignmentChanged { topic: u.topic, partition: u.partition });
        }
        Ok(())
    }
}

fn new_assignment(topic: &str, partition: u32, replicas: Vec<u64>) -> PartitionAssignment {
    let leader = replicas[0];
    PartitionAssignment {
        topic: topic.to_string(),
        partition,
        broker_id: leader as i32,
        is_leader: true,
        isr: replicas.clone(),
        replicas,
        leader_id: leader,
        leader_epoch: 0,
    }
}

fn validate_replicas(replicas: &[u64], brokers: &[u64]) -> Result<(), String> {
    if replicas.is_empty() {
        return Err("empty replica list".into());
    }
    let mut seen = BTreeSet::new();
    for r in replicas {
        if !seen.insert(*r) {
            return Err(format!("replica {} listed twice", r));
        }
        if !brokers.contains(r) {
            return Err(format!("replica {} is not a registered broker", r));
        }
    }
    Ok(())
}

/// Set one topic config key, keeping the typed fields in step with the map.
fn set_config(config: &mut TopicConfig, key: String, value: String) -> Result<(), CatalogError> {
    match key.as_str() {
        "retention.ms" => match value.parse::<i64>() {
            Ok(-1) => config.retention_ms = None,
            Ok(ms) if ms >= 0 => config.retention_ms = Some(ms),
            _ => return Err(CatalogError::InvalidConfig(format!("retention.ms: {}", value))),
        },
        "segment.bytes" => match value.parse::<i64>() {
            Ok(b) if b > 0 => config.segment_bytes = b,
            _ => return Err(CatalogError::InvalidConfig(format!("segment.bytes: {}", value))),
        },
        _ => {
            config.config.insert(key, value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker(id: i32) -> BrokerMetadata {
        BrokerMetadata {
            broker_id: id,
            host: format!("node{}", id),
            port: 9092,
            rack: None,
            status: BrokerStatus::Online,
            created_at: timestamp(1_000),
            updated_at: timestamp(1_000),
        }
    }

    fn config(partitions: u32, rf: u32) -> TopicConfig {
        TopicConfig { partition_count: partitions, replication_factor: rf, ..TopicConfig::default() }
    }

    fn create(name: &str, partitions: u32, rf: u32) -> CatalogCommand {
        CatalogCommand::CreateTopic {
            name: name.into(),
            config: config(partitions, rf),
            auto_created: false,
            replicas: None,
            timestamp_ms: 5_000,
        }
    }

    /// A catalog with brokers 1..=n, applied as log entries 1..=n.
    fn with_brokers(n: i32) -> (CatalogState, u64) {
        let mut s = CatalogState::default();
        for id in 1..=n {
            s.apply(id as u64, 1, CatalogCommand::RegisterBroker { broker: broker(id) });
        }
        (s, n as u64)
    }

    fn run(s: &mut CatalogState, index: &mut u64, cmd: CatalogCommand) -> CatalogResult {
        *index += 1;
        s.apply_entry(*index, 2, &cmd.encode()).0
    }

    fn replicas_of(s: &CatalogState, topic: &str) -> Vec<Vec<u64>> {
        s.assignments_for(topic).into_iter().map(|a| a.replicas).collect()
    }

    #[test]
    fn create_places_round_robin_and_rotates_leaders_across_topics() {
        let (mut s, mut i) = with_brokers(3);
        match run(&mut s, &mut i, create("a", 3, 3)) {
            CatalogResult::Topic { assignments, .. } => assert_eq!(assignments.len(), 3),
            other => panic!("{:?}", other),
        }
        assert_eq!(replicas_of(&s, "a"), vec![vec![1, 2, 3], vec![2, 3, 1], vec![3, 1, 2]]);
        // Single-partition topics take turns leading.
        run(&mut s, &mut i, create("b", 1, 3));
        run(&mut s, &mut i, create("c", 1, 3));
        assert_eq!(s.assignment("b", 0).unwrap().leader_id, 2);
        assert_eq!(s.assignment("c", 0).unwrap().leader_id, 3);
        // RF capped at the broker count; ISR starts as the replica set.
        run(&mut s, &mut i, create("d", 1, 5));
        let d = s.assignment("d", 0).unwrap();
        assert_eq!(d.replicas.len(), 3);
        assert_eq!(d.isr, d.replicas);
        assert_eq!(d.leader_epoch, 0);
    }

    #[test]
    fn a_name_is_created_once_and_ids_come_from_the_log() {
        let (mut s, mut i) = with_brokers(3);
        let first = match run(&mut s, &mut i, create("t", 1, 1)) {
            CatalogResult::Topic { topic, .. } => topic,
            other => panic!("{:?}", other),
        };
        assert_eq!(first.id, topic_id(2, i));
        assert_eq!(first.created_at, timestamp(5_000));
        assert_eq!(run(&mut s, &mut i, create("t", 3, 3)), CatalogResult::Err(CatalogError::TopicAlreadyExists));
        // The losing create changed nothing.
        assert_eq!(s.topic("t").unwrap().config.partition_count, 1);
        run(&mut s, &mut i, CatalogCommand::DeleteTopic { name: "t".into() });
        assert!(s.assignments_for("t").is_empty());
        let again = match run(&mut s, &mut i, create("t", 2, 1)) {
            CatalogResult::Topic { topic, .. } => topic,
            other => panic!("{:?}", other),
        };
        assert_ne!(again.id, first.id, "a recreated topic is a different topic");
        assert_eq!(s.assignments_for("t").len(), 2);
    }

    #[test]
    fn creation_is_refused_without_brokers_or_with_bad_replicas() {
        let mut s = CatalogState::default();
        let mut i = 0;
        assert_eq!(run(&mut s, &mut i, create("t", 1, 1)), CatalogResult::Err(CatalogError::NoBrokers));
        let (mut s, mut i) = with_brokers(2);
        let explicit = |lists: Vec<Vec<u64>>| CatalogCommand::CreateTopic {
            name: "t".into(),
            config: config(lists.len().max(1) as u32, 2),
            auto_created: false,
            replicas: Some(lists),
            timestamp_ms: 0,
        };
        for bad in [vec![vec![1, 1]], vec![vec![1, 9]], vec![vec![]]] {
            assert!(matches!(
                run(&mut s, &mut i, explicit(bad)),
                CatalogResult::Err(CatalogError::InvalidReplicaAssignment(_))
            ));
        }
        assert!(matches!(run(&mut s, &mut i, create("z", 0, 1)), CatalogResult::Err(CatalogError::InvalidPartitions(_))));
        assert!(s.topics().next().is_none());
        assert!(matches!(run(&mut s, &mut i, explicit(vec![vec![2, 1]])), CatalogResult::Topic { .. }));
        assert_eq!(s.assignment("t", 0).unwrap().leader_id, 2);
    }

    #[test]
    fn config_replace_and_incremental_keep_typed_fields_in_step() {
        let (mut s, mut i) = with_brokers(1);
        run(&mut s, &mut i, create("t", 1, 1));
        let set = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        run(&mut s, &mut i, CatalogCommand::AlterTopicConfig {
            name: "t".into(),
            change: ConfigChange::Incremental { set: set(&[("retention.ms", "1000"), ("cleanup.policy", "compact")]), delete: vec![] },
            timestamp_ms: 9_000,
        });
        let c = &s.topic("t").unwrap().config;
        assert_eq!((c.retention_ms, c.config.get("cleanup.policy").map(String::as_str)), (Some(1000), Some("compact")));
        assert_eq!(s.topic("t").unwrap().updated_at, timestamp(9_000));

        // An invalid value refuses the whole change.
        let r = run(&mut s, &mut i, CatalogCommand::AlterTopicConfig {
            name: "t".into(),
            change: ConfigChange::Incremental { set: set(&[("segment.bytes", "-5"), ("x", "y")]), delete: vec![] },
            timestamp_ms: 0,
        });
        assert!(matches!(r, CatalogResult::Err(CatalogError::InvalidConfig(_))));
        assert!(!s.topic("t").unwrap().config.config.contains_key("x"));

        run(&mut s, &mut i, CatalogCommand::AlterTopicConfig {
            name: "t".into(),
            change: ConfigChange::Replace(set(&[("segment.bytes", "2048")])),
            timestamp_ms: 0,
        });
        let c = &s.topic("t").unwrap().config;
        assert_eq!((c.retention_ms, c.segment_bytes, c.config.len()), (None, 2048, 0));
        assert_eq!(c.partition_count, 1, "structural fields survive a replace");
    }

    #[test]
    fn partitions_only_grow_and_new_ones_are_placed() {
        let (mut s, mut i) = with_brokers(3);
        run(&mut s, &mut i, create("t", 1, 2));
        let grow = |n| CatalogCommand::CreatePartitions { name: "t".into(), new_count: n, replicas: None, timestamp_ms: 0 };
        assert!(matches!(run(&mut s, &mut i, grow(1)), CatalogResult::Err(CatalogError::InvalidPartitions(_))));
        assert_eq!(run(&mut s, &mut i, grow(3)), CatalogResult::Ok);
        assert_eq!(s.topic("t").unwrap().config.partition_count, 3);
        let reps = replicas_of(&s, "t");
        assert_eq!(reps.len(), 3);
        assert!(reps.iter().all(|r| r.len() == 2));
        assert_eq!(run(&mut s, &mut i, CatalogCommand::CreatePartitions { name: "nope".into(), new_count: 2, replicas: None, timestamp_ms: 0 }),
            CatalogResult::Err(CatalogError::UnknownTopic));
    }

    fn change(topic: &str, partition: u32) -> AssignmentChange {
        AssignmentChange { topic: topic.into(), partition, replicas: None, leader: None, isr: None, expected_epoch: None }
    }

    #[test]
    fn epoch_moves_only_when_the_leader_changes_and_stale_decisions_are_refused() {
        let (mut s, mut i) = with_brokers(3);
        run(&mut s, &mut i, create("t", 1, 3)); // [1,2,3]
        let batch = |c: AssignmentChange| CatalogCommand::SetAssignments { changes: vec![c] };

        // Re-asserting the same leader keeps the epoch.
        run(&mut s, &mut i, batch(AssignmentChange { leader: Some(1), ..change("t", 0) }));
        assert_eq!(s.assignment("t", 0).unwrap().leader_epoch, 0);

        // Failover to 2, decided at epoch 0.
        let r = run(&mut s, &mut i, batch(AssignmentChange { leader: Some(2), isr: Some(vec![2, 3]), expected_epoch: Some(0), ..change("t", 0) }));
        assert_eq!(r, CatalogResult::Items(vec![Ok(())]));
        let a = s.assignment("t", 0).unwrap();
        assert_eq!((a.leader_id, a.leader_epoch, a.isr.clone(), a.replicas.clone()), (2, 1, vec![2, 3], vec![1, 2, 3]));

        // A second decision made from the epoch-0 view is refused.
        let r = run(&mut s, &mut i, batch(AssignmentChange { leader: Some(3), expected_epoch: Some(0), ..change("t", 0) }));
        assert_eq!(r, CatalogResult::Items(vec![Err(CatalogError::StaleEpoch { current: 1 })]));
        assert_eq!(s.assignment("t", 0).unwrap().leader_id, 2);

        // A leader outside the replicas, an unknown partition: refused item by item.
        let r = run(&mut s, &mut i, CatalogCommand::SetAssignments { changes: vec![
            AssignmentChange { leader: Some(9), ..change("t", 0) },
            change("t", 7),
            change("missing", 0),
            AssignmentChange { leader: Some(1), ..change("t", 0) },
        ]});
        match r {
            CatalogResult::Items(items) => {
                assert!(matches!(items[0], Err(CatalogError::InvalidReplicaAssignment(_))));
                assert_eq!(items[1], Err(CatalogError::UnknownPartition));
                assert_eq!(items[2], Err(CatalogError::UnknownTopic));
                assert_eq!(items[3], Ok(()));
            }
            other => panic!("{:?}", other),
        }
        // Preferred leader 1 returns: epoch 2, and 1 joins the ISR as leader.
        let a = s.assignment("t", 0).unwrap();
        assert_eq!((a.leader_id, a.leader_epoch), (1, 2));
        assert!(a.isr.contains(&1));
    }

    #[test]
    fn isr_reports_are_fenced_by_leader_and_epoch() {
        let (mut s, mut i) = with_brokers(3);
        run(&mut s, &mut i, create("t", 1, 3)); // leader 1, epoch 0
        let report = |leader, epoch, isr: Vec<u64>| CatalogCommand::UpdateIsr {
            updates: vec![IsrUpdate { topic: "t".into(), partition: 0, leader_id: leader, leader_epoch: epoch, isr }],
        };
        assert_eq!(run(&mut s, &mut i, report(1, 0, vec![3, 1])), CatalogResult::Items(vec![Ok(())]));
        assert_eq!(s.assignment("t", 0).unwrap().isr, vec![1, 3], "kept in replica order");
        assert_eq!(run(&mut s, &mut i, report(2, 0, vec![2])), CatalogResult::Items(vec![Err(CatalogError::NotLeader { current: 1 })]));
        assert_eq!(run(&mut s, &mut i, report(1, 5, vec![1])), CatalogResult::Items(vec![Err(CatalogError::StaleEpoch { current: 0 })]));
        assert!(matches!(run(&mut s, &mut i, report(1, 0, vec![2, 3])),
            CatalogResult::Items(ref v) if matches!(v[0], Err(CatalogError::InvalidReplicaAssignment(_)))));
        assert_eq!(s.assignment("t", 0).unwrap().isr, vec![1, 3]);
    }

    /// An unassigned partition (the Thunderbird shape) can be given replicas.
    #[test]
    fn an_unassigned_partition_is_repaired_with_explicit_replicas() {
        let (mut s, mut i) = with_brokers(3);
        run(&mut s, &mut i, create("t", 2, 3));
        s.assignments.remove(&("t".to_string(), 1));
        let r = run(&mut s, &mut i, CatalogCommand::SetAssignments { changes: vec![change("t", 1)] });
        assert!(matches!(r, CatalogResult::Items(ref v) if matches!(v[0], Err(CatalogError::InvalidReplicaAssignment(_)))));
        run(&mut s, &mut i, CatalogCommand::SetAssignments { changes: vec![AssignmentChange { replicas: Some(vec![2, 3, 1]), ..change("t", 1) }] });
        let a = s.assignment("t", 1).unwrap();
        assert_eq!((a.leader_id, a.leader_epoch, a.isr.clone()), (2, 0, vec![2, 3, 1]));
    }

    #[test]
    fn acls_scram_and_brokers() {
        let (mut s, mut i) = with_brokers(1);
        let acl = AclBindingRecord {
            resource_type: 2,
            resource_name: "orders".into(),
            pattern_type: 3,
            principal: "User:alice".into(),
            host: "*".into(),
            operation: 3,
            permission_type: 3,
        };
        run(&mut s, &mut i, CatalogCommand::CreateAcl { binding: acl.clone() });
        run(&mut s, &mut i, CatalogCommand::CreateAcl { binding: acl.clone() });
        assert_eq!(s.acls(), vec![acl.clone()]);
        assert_eq!(run(&mut s, &mut i, CatalogCommand::DeleteAcl { binding: acl.clone() }), CatalogResult::Ok);
        assert_eq!(run(&mut s, &mut i, CatalogCommand::DeleteAcl { binding: acl }), CatalogResult::Err(CatalogError::NotFound));

        let cred = ScramCredentialRecord {
            username: "alice".into(),
            mechanism: 2,
            iterations: 4096,
            salt: vec![1],
            stored_key: vec![2],
            server_key: vec![3],
        };
        run(&mut s, &mut i, CatalogCommand::UpsertScram { credential: cred.clone() });
        assert_eq!(s.scram_credentials(), vec![cred]);
        assert_eq!(run(&mut s, &mut i, CatalogCommand::DeleteScram { username: "alice".into(), mechanism: 2 }), CatalogResult::Ok);
        assert!(s.scram_credentials().is_empty());

        // Re-registering keeps the original creation time.
        let mut b = broker(1);
        b.host = "moved".into();
        b.created_at = timestamp(99_999);
        run(&mut s, &mut i, CatalogCommand::RegisterBroker { broker: b });
        let got = s.broker(1).unwrap();
        assert_eq!((got.host.as_str(), got.created_at), ("moved", timestamp(1_000)));
    }

    #[test]
    fn malformed_entries_are_refused_identically() {
        let mut s = CatalogState::default();
        let (r, ch) = s.apply_entry(7, 1, &[CATALOG_FORMAT_VERSION, 0xFF, 0xFF]);
        assert!(matches!(r, CatalogResult::Err(CatalogError::Malformed(_))));
        assert!(ch.is_empty());
        assert_eq!(s.applied_index(), 7);
        let (r, _) = s.apply_entry(8, 1, &[99, 1, 2]);
        assert!(matches!(r, CatalogResult::Err(CatalogError::Malformed(_))));
    }

    /// Two nodes applying the same entries reach the same state; a snapshot
    /// taken mid-way restores a third that then continues identically.
    #[test]
    fn replay_is_deterministic_and_snapshots_round_trip() {
        let mut cmds: Vec<CatalogCommand> = (1..=3).map(|id| CatalogCommand::RegisterBroker { broker: broker(id) }).collect();
        for t in 0..20 {
            cmds.push(create(&format!("t{}", t), 1 + t % 4, 3));
        }
        cmds.push(create("t3", 1, 1));
        cmds.push(CatalogCommand::DeleteTopic { name: "t5".into() });
        cmds.push(CatalogCommand::SetAssignments { changes: vec![AssignmentChange { leader: Some(3), ..change("t0", 0) }] });
        cmds.push(CatalogCommand::CreatePartitions { name: "t1".into(), new_count: 6, replicas: None, timestamp_ms: 1 });
        let entries: Vec<Vec<u8>> = cmds.iter().map(|c| c.encode()).collect();

        let mut a = CatalogState::default();
        let mut b = CatalogState::default();
        let mut c = CatalogState::default();
        let mut results_a = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            results_a.push(a.apply_entry(i as u64 + 1, 1, e).0);
            assert_eq!(b.apply_entry(i as u64 + 1, 1, e).0, results_a[i]);
            if i == 12 {
                c = CatalogState::from_snapshot(&b.to_snapshot()).unwrap();
            } else if i > 12 {
                assert_eq!(c.apply_entry(i as u64 + 1, 1, e).0, results_a[i]);
            }
        }
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(CatalogState::from_snapshot(&a.to_snapshot()).unwrap(), a);
        assert!(CatalogState::from_snapshot(&[42]).is_err());
    }
}
