//! Partition assignment persistence.
//!
//! Partition assignments are the input to leader election and to follower-pull
//! replication: a broker that comes back up and disagrees about who leads a
//! partition will either refuse to serve it or serve it from the wrong replica.
//! So the property under test is that an assignment written before a restart is
//! the assignment read after one — reconstructed from the metadata event log,
//! not from in-process state.
//!
//! Previously written against `ChronikMetaLogStore` + `MetaLogWalInterface`,
//! neither of which still exists; the store is `WalMetadataStore`, which takes
//! an append callback and rebuilds itself by replaying the events that callback
//! recorded.

use chronik_common::metadata::{
    MetadataEvent, MetadataStore, PartitionAssignment, TopicConfig, WalMetadataStore,
};
use parking_lot::RwLock;
use std::sync::Arc;

/// Stands in for the WAL: keeps the serialized event bytes so a second store can
/// replay them, which is what makes the restart test a real one.
#[derive(Clone, Default)]
struct EventLog {
    records: Arc<RwLock<Vec<Vec<u8>>>>,
}

impl EventLog {
    fn append_fn(&self) -> chronik_common::metadata::WalAppendFn {
        let records = self.records.clone();
        Arc::new(move |bytes: Vec<u8>| {
            let records = records.clone();
            Box::pin(async move {
                let mut guard = records.write();
                guard.push(bytes);
                Ok(guard.len() as i64 - 1)
            })
        })
    }

    /// Decode everything appended so far, in order.
    fn events(&self) -> Vec<MetadataEvent> {
        self.records
            .read()
            .iter()
            .map(|bytes| {
                serde_json::from_slice::<MetadataEvent>(bytes)
                    .expect("metadata event should round-trip through the WAL encoding")
            })
            .collect()
    }
}

fn store_with(log: &EventLog) -> WalMetadataStore {
    WalMetadataStore::new(1, log.append_fn())
}

async fn create_topic(store: &WalMetadataStore, name: &str, partitions: u32) {
    store
        .create_topic(
            name,
            TopicConfig {
                partition_count: partitions,
                replication_factor: 1,
                ..Default::default()
            },
        )
        .await
        .expect("create_topic");
}

#[tokio::test]
async fn test_partition_assignment_persistence() {
    let log = EventLog::default();
    let store = store_with(&log);

    create_topic(&store, "test-topic", 3).await;

    // Alternate the leader between nodes 1 and 2.
    for partition in 0..3u32 {
        let leader = (partition % 2 + 1) as u64;
        store
            .assign_partition(PartitionAssignment {
                topic: "test-topic".to_string(),
                partition,
                broker_id: leader as i32,
                is_leader: true,
                replicas: vec![leader],
                leader_id: leader,
                leader_epoch: 0,
                isr: vec![leader],
            })
            .await
            .expect("assign_partition");
    }

    let assignments = store
        .get_partition_assignments("test-topic")
        .await
        .expect("get_partition_assignments");
    assert_eq!(assignments.len(), 3);

    assert_eq!(store.get_partition_leader("test-topic", 0).await.unwrap(), Some(1));
    assert_eq!(store.get_partition_leader("test-topic", 1).await.unwrap(), Some(2));
    assert_eq!(store.get_partition_leader("test-topic", 2).await.unwrap(), Some(1));

    assert_eq!(
        store.get_partition_replicas("test-topic", 0).await.unwrap(),
        Some(vec![1])
    );
    assert_eq!(
        store.get_partition_replicas("test-topic", 1).await.unwrap(),
        Some(vec![2])
    );
}

#[tokio::test]
async fn test_partition_assignment_survives_restart() {
    let log = EventLog::default();

    // Write assignments, then drop the store — the process is gone, only the
    // event log survives.
    {
        let store = store_with(&log);
        create_topic(&store, "persistent-topic", 2).await;

        for partition in 0..2u32 {
            store
                .assign_partition(PartitionAssignment {
                    topic: "persistent-topic".to_string(),
                    partition,
                    broker_id: 5,
                    is_leader: true,
                    replicas: vec![5],
                    leader_id: 5,
                    leader_epoch: 0,
                    isr: vec![5],
                })
                .await
                .expect("assign_partition");
        }
    }

    // Restart: a fresh store replays what was written.
    //
    // The old version of this test asserted
    // `leader.is_none() || leader == Some(5)`, which holds for every possible
    // outcome — it could not fail, and it did not test persistence.
    let recovered = store_with(&log);
    recovered.replay_events(log.events()).await.expect("replay_events");

    assert_eq!(
        recovered.get_partition_leader("persistent-topic", 0).await.unwrap(),
        Some(5),
        "partition 0 lost its leader across restart"
    );
    assert_eq!(
        recovered.get_partition_leader("persistent-topic", 1).await.unwrap(),
        Some(5),
        "partition 1 lost its leader across restart"
    );
    assert_eq!(
        recovered.get_partition_replicas("persistent-topic", 0).await.unwrap(),
        Some(vec![5]),
        "partition 0 lost its replica set across restart"
    );
}

#[tokio::test]
async fn test_partition_assignment_with_no_assignments() {
    let log = EventLog::default();
    let store = store_with(&log);

    assert_eq!(store.get_partition_leader("nonexistent", 0).await.unwrap(), None);
    assert_eq!(store.get_partition_replicas("nonexistent", 0).await.unwrap(), None);
}
