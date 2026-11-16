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
    AssignmentChange, CatalogChange, CatalogCommand, CatalogError, CatalogResult, ConfigChange,
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
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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
    pub fn new(catalog: Arc<ClusterCatalog>, local: Arc<WalMetadataStore>) -> Self {
        let (security_generation, _) = watch::channel(0);
        Self { catalog, local, security_generation }
    }

    pub fn catalog(&self) -> &Arc<ClusterCatalog> {
        &self.catalog
    }

    pub fn local(&self) -> &Arc<WalMetadataStore> {
        &self.local
    }

    /// Run a command and fail on a refused one.
    async fn run(&self, cmd: CatalogCommand, subject: &str) -> Result<CatalogResult> {
        match self.catalog.execute(cmd).await? {
            CatalogResult::Err(e) => Err(to_metadata_error(e, subject)),
            ok => Ok(ok),
        }
    }

    /// Apply a batch of assignment changes; the first refused item fails.
    pub async fn set_assignments(&self, changes: Vec<AssignmentChange>) -> Result<Vec<std::result::Result<(), CatalogError>>> {
        match self.catalog.execute(CatalogCommand::SetAssignments { changes }).await? {
            CatalogResult::Items(items) => Ok(items),
            CatalogResult::Err(e) => Err(to_metadata_error(e, "assignments")),
            other => Err(MetadataError::StorageError(format!("unexpected result {:?}", other))),
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

    async fn apply_replicated_event(&self, _event: MetadataEvent) -> Result<()> {
        Err(MetadataError::StorageError(
            "metadata events are no longer replicated; the catalog travels in its raft log".into(),
        ))
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
        self.local.commit_offset(offset).await
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        self.local.get_consumer_offset(group_id, topic, partition).await
    }

    async fn commit_transactional_offsets(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>,
    ) -> Result<()> {
        self.local
            .commit_transactional_offsets(transactional_id, producer_id, producer_epoch, group_id, offsets)
            .await
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
        self.local.delete_consumer_group(group_id).await
    }

    async fn delete_consumer_offsets(&self, group_id: &str, topic_partitions: &[(String, u32)]) -> Result<()> {
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
