//! One-time import of pre-catalog metadata.
//!
//! Before the catalog moved to Raft, every node kept topics, assignments,
//! brokers, ACLs and SCRAM credentials in its own metadata WAL. When such a
//! node first boots with the Raft catalog, that WAL still holds them — but the
//! catalog starts empty, and without this step the cluster would forget every
//! topic it has.
//!
//! Each node imports what its own WAL holds, once (a marker file records it).
//! Imports from several nodes merge in the catalog (`ImportTopics`): a topic
//! not yet present is added; for one that is, each partition keeps the
//! assignment with the higher leader epoch. The result is the union of the
//! nodes' views — a topic one node still held after the others deleted it
//! comes back and is logged, which is preferable to losing a topic with data.
//!
//! Until its import has finished the node does not count as caught up, so it
//! does not auto-create a topic that the import is about to bring in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chronik_common::metadata::catalog::{CatalogCommand, CatalogResult, ImportedTopic};
use chronik_common::metadata::{
    AclBindingRecord, BrokerMetadata, MetadataStore, ScramCredentialRecord, WalMetadataStore,
};
use tracing::{info, warn};

use crate::cluster_catalog::ClusterCatalog;

/// Marker written into the catalog directory once this node has imported.
pub const MARKER: &str = "legacy-import.done";
const BATCH: usize = 200;

/// What a node's pre-catalog metadata holds.
#[derive(Debug, Default)]
pub struct Legacy {
    pub brokers: Vec<BrokerMetadata>,
    pub acls: Vec<AclBindingRecord>,
    pub scram: Vec<ScramCredentialRecord>,
    pub topics: Vec<ImportedTopic>,
}

impl Legacy {
    pub fn is_empty(&self) -> bool {
        self.brokers.is_empty() && self.acls.is_empty() && self.scram.is_empty() && self.topics.is_empty()
    }
}

/// Read the catalog-shaped state out of the node's local metadata store.
pub async fn read_legacy(local: &WalMetadataStore) -> Legacy {
    let mut legacy = Legacy {
        brokers: local.list_brokers().await.unwrap_or_default(),
        acls: local.list_acls().await.unwrap_or_default(),
        scram: local.list_scram_credentials().await.unwrap_or_default(),
        topics: Vec::new(),
    };
    for t in local.list_topics().await.unwrap_or_default() {
        let assignments = local.get_partition_assignments(&t.name).await.unwrap_or_default();
        legacy.topics.push(ImportedTopic {
            name: t.name,
            config: t.config,
            auto_created: t.auto_created,
            created_at_ms: t.created_at.timestamp_millis(),
            assignments,
        });
    }
    legacy.topics.sort_by(|a, b| a.name.cmp(&b.name));
    legacy
}

/// Counts of what one import added to the catalog.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub topics_added: usize,
    pub topics_merged: usize,
    pub topics_refused: usize,
    pub brokers: usize,
    pub acls: usize,
    pub scram: usize,
}

/// Propose the import. Idempotent: running it twice changes nothing more.
pub async fn import(catalog: &ClusterCatalog, legacy: Legacy) -> Result<ImportReport, String> {
    let mut report = ImportReport::default();
    for b in legacy.brokers {
        if catalog.read().broker(b.broker_id).is_none() {
            expect_ok(catalog, CatalogCommand::RegisterBroker { broker: b }).await?;
            report.brokers += 1;
        }
    }
    let existing_acls = catalog.read().acls();
    for binding in legacy.acls {
        if !existing_acls.contains(&binding) {
            expect_ok(catalog, CatalogCommand::CreateAcl { binding }).await?;
            report.acls += 1;
        }
    }
    let existing_scram = catalog.read().scram_credentials();
    for credential in legacy.scram {
        if !existing_scram.iter().any(|c| c.username == credential.username && c.mechanism == credential.mechanism) {
            expect_ok(catalog, CatalogCommand::UpsertScram { credential }).await?;
            report.scram += 1;
        }
    }
    for chunk in legacy.topics.chunks(BATCH) {
        let (added, merged): (Vec<&ImportedTopic>, Vec<&ImportedTopic>) = {
            let state = catalog.read();
            chunk.iter().partition(|t| state.topic(&t.name).is_none())
        };
        match catalog
            .execute(CatalogCommand::ImportTopics { topics: chunk.to_vec() })
            .await
            .map_err(|e| e.to_string())?
        {
            CatalogResult::Items(items) => {
                for (t, outcome) in chunk.iter().zip(items) {
                    if let Err(e) = outcome {
                        warn!("Catalog import: '{}' refused: {}", t.name, e);
                        report.topics_refused += 1;
                    }
                }
            }
            other => return Err(format!("unexpected import result {:?}", other)),
        }
        report.topics_added += added.len();
        report.topics_merged += merged.len();
    }
    Ok(report)
}

async fn expect_ok(catalog: &ClusterCatalog, cmd: CatalogCommand) -> Result<(), String> {
    match catalog.execute(cmd).await.map_err(|e| e.to_string())? {
        CatalogResult::Err(e) => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// Import this node's pre-catalog metadata once, in the background. The node
/// counts as not caught up until it is done.
pub fn spawn(catalog: Arc<ClusterCatalog>, local: Arc<WalMetadataStore>, catalog_dir: PathBuf) {
    let marker = catalog_dir.join(MARKER);
    if marker.exists() {
        return;
    }
    catalog.set_import_pending(true);
    tokio::spawn(async move {
        let legacy = read_legacy(&local).await;
        if legacy.is_empty() {
            info!("Catalog import: no pre-catalog metadata on this node");
        } else {
            info!(
                "Catalog import: this node's pre-catalog metadata holds {} topic(s), {} broker(s), {} ACL(s), {} SCRAM credential(s)",
                legacy.topics.len(),
                legacy.brokers.len(),
                legacy.acls.len(),
                legacy.scram.len()
            );
            let mut backoff = Duration::from_secs(1);
            loop {
                while !catalog.wait_raft_caught_up(Duration::from_secs(30)).await {
                    info!("Catalog import waiting for the catalog to catch up");
                }
                match import(&catalog, read_legacy(&local).await).await {
                    Ok(r) => {
                        info!(
                            "Catalog import complete: {} topic(s) added, {} merged with another node's import, {} refused; {} broker(s), {} ACL(s), {} SCRAM credential(s)",
                            r.topics_added, r.topics_merged, r.topics_refused, r.brokers, r.acls, r.scram
                        );
                        break;
                    }
                    Err(e) => {
                        warn!("Catalog import failed, retrying in {:?}: {}", backoff, e);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }
        if let Err(e) = std::fs::write(&marker, b"imported\n") {
            warn!("Catalog import: could not write {}: {} (the import will run again next boot; it is idempotent)", marker.display(), e);
        }
        catalog.set_import_pending(false);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_catalog::ClusterCatalogConfig;
    use chronik_common::metadata::{BrokerStatus, PartitionAssignment, TopicConfig, WalAppendFn};
    use tempfile::TempDir;

    fn local(node: u64) -> Arc<WalMetadataStore> {
        let append: WalAppendFn = Arc::new(|_| Box::pin(async { Ok(0) }));
        Arc::new(WalMetadataStore::new(node, append))
    }

    async fn topic_with(store: &WalMetadataStore, name: &str, partitions: u32, leader: u64, moves: u32) {
        let config = TopicConfig { partition_count: partitions, replication_factor: 3, ..TopicConfig::default() };
        store.create_topic(name, config).await.unwrap();
        for p in 0..partitions {
            // Each leader change bumps the epoch in the legacy store.
            for m in 0..=moves {
                let l = if m % 2 == 0 { leader } else { leader % 3 + 1 };
                store
                    .assign_partition(PartitionAssignment {
                        topic: name.into(),
                        partition: p,
                        broker_id: l as i32,
                        is_leader: true,
                        replicas: vec![1, 2, 3],
                        leader_id: l,
                        leader_epoch: 0,
                        isr: vec![1, 2, 3],
                    })
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_nodes_import_into_one_catalog() {
        let dir = TempDir::new().unwrap();
        let mut cfg = ClusterCatalogConfig::new(1, dir.path().join("c"), vec![(1, "127.0.0.1:0".into())], vec![1]);
        cfg.tick_interval = Duration::from_millis(20);
        cfg.election_tick = 10;
        cfg.heartbeat_tick = 2;
        let catalog = ClusterCatalog::start(cfg, tokio::runtime::Handle::current()).unwrap();
        assert!(catalog.wait_raft_caught_up(Duration::from_secs(10)).await);

        // Node A: "orders" at epoch 2, "only-a". Node B: "orders" at epoch 4, "only-b".
        let a = local(1);
        a.register_broker(BrokerMetadata {
            broker_id: 1,
            host: "h1".into(),
            port: 9092,
            rack: None,
            status: BrokerStatus::Online,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
        topic_with(&a, "orders", 2, 1, 2).await;
        topic_with(&a, "only-a", 1, 1, 0).await;
        let b = local(2);
        topic_with(&b, "orders", 2, 2, 4).await;
        topic_with(&b, "only-b", 3, 3, 0).await;
        let epoch_a = a.get_partition_assignments("orders").await.unwrap()[0].leader_epoch;
        let epoch_b = b.get_partition_assignments("orders").await.unwrap()[0].leader_epoch;
        assert!(epoch_b > epoch_a, "{} vs {}", epoch_b, epoch_a);

        catalog.set_import_pending(true);
        assert!(!catalog.is_caught_up(), "the gate holds while an import is pending");
        let ra = import(&catalog, read_legacy(&a).await).await.unwrap();
        assert_eq!((ra.topics_added, ra.topics_merged, ra.brokers), (2, 0, 1));
        let rb = import(&catalog, read_legacy(&b).await).await.unwrap();
        assert_eq!((rb.topics_added, rb.topics_merged), (1, 1));
        catalog.set_import_pending(false);
        assert!(catalog.is_caught_up());

        let b_assign = b.get_partition_assignments("orders").await.unwrap();
        {
            let state = catalog.read();
            let mut names: Vec<&str> = state.topics().map(|t| t.name.as_str()).collect();
            names.sort();
            assert_eq!(names, vec!["only-a", "only-b", "orders"]);
            // The higher epoch won, and the assignment that came with it.
            let o = state.assignment("orders", 0).unwrap();
            assert_eq!(o.leader_epoch, epoch_b);
            assert_eq!(o.leader_id, b_assign.iter().find(|x| x.partition == 0).unwrap().leader_id);
            assert_eq!(state.topic("only-b").unwrap().config.partition_count, 3);
            assert_eq!(state.assignments_for("only-b").len(), 3);
        }

        // Idempotent.
        let again = import(&catalog, read_legacy(&a).await).await.unwrap();
        assert_eq!((again.topics_added, again.brokers, again.acls, again.scram), (0, 0, 0, 0));
    }
}
