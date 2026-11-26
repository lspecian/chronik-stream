//! `MetadataStore` over the Raft catalog.
//!
//! Cluster-shape state — topics, partition assignments, brokers, ACLs, SCRAM
//! credentials — goes through [`ClusterCatalog`]: writes are commands in the
//! catalog log (forwarded to its leader from any node), reads come from the
//! applied catalog in memory. Everything high-frequency or node-local —
//! high watermarks, log start offsets, segment and Parquet records, consumer
//! groups and offsets, transactions — stays in the node's own
//! [`WalMetadataStore`], exactly as before.
//!
//! When the catalog deletes a topic, every node purges its local state for it
//! (see [`CatalogMetadataStore::handle_change`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chronik_common::metadata::catalog::{
    AssignmentChange, CatalogChange, CatalogCommand, CatalogError, CatalogResult, ConfigChange, GroupOffset,
    IsrUpdate, OffsetCommitEntry, FEATURE_GROUP_OFFSETS, FEATURE_GROUP_OFFSETS_MIRROR,
};
use chronik_common::metadata::{
    AclBindingRecord, BrokerMetadata, BrokerStatus, ConsumerGroupMetadata, ConsumerOffset, MetadataError,
    MetadataEvent, MetadataStore, ParquetSegmentMetadata, PartitionAssignment, Result, ScramCredentialRecord,
    SegmentMetadata, TopicConfig, TopicMetadata, TransactionMetadata, WalMetadataStore,
};
use tokio::sync::watch;

use crate::cluster_catalog::ClusterCatalog;

pub struct CatalogMetadataStore {
    catalog: Arc<ClusterCatalog>,
    local: Arc<WalMetadataStore>,
    /// Bumped whenever ACLs or SCRAM credentials change, for the security caches.
    security_generation: watch::Sender<u64>,
    /// Commits hold it shared from choosing where to write until written; the
    /// offset merge holds it exclusively, so no commit decided "local only"
    /// before the merge lands after the merge has read the local offsets.
    offsets_gate: tokio::sync::RwLock<()>,
}

/// Where committed offsets are written and read during the move to the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OffsetMode {
    /// Some voter does not understand catalog offsets: each node, as before.
    Local,
    /// [`FEATURE_GROUP_OFFSETS_MIRROR`]: every voter understands them, not all
    /// have merged their local offsets yet: read locally, write locally and
    /// to the catalog.
    Mirrored,
    /// [`FEATURE_GROUP_OFFSETS`] is on: the catalog only.
    Catalog,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Declared by a node once its pre-catalog offsets are merged into the catalog.
const OFFSETS_MERGED: &str = "group-offsets-merged";

fn offset_entry(o: ConsumerOffset) -> OffsetCommitEntry {
    OffsetCommitEntry {
        topic: o.topic,
        partition: o.partition,
        offset: o.offset,
        metadata: o.metadata,
        commit_timestamp_ms: o.commit_timestamp.timestamp_millis(),
    }
}

fn consumer_offset(group_id: &str, topic: &str, partition: u32, o: &GroupOffset) -> ConsumerOffset {
    ConsumerOffset {
        group_id: group_id.to_string(),
        topic: topic.to_string(),
        partition,
        offset: o.offset,
        metadata: o.metadata.clone(),
        commit_timestamp: chrono::DateTime::from_timestamp_millis(o.commit_timestamp_ms).unwrap_or_default(),
    }
}

fn to_metadata_error(e: CatalogError, subject: &str) -> MetadataError {
    match e {
        CatalogError::TopicAlreadyExists => MetadataError::AlreadyExists(subject.to_string()),
        CatalogError::UnknownTopic | CatalogError::UnknownPartition | CatalogError::NotFound => {
            MetadataError::NotFound(subject.to_string())
        }
        other => MetadataError::StorageError(format!("{}: {}", subject, other)),
    }
}

impl CatalogMetadataStore {
    /// Committed offsets live in the catalog once every node supports it
    /// ([`FEATURE_GROUP_OFFSETS`]); before that, on each node as they were.
    pub fn offsets_in_catalog(&self) -> bool {
        self.offset_mode() == OffsetMode::Catalog
    }

    fn offset_mode(&self) -> OffsetMode {
        let st = self.catalog.read();
        if st.has_feature(FEATURE_GROUP_OFFSETS) {
            OffsetMode::Catalog
        } else if st.has_feature(FEATURE_GROUP_OFFSETS_MIRROR) {
            OffsetMode::Mirrored
        } else {
            OffsetMode::Local
        }
    }

    /// Bring this node to [`FEATURE_GROUP_OFFSETS`] without a moment where a
    /// committed offset is missing:
    ///   1. declare the capability once caught up;
    ///   2. once every voter has declared it (so none would skip an offset
    ///      command) the leader enables [`FEATURE_GROUP_OFFSETS_MIRROR`]:
    ///      commits go to the catalog as well as locally, and this
    ///      node merges its pre-catalog offsets into the catalog (the newer
    ///      commit wins) and records that it did, in the catalog, so a restart
    ///      never merges again (and never resurrects an offset deleted since);
    ///   3. the leader enables the feature once every voter has merged; from
    ///      then on offsets are read from the catalog, which by now holds every
    ///      node's offsets and every commit made since step 2.
    /// Until step 2, offsets stay on each node as before, so a cluster
    /// mid-upgrade never sends a command an older node would skip.
    pub fn spawn_group_offsets_rollout(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let store = self.clone();
        tokio::spawn(async move {
            let node = store.catalog.node_id();
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if !store.catalog.is_caught_up() {
                    continue;
                }
                let (declared, migrated, mirrored, enabled, all_declared, all_merged) = {
                    let st = store.catalog.read();
                    let voters = store.catalog.voters();
                    (
                        st.declared(node, FEATURE_GROUP_OFFSETS),
                        st.declared(node, OFFSETS_MERGED),
                        st.has_feature(FEATURE_GROUP_OFFSETS_MIRROR),
                        st.has_feature(FEATURE_GROUP_OFFSETS),
                        st.all_declare(&voters, FEATURE_GROUP_OFFSETS),
                        st.all_declare(&voters, OFFSETS_MERGED),
                    )
                };
                if enabled && migrated {
                    return;
                }
                if !declared {
                    let caps = if migrated {
                        vec![FEATURE_GROUP_OFFSETS.to_string(), OFFSETS_MERGED.to_string()]
                    } else {
                        vec![FEATURE_GROUP_OFFSETS.to_string()]
                    };
                    if let Err(e) = store.catalog.execute(CatalogCommand::DeclareCapabilities { node_id: node, capabilities: caps }).await {
                        tracing::debug!("declaring capabilities: {e:?}");
                    }
                    continue;
                }
                if !mirrored && !enabled {
                    if store.catalog.am_i_leader() && all_declared {
                        store.enable(FEATURE_GROUP_OFFSETS_MIRROR, "Every node supports catalog consumer-group offsets: \
                            commits now also go to the catalog while nodes merge their own").await;
                    }
                    continue;
                }
                if !migrated {
                    match store.merge_local_offsets().await {
                        Ok(count) => {
                            let caps = vec![FEATURE_GROUP_OFFSETS.to_string(), OFFSETS_MERGED.to_string()];
                            match store.catalog.execute(CatalogCommand::DeclareCapabilities { node_id: node, capabilities: caps }).await {
                                Ok(_) => tracing::info!("Merged {count} committed offset(s) of this node into the catalog"),
                                Err(e) => tracing::warn!("recording the offset merge: {e:?}"),
                            }
                        }
                        Err(e) => tracing::warn!("merging this node's offsets into the catalog (retrying): {e}"),
                    }
                    continue;
                }
                if !enabled && store.catalog.am_i_leader() && all_merged {
                    store.enable(FEATURE_GROUP_OFFSETS, "Every node merged its offsets: catalog consumer-group offsets \
                        enabled (groups are now coordinated by the catalog leader)").await;
                }
            }
        })
    }

    async fn enable(&self, feature: &str, announce: &str) {
        match self.catalog.execute(CatalogCommand::EnableFeature { feature: feature.to_string() }).await {
            Ok(_) => tracing::info!("{announce}"),
            Err(e) => tracing::debug!("enabling {feature}: {e:?}"),
        }
    }

    /// Propose this node's local offsets, per group, keeping newer catalog ones.
    async fn merge_local_offsets(&self) -> Result<usize> {
        // Exclusive: every commit that chose "local only" has landed, and every
        // later one also writes to the catalog.
        let _gate = self.offsets_gate.write().await;
        let mut by_group: BTreeMap<String, Vec<OffsetCommitEntry>> = BTreeMap::new();
        for o in self.local.all_consumer_offsets().await {
            by_group.entry(o.group_id.clone()).or_default().push(offset_entry(o));
        }
        let mut count = 0;
        for (group_id, entries) in by_group {
            for chunk in entries.chunks(500) {
                count += chunk.len();
                self.run(
                    CatalogCommand::CommitOffsets {
                        group_id: group_id.clone(),
                        offsets: chunk.to_vec(),
                        only_if_newer: true,
                    },
                    &format!("merging offsets of group {group_id}"),
                )
                .await?;
            }
        }
        Ok(count)
    }

    /// One catalog entry per group (an OffsetCommit names one group).
    async fn commit_offsets_to_catalog(&self, offsets: Vec<ConsumerOffset>) -> Result<Vec<Result<()>>> {
        let mut results: Vec<Result<()>> = (0..offsets.len()).map(|_| Ok(())).collect();
        let mut by_group: BTreeMap<String, Vec<(usize, OffsetCommitEntry)>> = BTreeMap::new();
        for (i, o) in offsets.into_iter().enumerate() {
            by_group.entry(o.group_id.clone()).or_default().push((i, offset_entry(o)));
        }
        for (group_id, items) in by_group {
            let (positions, entries): (Vec<usize>, Vec<OffsetCommitEntry>) = items.into_iter().unzip();
            let subject = format!("offsets of group {group_id}");
            match self
                .catalog
                .execute(CatalogCommand::CommitOffsets { group_id, offsets: entries, only_if_newer: false })
                .await?
            {
                CatalogResult::Items(outcomes) => {
                    for (pos, outcome) in positions.into_iter().zip(outcomes) {
                        results[pos] = outcome.map_err(|e| to_metadata_error(e, &subject));
                    }
                }
                CatalogResult::Err(e) => {
                    let err = to_metadata_error(e, &subject);
                    for pos in positions {
                        results[pos] = Err(MetadataError::StorageError(err.to_string()));
                    }
                }
                _ => {}
            }
        }
        Ok(results)
    }

    pub fn new(catalog: Arc<ClusterCatalog>, local: Arc<WalMetadataStore>) -> Self {
        let (security_generation, _) = watch::channel(0);
        Self { catalog, local, security_generation, offsets_gate: tokio::sync::RwLock::new(()) }
    }

    pub fn catalog(&self) -> &Arc<ClusterCatalog> {
        &self.catalog
    }

    /// Run a command and fail on a refused one.
    async fn run(&self, cmd: CatalogCommand, subject: &str) -> Result<CatalogResult> {
        match self.catalog.execute(cmd).await? {
            CatalogResult::Err(e) => Err(to_metadata_error(e, subject)),
            ok => Ok(ok),
        }
    }

    async fn create(
        &self,
        name: &str,
        config: TopicConfig,
        auto_created: bool,
        replicas: Option<Vec<Vec<u64>>>,
    ) -> Result<TopicMetadata> {
        let cmd = CatalogCommand::CreateTopic {
            name: name.to_string(),
            config,
            auto_created,
            replicas,
            timestamp_ms: now_ms(),
        };
        match self.run(cmd, name).await? {
            CatalogResult::Topic { topic, .. } => Ok(topic),
            other => Err(MetadataError::StorageError(format!("unexpected result {:?}", other))),
        }
    }

    /// Explicit replica lists from caller-built assignments, when they cover
    /// every partition; otherwise the catalog places them.
    fn replicas_from(config: &TopicConfig, assignments: &[PartitionAssignment]) -> Option<Vec<Vec<u64>>> {
        let mut by_partition: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
        for a in assignments {
            if !a.replicas.is_empty() {
                by_partition.insert(a.partition, a.replicas.clone());
            }
        }
        let complete = by_partition.len() == config.partition_count as usize
            && by_partition.keys().copied().eq(0..config.partition_count);
        complete.then(|| by_partition.into_values().collect())
    }

    async fn create_with_offsets(
        &self,
        name: &str,
        config: TopicConfig,
        auto_created: bool,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>,
    ) -> Result<TopicMetadata> {
        let replicas = Self::replicas_from(&config, &assignments);
        let topic = self.create(name, config, auto_created, replicas).await?;
        for (partition, hwm, lso) in offsets {
            self.local.update_partition_offset(name, partition, hwm, lso).await?;
        }
        Ok(topic)
    }

    /// React to an applied catalog change on this node.
    pub async fn handle_change(&self, change: &CatalogChange) {
        match change {
            CatalogChange::TopicDeleted(name) => {
                if let Err(e) = self.local.purge_topic_local(name).await {
                    tracing::warn!("Purging local state of deleted topic {}: {}", name, e);
                }
            }
            CatalogChange::AclsChanged | CatalogChange::ScramChanged | CatalogChange::Reset => {
                self.security_generation.send_modify(|g| *g += 1);
            }
            _ => {}
        }
    }
}

#[async_trait]
impl MetadataStore for CatalogMetadataStore {
    // ---- topics ------------------------------------------------------------

    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        self.create(name, config, false, None).await
    }

    async fn auto_create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        self.create(name, config, true, None).await
    }

    async fn create_topic_with_assignments(
        &self,
        name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>,
    ) -> Result<TopicMetadata> {
        self.create_with_offsets(name, config, false, assignments, offsets).await
    }

    async fn auto_create_topic_with_assignments(
        &self,
        name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>,
    ) -> Result<TopicMetadata> {
        self.create_with_offsets(name, config, true, assignments, offsets).await
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        Ok(self.catalog.read().topic(name).cloned())
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        Ok(self.catalog.read().topics().cloned().collect())
    }

    /// Partition growth and config changes, expressed as catalog commands.
    /// Prefer the specific commands; this exists for callers that hand over a
    /// whole new `TopicConfig`.
    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let current = self
            .catalog
            .read()
            .topic(name)
            .cloned()
            .ok_or_else(|| MetadataError::NotFound(name.to_string()))?;
        if config.partition_count < current.config.partition_count {
            return Err(MetadataError::StorageError(format!(
                "{}: partitions cannot be removed ({} -> {})",
                name, current.config.partition_count, config.partition_count
            )));
        }
        if config.partition_count > current.config.partition_count {
            self.run(
                CatalogCommand::CreatePartitions {
                    name: name.to_string(),
                    new_count: config.partition_count,
                    replicas: None,
                    timestamp_ms: now_ms(),
                },
                name,
            )
            .await?;
        }
        let differs = config.retention_ms != current.config.retention_ms
            || config.segment_bytes != current.config.segment_bytes
            || config.config != current.config.config;
        if differs {
            let mut entries: BTreeMap<String, String> =
                config.config.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            if let Some(ms) = config.retention_ms {
                entries.insert("retention.ms".into(), ms.to_string());
            }
            entries.insert("segment.bytes".into(), config.segment_bytes.to_string());
            self.run(
                CatalogCommand::AlterTopicConfig {
                    name: name.to_string(),
                    change: ConfigChange::Replace(entries),
                    timestamp_ms: now_ms(),
                },
                name,
            )
            .await?;
        }
        self.catalog.read().topic(name).cloned().ok_or_else(|| MetadataError::NotFound(name.to_string()))
    }

    async fn delete_topic(&self, name: &str) -> Result<()> {
        self.run(CatalogCommand::DeleteTopic { name: name.to_string() }, name).await.map(|_| ())
    }

    // ---- brokers -----------------------------------------------------------

    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        let subject = format!("broker {}", metadata.broker_id);
        self.run(CatalogCommand::RegisterBroker { broker: metadata }, &subject).await.map(|_| ())
    }

    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        Ok(self.catalog.read().broker(broker_id).cloned())
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        Ok(self.catalog.read().brokers().cloned().collect())
    }

    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let subject = format!("broker {}", broker_id);
        self.run(CatalogCommand::SetBrokerStatus { broker_id, status, timestamp_ms: now_ms() }, &subject)
            .await
            .map(|_| ())
    }

    // ---- assignments -------------------------------------------------------

    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let subject = format!("{}-{}", assignment.topic, assignment.partition);
        let change = AssignmentChange {
            topic: assignment.topic,
            partition: assignment.partition,
            replicas: (!assignment.replicas.is_empty()).then_some(assignment.replicas),
            leader: Some(assignment.leader_id),
            // An empty ISR means "unknown", never "nobody is in sync".
            isr: (!assignment.isr.is_empty()).then_some(assignment.isr),
            expected_epoch: None,
        };
        match self.set_assignments(vec![change]).await?.into_iter().next() {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(to_metadata_error(e, &subject)),
            None => Err(MetadataError::StorageError(format!("{}: empty result", subject))),
        }
    }

    async fn set_assignments(&self, changes: Vec<AssignmentChange>) -> Result<Vec<std::result::Result<(), CatalogError>>> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        match self.catalog.execute(CatalogCommand::SetAssignments { changes }).await? {
            CatalogResult::Items(items) => Ok(items),
            CatalogResult::Err(e) => Err(to_metadata_error(e, "assignments")),
            other => Err(MetadataError::StorageError(format!("unexpected result {:?}", other))),
        }
    }

    async fn update_isr(&self, updates: Vec<IsrUpdate>) -> Result<Vec<std::result::Result<(), CatalogError>>> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        match self.catalog.execute(CatalogCommand::UpdateIsr { updates }).await? {
            CatalogResult::Items(items) => Ok(items),
            CatalogResult::Err(e) => Err(to_metadata_error(e, "isr")),
            other => Err(MetadataError::StorageError(format!("unexpected result {:?}", other))),
        }
    }

    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        Ok(self.catalog.read().assignments_for(topic))
    }

    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        Ok(self.catalog.read().assignment(topic, partition).map(|a| a.leader_id as i32))
    }

    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>> {
        Ok(self
            .catalog
            .read()
            .assignment(topic, partition)
            .map(|a| a.replicas.iter().map(|&r| r as i32).collect()))
    }

    async fn get_partition_leader_epoch(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        Ok(self.catalog.read().assignment(topic, partition).map(|a| a.leader_epoch))
    }

    // ---- security ----------------------------------------------------------

    async fn create_acl(&self, binding: AclBindingRecord) -> Result<()> {
        self.run(CatalogCommand::CreateAcl { binding }, "acl").await.map(|_| ())
    }

    async fn delete_acl(&self, binding: AclBindingRecord) -> Result<()> {
        self.run(CatalogCommand::DeleteAcl { binding }, "acl").await.map(|_| ())
    }

    async fn list_acls(&self) -> Result<Vec<AclBindingRecord>> {
        Ok(self.catalog.read().acls())
    }

    fn acl_changes(&self) -> Option<watch::Receiver<u64>> {
        Some(self.security_generation.subscribe())
    }

    async fn upsert_scram_credential(&self, credential: ScramCredentialRecord) -> Result<()> {
        let subject = format!("SCRAM credential for {}", credential.username);
        self.run(CatalogCommand::UpsertScram { credential }, &subject).await.map(|_| ())
    }

    async fn delete_scram_credential(&self, username: &str, mechanism: i8) -> Result<()> {
        let subject = format!("SCRAM credential for {}", username);
        self.run(CatalogCommand::DeleteScram { username: username.to_string(), mechanism }, &subject)
            .await
            .map(|_| ())
    }

    async fn list_scram_credentials(&self) -> Result<Vec<ScramCredentialRecord>> {
        Ok(self.catalog.read().scram_credentials())
    }

    // ---- local state ---------------------------------------------------------

    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        self.local.persist_segment_metadata(metadata).await
    }

    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        self.local.get_segment_metadata(topic, segment_id).await
    }

    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        self.local.list_segments(topic, partition).await
    }

    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        self.local.delete_segment(topic, segment_id).await
    }

    async fn persist_parquet_segment(&self, metadata: ParquetSegmentMetadata) -> Result<()> {
        self.local.persist_parquet_segment(metadata).await
    }

    async fn get_parquet_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<Option<ParquetSegmentMetadata>> {
        self.local.get_parquet_segment(topic, partition, segment_id).await
    }

    async fn list_parquet_segments(&self, topic: &str, partition: Option<i32>) -> Result<Vec<ParquetSegmentMetadata>> {
        self.local.list_parquet_segments(topic, partition).await
    }

    async fn get_parquet_paths(&self, topic: &str) -> Result<Vec<String>> {
        self.local.get_parquet_paths(topic).await
    }

    async fn delete_parquet_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<()> {
        self.local.delete_parquet_segment(topic, partition, segment_id).await
    }

    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        self.local.create_consumer_group(metadata).await
    }

    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        self.local.get_consumer_group(group_id).await
    }

    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        self.local.update_consumer_group(metadata).await
    }

    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        self.commit_offsets(vec![offset])
            .await?
            .into_iter()
            .next()
            .unwrap_or(Ok(()))
    }

    async fn commit_offsets(&self, offsets: Vec<ConsumerOffset>) -> Result<Vec<Result<()>>> {
        let _gate = self.offsets_gate.read().await;
        let mode = self.offset_mode();
        if mode == OffsetMode::Catalog {
            return self.commit_offsets_to_catalog(offsets).await;
        }
        let mut results = Vec::with_capacity(offsets.len());
        for offset in &offsets {
            results.push(self.local.commit_offset(offset.clone()).await);
        }
        if mode == OffsetMode::Mirrored {
            // A commit the catalog refuses would be gone once offsets are read
            // from it, so the client hears of it now.
            for (result, mirrored) in results.iter_mut().zip(self.commit_offsets_to_catalog(offsets).await?) {
                if result.is_ok() {
                    *result = mirrored;
                }
            }
        }
        Ok(results)
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        if !self.offsets_in_catalog() {
            return self.local.get_consumer_offset(group_id, topic, partition).await;
        }
        Ok(self
            .catalog
            .read()
            .group_offset(group_id, topic, partition)
            .map(|o| consumer_offset(group_id, topic, partition, o)))
    }

    async fn list_consumer_offsets(&self, group_id: &str) -> Result<Vec<ConsumerOffset>> {
        if !self.offsets_in_catalog() {
            return self.local.list_consumer_offsets(group_id).await;
        }
        Ok(self
            .catalog
            .read()
            .group_offsets(group_id)
            .into_iter()
            .map(|(topic, partition, o)| consumer_offset(group_id, &topic, partition, &o))
            .collect())
    }

    async fn list_offset_groups(&self) -> Result<Vec<String>> {
        if !self.offsets_in_catalog() {
            return self.local.list_offset_groups().await;
        }
        Ok(self.catalog.read().offset_groups().into_iter().collect())
    }

    async fn commit_transactional_offsets(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>,
    ) -> Result<()> {
        // Committed as plain commits, as the local store does, so they follow
        // the offsets wherever they currently live.
        let _ = (transactional_id, producer_id, producer_epoch);
        let now = chrono::Utc::now();
        let batch = offsets
            .into_iter()
            .map(|(topic, partition, offset, metadata)| ConsumerOffset {
                group_id: group_id.clone(),
                topic,
                partition,
                offset,
                metadata,
                commit_timestamp: now,
            })
            .collect();
        for outcome in self.commit_offsets(batch).await? {
            outcome?;
        }
        Ok(())
    }


    async fn begin_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16, timeout_ms: i32) -> Result<()> {
        self.local.begin_transaction(transactional_id, producer_id, producer_epoch, timeout_ms).await
    }

    async fn add_partitions_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        partitions: Vec<(String, u32)>,
    ) -> Result<()> {
        self.local.add_partitions_to_transaction(transactional_id, producer_id, producer_epoch, partitions).await
    }

    async fn add_offsets_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
    ) -> Result<()> {
        self.local.add_offsets_to_transaction(transactional_id, producer_id, producer_epoch, group_id).await
    }

    async fn prepare_commit_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16) -> Result<()> {
        self.local.prepare_commit_transaction(transactional_id, producer_id, producer_epoch).await
    }

    async fn commit_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16) -> Result<()> {
        self.local.commit_transaction(transactional_id, producer_id, producer_epoch).await
    }

    async fn abort_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16) -> Result<()> {
        self.local.abort_transaction(transactional_id, producer_id, producer_epoch).await
    }

    async fn fence_producer(
        &self,
        transactional_id: String,
        old_producer_id: i64,
        old_producer_epoch: i16,
        new_producer_id: i64,
        new_producer_epoch: i16,
    ) -> Result<()> {
        self.local
            .fence_producer(transactional_id, old_producer_id, old_producer_epoch, new_producer_id, new_producer_epoch)
            .await
    }

    async fn allocate_producer_id(&self) -> Result<i64> {
        self.local.allocate_producer_id().await
    }

    async fn get_transaction(&self, transactional_id: &str) -> Result<Option<TransactionMetadata>> {
        self.local.get_transaction(transactional_id).await
    }

    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        self.local.update_partition_offset(topic, partition, high_watermark, log_start_offset).await
    }

    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        self.local.get_partition_offset(topic, partition).await
    }

    async fn update_log_start_offset(&self, topic: &str, partition: u32, log_start_offset: i64) -> Result<i64> {
        self.local.update_log_start_offset(topic, partition, log_start_offset).await
    }

    async fn list_consumer_groups(&self) -> Result<Vec<ConsumerGroupMetadata>> {
        self.local.list_consumer_groups().await
    }

    async fn delete_consumer_group(&self, group_id: &str) -> Result<()> {
        let _gate = self.offsets_gate.read().await;
        if self.offset_mode() != OffsetMode::Local {
            self.run(
                CatalogCommand::DeleteOffsets { group_id: group_id.to_string(), partitions: None },
                &format!("offsets of group {group_id}"),
            )
            .await?;
        }
        self.local.delete_consumer_group(group_id).await
    }

    async fn delete_consumer_offsets(&self, group_id: &str, topic_partitions: &[(String, u32)]) -> Result<()> {
        let _gate = self.offsets_gate.read().await;
        let mode = self.offset_mode();
        if mode != OffsetMode::Local {
            self.run(
                CatalogCommand::DeleteOffsets {
                    group_id: group_id.to_string(),
                    partitions: Some(topic_partitions.to_vec()),
                },
                &format!("offsets of group {group_id}"),
            )
            .await?;
        }
        if mode == OffsetMode::Catalog {
            return Ok(());
        }
        self.local.delete_consumer_offsets(group_id, topic_partitions).await
    }

    async fn init_system_state(&self) -> Result<()> {
        self.local.init_system_state().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_catalog::ClusterCatalogConfig;
    use chronik_common::metadata::WalAppendFn;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    fn local_store(node: u64) -> Arc<WalMetadataStore> {
        let append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0) }));
        Arc::new(WalMetadataStore::new(node, append))
    }

    fn broker(id: i32) -> BrokerMetadata {
        BrokerMetadata {
            broker_id: id,
            host: "localhost".into(),
            port: 9092 + id,
            rack: None,
            status: BrokerStatus::Online,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn topic_config(partitions: u32, rf: u32) -> TopicConfig {
        TopicConfig { partition_count: partitions, replication_factor: rf, ..TopicConfig::default() }
    }

    struct Node {
        store: Arc<CatalogMetadataStore>,
        _dir: TempDir,
    }

    async fn cluster(n: u64) -> Vec<Node> {
        let mut listeners = Vec::new();
        for _ in 0..n {
            listeners.push(TcpListener::bind("127.0.0.1:0").await.unwrap());
        }
        let peers: Vec<(u64, String)> =
            listeners.iter().enumerate().map(|(i, l)| (i as u64 + 1, l.local_addr().unwrap().to_string())).collect();
        let voters: Vec<u64> = (1..=n).collect();
        let mut nodes = Vec::new();
        for (i, listener) in listeners.into_iter().enumerate() {
            let id = i as u64 + 1;
            let dir = TempDir::new().unwrap();
            let mut cfg = ClusterCatalogConfig::new(id, dir.path().join("catalog"), peers.clone(), voters.clone());
            cfg.tick_interval = Duration::from_millis(20);
            cfg.election_tick = 10;
            cfg.heartbeat_tick = 2;
            cfg.propose_timeout = Duration::from_secs(10);
            let catalog = Arc::new(ClusterCatalog::start(cfg, tokio::runtime::Handle::current()).unwrap());
            let service = catalog.service();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(service)
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                    .await
                    .unwrap();
            });
            let store = Arc::new(CatalogMetadataStore::new(catalog.clone(), local_store(id)));
            let s = store.clone();
            let mut rx = catalog.subscribe();
            tokio::spawn(async move {
                while let Ok(c) = rx.recv().await {
                    s.handle_change(&c).await;
                }
            });
            nodes.push(Node { store, _dir: dir });
        }
        for n in &nodes {
            assert!(n.store.catalog().wait_caught_up(Duration::from_secs(10)).await, "node never caught up");
        }
        nodes
    }

    fn local_offset(group: &str, topic: &str, partition: u32, offset: i64, ts_ms: i64) -> ConsumerOffset {
        ConsumerOffset {
            group_id: group.into(),
            topic: topic.into(),
            partition,
            offset,
            metadata: None,
            commit_timestamp: chrono::DateTime::from_timestamp_millis(ts_ms).unwrap(),
        }
    }

    /// Catalog offsets turn on only when every voter supports them and has
    /// merged its pre-catalog offsets (the newer commit wins); from then on a
    /// commit through any node is read the same through every node.
    #[tokio::test(flavor = "multi_thread")]
    async fn group_offsets_move_into_the_catalog_once_every_node_supports_it() {
        let nodes = cluster(3).await;
        let s1 = &nodes[0].store;
        for id in 1..=3 {
            s1.register_broker(broker(id)).await.unwrap();
        }
        s1.create_topic("t", topic_config(2, 3)).await.unwrap();
        // Offsets committed per broker before the upgrade, disagreeing on t-0.
        nodes[0].store.local.commit_offset(local_offset("g", "t", 0, 10, 1_000)).await.unwrap();
        nodes[1].store.local.commit_offset(local_offset("g", "t", 0, 20, 2_000)).await.unwrap();
        nodes[2].store.local.commit_offset(local_offset("g", "t", 1, 5, 1_500)).await.unwrap();
        assert!(!s1.offsets_in_catalog());
        assert_eq!(s1.get_consumer_offset("g", "t", 0).await.unwrap().map(|o| o.offset), Some(10), "before: local");

        // Two of three nodes upgraded: the feature must stay off.
        nodes[0].store.spawn_group_offsets_rollout();
        nodes[1].store.spawn_group_offsets_rollout();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!s1.offsets_in_catalog(), "enabled before every voter declared it");

        nodes[2].store.spawn_group_offsets_rollout();
        let merged = |n: &Node| {
            let st = n.store.catalog().read();
            (1..=3).all(|id| st.declared(id, OFFSETS_MERGED))
        };
        // Never enabled while some node still holds offsets only locally.
        for _ in 0..2000 {
            for n in &nodes {
                if n.store.offsets_in_catalog() {
                    assert!(merged(n), "enabled before every voter merged its offsets");
                }
            }
            if nodes.iter().all(|n| n.store.offsets_in_catalog()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            nodes.iter().all(merged),
            "nodes did not all merge their offsets: {:?}",
            nodes
                .iter()
                .map(|n| { let st = n.store.catalog().read(); ((1..=3).map(|id| (st.declared(id, FEATURE_GROUP_OFFSETS), st.declared(id, OFFSETS_MERGED))).collect::<Vec<_>>(), n.store.offset_mode()) })
                .collect::<Vec<_>>()
        );
        for n in &nodes {
            assert!(n.store.offsets_in_catalog());
            assert_eq!(n.store.get_consumer_offset("g", "t", 0).await.unwrap().map(|o| o.offset), Some(20), "newer commit wins");
            assert_eq!(n.store.get_consumer_offset("g", "t", 1).await.unwrap().map(|o| o.offset), Some(5));
        }

        // A commit through node 3 reads the same through node 1.
        let results = nodes[2].store.commit_offsets(vec![local_offset("g", "t", 0, 30, 9_000)]).await.unwrap();
        assert!(results.iter().all(|r| r.is_ok()), "{results:?}");
        let mut seen = None;
        for _ in 0..50 {
            seen = s1.get_consumer_offset("g", "t", 0).await.unwrap().map(|o| o.offset);
            if seen == Some(30) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(seen, Some(30));
        assert_eq!(s1.list_offset_groups().await.unwrap(), vec!["g".to_string()]);
        let unknown = nodes[2].store.commit_offsets(vec![local_offset("g", "nope", 0, 1, 9_000)]).await.unwrap();
        assert!(unknown[0].is_err(), "a commit to an unknown topic was accepted");
    }

    /// A commit made while nodes are still merging (mirrored) is in the
    /// committing node's own store and in the catalog, so it is not lost when
    /// reads move to the catalog.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_commit_while_nodes_merge_survives_the_switch_to_the_catalog() {
        let nodes = cluster(3).await;
        let s1 = &nodes[0].store;
        for id in 1..=3 {
            s1.register_broker(broker(id)).await.unwrap();
        }
        s1.create_topic("t", topic_config(1, 3)).await.unwrap();
        for id in 1..=3u64 {
            s1.catalog()
                .execute(CatalogCommand::DeclareCapabilities { node_id: id, capabilities: vec![FEATURE_GROUP_OFFSETS.into()] })
                .await
                .unwrap();
        }
        s1.catalog().execute(CatalogCommand::EnableFeature { feature: FEATURE_GROUP_OFFSETS_MIRROR.into() }).await.unwrap();
        for _ in 0..50 {
            if nodes[1].store.offset_mode() == OffsetMode::Mirrored {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(nodes[1].store.offset_mode(), OffsetMode::Mirrored);

        let r = nodes[1].store.commit_offsets(vec![local_offset("g", "t", 0, 42, 5_000)]).await.unwrap();
        assert!(r.iter().all(|r| r.is_ok()), "{r:?}");
        assert_eq!(nodes[1].store.get_consumer_offset("g", "t", 0).await.unwrap().map(|o| o.offset), Some(42), "read locally");
        assert!(s1.get_consumer_offset("g", "t", 0).await.unwrap().is_none(), "node 1 still reads its own store");
        let r = nodes[1].store.commit_offsets(vec![local_offset("g", "nope", 0, 1, 5_000)]).await.unwrap();
        assert!(r[0].is_err(), "a commit the catalog refuses was reported as done");

        s1.catalog().execute(CatalogCommand::EnableFeature { feature: FEATURE_GROUP_OFFSETS.into() }).await.unwrap();
        let mut seen = None;
        for _ in 0..50 {
            seen = s1.get_consumer_offset("g", "t", 0).await.unwrap().map(|o| o.offset);
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(seen, Some(42), "the mirrored commit is in the catalog");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_node_topic_lifecycle() {
        let nodes = cluster(1).await;
        let s = &nodes[0].store;
        assert!(matches!(s.create_topic("t", topic_config(1, 1)).await, Err(MetadataError::StorageError(_))), "no brokers yet");
        s.register_broker(broker(1)).await.unwrap();
        let t = s.create_topic("t", topic_config(2, 1)).await.unwrap();
        assert_eq!(s.get_topic("t").await.unwrap().unwrap().id, t.id);
        assert!(matches!(s.create_topic("t", topic_config(1, 1)).await, Err(MetadataError::AlreadyExists(_))));
        assert_eq!(s.get_partition_assignments("t").await.unwrap().len(), 2);

        let mut grown = t.config.clone();
        grown.partition_count = 4;
        grown.config.insert("cleanup.policy".into(), "compact".into());
        let after = s.update_topic("t", grown).await.unwrap();
        assert_eq!(after.config.partition_count, 4);
        assert_eq!(after.config.config.get("cleanup.policy").map(String::as_str), Some("compact"));
        assert_eq!(s.get_partition_assignments("t").await.unwrap().len(), 4);

        // Local offsets are purged when the catalog deletes the topic.
        s.update_partition_offset("t", 0, 42, 0).await.unwrap();
        s.delete_topic("t").await.unwrap();
        assert!(s.get_topic("t").await.unwrap().is_none());
        assert!(matches!(s.delete_topic("t").await, Err(MetadataError::NotFound(_))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(s.get_partition_offset("t", 0).await.unwrap(), None);

        // Security caches are told about ACL changes.
        let mut gen = s.acl_changes().unwrap();
        let acl = AclBindingRecord {
            resource_type: 2,
            resource_name: "t".into(),
            pattern_type: 3,
            principal: "User:a".into(),
            host: "*".into(),
            operation: 3,
            permission_type: 3,
        };
        s.create_acl(acl.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), gen.changed()).await.unwrap().unwrap();
        assert_eq!(s.list_acls().await.unwrap(), vec![acl]);
    }

    /// A write on a follower is forwarded, and visible on that follower as
    /// soon as the call returns. Concurrent creates of the same names from
    /// every node create each name once, with one id everywhere.
    #[tokio::test(flavor = "multi_thread")]
    async fn three_nodes_forward_and_create_each_name_once() {
        let nodes = cluster(3).await;
        for id in 1..=3 {
            nodes[0].store.register_broker(broker(id)).await.unwrap();
        }
        let follower = nodes.iter().find(|n| !n.store.catalog().is_leader()).unwrap();
        let t = follower.store.create_topic("from-follower", topic_config(3, 3)).await.unwrap();
        assert_eq!(follower.store.get_topic("from-follower").await.unwrap().unwrap().id, t.id, "read your writes");

        let mut tasks = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            for k in 0..30 {
                let s = n.store.clone();
                tasks.push(tokio::spawn(async move {
                    let name = format!("race-{}", (k + i) % 10);
                    (name.clone(), s.create_topic(&name, topic_config(1, 3)).await)
                }));
            }
        }
        let mut created = std::collections::HashMap::<String, usize>::new();
        for t in tasks {
            let (name, r) = t.await.unwrap();
            match r {
                Ok(_) => *created.entry(name).or_default() += 1,
                Err(MetadataError::AlreadyExists(_)) => {}
                Err(e) => panic!("{}: {}", name, e),
            }
        }
        assert_eq!(created.len(), 10);
        assert!(created.values().all(|&c| c == 1), "{:?}", created);

        // A rule created through one node reaches every node's store, and each
        // node's security watch fires (its authorizer reloads).
        let mut watches: Vec<_> = nodes.iter().map(|n| n.store.acl_changes().unwrap()).collect();
        let acl = AclBindingRecord {
            resource_type: 2,
            resource_name: "orders".into(),
            pattern_type: 3,
            principal: "User:alice".into(),
            host: "*".into(),
            operation: 3,
            permission_type: 3,
        };
        follower.store.create_acl(acl.clone()).await.unwrap();
        for (i, w) in watches.iter_mut().enumerate() {
            tokio::time::timeout(Duration::from_secs(10), w.changed()).await.unwrap_or_else(|_| panic!("node {} not told", i + 1)).unwrap();
            assert_eq!(nodes[i].store.list_acls().await.unwrap(), vec![acl.clone()], "node {}", i + 1);
        }

        // Same catalog, ids included, on every node.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let views: Vec<Vec<(String, uuid::Uuid)>> = futures::future::join_all(nodes.iter().map(|n| async {
                let mut v: Vec<_> = n.store.list_topics().await.unwrap().into_iter().map(|t| (t.name, t.id)).collect();
                v.sort();
                v
            }))
            .await;
            if views.iter().all(|v| v == &views[0]) && views[0].len() == 11 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "catalogs did not converge: {:?}", views.iter().map(|v| v.len()).collect::<Vec<_>>());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Group and transaction coordination from the catalog: off until
/// [`FEATURE_GROUP_OFFSETS`] is enabled; then the catalog leader coordinates.
pub struct CatalogCoordination {
    catalog: Arc<ClusterCatalog>,
}

impl CatalogCoordination {
    pub fn new(catalog: Arc<ClusterCatalog>) -> Self {
        Self { catalog }
    }
}

impl crate::consumer_group::Coordination for CatalogCoordination {
    fn is_local(&self) -> Option<bool> {
        if !self.catalog.read().has_feature(FEATURE_GROUP_OFFSETS) {
            return None;
        }
        Some(self.catalog.am_i_leader())
    }

    fn coordinator(&self) -> Option<(i32, String, i32)> {
        if !self.catalog.read().has_feature(FEATURE_GROUP_OFFSETS) {
            return None;
        }
        let leader = self.catalog.leader_id()?;
        let state = self.catalog.read();
        let broker = state.broker(leader as i32)?;
        Some((broker.broker_id, broker.host.clone(), broker.port))
    }
}
