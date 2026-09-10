//! Consumer group coordination tests

#[path = "common.rs"]
mod common;
#[path = "test_setup.rs"]
mod test_setup;

use common::*;
use chronik_common::Result;
use rdkafka::{
    ClientConfig,
    Message,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    consumer::{Consumer, StreamConsumer, CommitMode},
    producer::{FutureProducer, FutureRecord},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

/// The partitions the group coordinator actually assigned to this consumer.
///
/// These tests used to infer assignment by polling for messages and recording
/// which partitions they arrived from. That measures data flow, not assignment:
/// a consumer holding partitions whose records an earlier phase already drained
/// records zero partitions and the test fails on a broker that did nothing
/// wrong. `assignment()` asks the client what SyncGroup handed it, which is the
/// coordinator behaviour under test.
fn assigned_partitions(consumer: &StreamConsumer) -> HashSet<i32> {
    use rdkafka::consumer::Consumer as _;
    consumer
        .assignment()
        .expect("assignment() failed")
        .elements()
        .iter()
        .map(|e| e.partition())
        .collect()
}

/// Poll each consumer briefly so librdkafka services the group protocol, then
/// wait for every partition to be claimed exactly once.
async fn await_stable_assignment(
    consumers: &[(&str, &StreamConsumer)],
    expected_partitions: usize,
) -> HashMap<String, HashSet<i32>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);

    loop {
        // recv() drives the client's background work, including rebalance.
        for (_, consumer) in consumers {
            let _ = timeout(Duration::from_millis(200), consumer.recv()).await;
        }

        let assignments: HashMap<String, HashSet<i32>> = consumers
            .iter()
            .map(|(name, consumer)| (name.to_string(), assigned_partitions(consumer)))
            .collect();

        let total: usize = assignments.values().map(|p| p.len()).sum();
        let union: HashSet<i32> = assignments.values().flatten().copied().collect();

        // Stable means: every partition claimed, and none claimed twice.
        if union.len() == expected_partitions && total == expected_partitions {
            return assignments;
        }

        if std::time::Instant::now() >= deadline {
            panic!(
                "group never reached a stable assignment of {} partitions: {:?}",
                expected_partitions, assignments
            );
        }
    }
}

#[tokio::test]
async fn test_consumer_group_rebalance() -> Result<()> {
    test_setup::init();
    let _serial = common::exclusive().await;
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic with multiple partitions
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-rebalance", 6, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .as_ref().expect("Failed to create topic");
    
    // Produce test data
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    for i in 0..60 {
        producer
            .send(
                FutureRecord::to("test-rebalance")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i))
                    .partition(i % 6),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Track which partitions are assigned to which consumer
    let partition_assignments = Arc::new(Mutex::new(HashMap::new()));
    
    // Start first consumer
    let consumer1: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "rebalance-test-group")
        .set("client.id", "consumer-1")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer1
        .subscribe(&["test-rebalance"])
        .expect("Failed to subscribe");
    
    // Let first consumer stabilize
    sleep(Duration::from_secs(2)).await;
    
    // Verify first consumer gets all partitions
    let mut consumer1_partitions = HashSet::new();
    let start = std::time::Instant::now();
    
    while consumer1_partitions.len() < 6 && start.elapsed() < Duration::from_secs(5) {
        match timeout(Duration::from_millis(100), consumer1.recv()).await {
            Ok(Ok(message)) => {
                consumer1_partitions.insert(message.partition());
            }
            _ => continue,
        }
    }
    
    assert_eq!(consumer1_partitions.len(), 6, "First consumer should get all partitions");
    
    // Start second consumer - should trigger rebalance
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "rebalance-test-group")
        .set("client.id", "consumer-2")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-rebalance"])
        .expect("Failed to subscribe");
    
    // Wait for rebalance
    sleep(Duration::from_secs(3)).await;
    
    // Verify partitions are distributed
    let mut assignments = partition_assignments.lock().await;
    assignments.clear();

    let two = [("consumer-1", &consumer1), ("consumer-2", &consumer2)];
    let two_way = await_stable_assignment(&two, 6).await;
    assignments.extend(two_way.clone());

    // Verify each consumer has some partitions
    assert!(!two_way["consumer-1"].is_empty(), "consumer-1 got no partitions");
    assert!(!two_way["consumer-2"].is_empty(), "consumer-2 got no partitions");

    // Verify no partition overlap
    assert!(
        two_way["consumer-1"].is_disjoint(&two_way["consumer-2"]),
        "partitions assigned to both consumers: {:?} / {:?}",
        two_way["consumer-1"],
        two_way["consumer-2"]
    );
    
    // Start third consumer
    let consumer3: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "rebalance-test-group")
        .set("client.id", "consumer-3")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer3
        .subscribe(&["test-rebalance"])
        .expect("Failed to subscribe");
    
    // Wait for rebalance
    sleep(Duration::from_secs(3)).await;
    
    // Verify partitions are re-distributed among three consumers
    assignments.clear();

    let three = [
        ("consumer-1", &consumer1),
        ("consumer-2", &consumer2),
        ("consumer-3", &consumer3),
    ];
    let three_way = await_stable_assignment(&three, 6).await;
    assignments.extend(three_way.clone());

    // 6 partitions over 3 consumers: an even split is 2 each, and no correct
    // assignor strays outside 1..=3.
    for (name, partitions) in three_way.iter() {
        assert!(
            (1..=3).contains(&partitions.len()),
            "{} has {} of 6 partitions across 3 consumers: {:?}",
            name,
            partitions.len(),
            three_way
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_consumer_group_offset_commit() -> Result<()> {
    test_setup::init();
    let _serial = common::exclusive().await;
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-offset-commit", 3, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .as_ref().expect("Failed to create topic");
    
    // Produce messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    for i in 0..30 {
        producer
            .send(
                FutureRecord::to("test-offset-commit")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i))
                    .partition(i % 3),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // First consumer - process and commit offsets
    let consumer1: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "offset-test-group")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer1
        .subscribe(&["test-offset-commit"])
        .expect("Failed to subscribe");
    
    // Process first 15 messages
    let mut consumed_first = HashSet::new();
    let mut offsets_to_commit = HashMap::new();

    while consumed_first.len() < 15 {
        match timeout(Duration::from_secs(1), consumer1.recv()).await {
            Ok(Ok(message)) => {
                let partition = message.partition();
                let offset = message.offset();

                // Track highest offset per partition
                offsets_to_commit.insert(partition, offset + 1);
                consumed_first.insert(
                    message.key_view::<str>().unwrap().unwrap().to_string(),
                );

                // Store offset for commit
                consumer1.store_offset_from_message(&message)
                    .expect("Failed to store offset");
            }
            _ => continue,
        }
    }
    
    // Commit offsets
    consumer1.commit_consumer_state(CommitMode::Sync)
        .expect("Failed to commit offsets");
    
    drop(consumer1);
    
    // Second consumer - should start from committed offsets
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "offset-test-group")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-offset-commit"])
        .expect("Failed to subscribe");
    
    // Resume from the committed positions.
    //
    // The messages were produced round-robin across 3 partitions, so the first
    // 15 the group consumed are NOT keys 0-14 — they are whichever 15 arrived
    // first across three independent logs. Asserting "consumer 2 sees keys
    // 15-29", as this test used to, encodes a single-log assumption that a
    // partitioned topic never satisfies.
    //
    // The property that offset commit actually guarantees is the one checked
    // here: across the commit and the consumer restart, the group sees every
    // record exactly once — nothing redelivered, nothing skipped.
    let mut received = Vec::new();
    let start = std::time::Instant::now();

    // Generous window: consumer 1 has just left, so the group must complete a
    // rebalance before consumer 2 owns all three partitions. A short window here
    // measures rebalance latency, not offset-commit correctness.
    while received.len() < 15 && start.elapsed() < Duration::from_secs(45) {
        match timeout(Duration::from_secs(1), consumer2.recv()).await {
            Ok(Ok(message)) => {
                let key = message.key_view::<str>().unwrap().unwrap();
                received.push(key.to_string());
            }
            _ => continue,
        }
    }

    assert_eq!(
        received.len(),
        15,
        "resumed consumer got {} of the 15 uncommitted records",
        received.len()
    );

    for key in &received {
        assert!(
            !consumed_first.contains(key),
            "{} was redelivered after being committed",
            key
        );
    }

    let all_keys: HashSet<String> = (0..30).map(|i| format!("key-{}", i)).collect();
    let seen: HashSet<String> = consumed_first
        .iter()
        .cloned()
        .chain(received.iter().cloned())
        .collect();
    let missing: Vec<_> = all_keys.difference(&seen).collect();
    assert!(missing.is_empty(), "records skipped across the commit: {:?}", missing);
    
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_failure_handling() -> Result<()> {
    test_setup::init();
    let _serial = common::exclusive().await;
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-failure", 4, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .as_ref().expect("Failed to create topic");
    
    // Produce messages continuously
    let producer_handle = tokio::spawn(async move {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .create()
            .expect("Failed to create producer");
        
        let mut i = 0;
        loop {
            let _ = producer
                .send(
                    FutureRecord::to("test-failure")
                        .key(&format!("key-{}", i))
                        .payload(&format!("value-{}", i)),
                    Duration::from_secs(1),
                )
                .await;
            
            i += 1;
            sleep(Duration::from_millis(100)).await;
        }
    });
    
    // Start two consumers
    let consumer1: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", cluster.bootstrap_servers())
        .set("group.id", "failure-test-group")
        .set("client.id", "consumer-1")
        .set("session.timeout.ms", "10000")
        .set("heartbeat.interval.ms", "3000")
        .set("enable.auto.commit", "true")
        .set("auto.commit.interval.ms", "1000")
        .set("auto.offset.reset", "latest")
        .create()
        .expect("Failed to create consumer");
    
    consumer1
        .subscribe(&["test-failure"])
        .expect("Failed to subscribe");
    
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", cluster.bootstrap_servers())
        .set("group.id", "failure-test-group")
        .set("client.id", "consumer-2")
        .set("session.timeout.ms", "10000")
        .set("heartbeat.interval.ms", "3000")
        .set("enable.auto.commit", "true")
        .set("auto.commit.interval.ms", "1000")
        .set("auto.offset.reset", "latest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-failure"])
        .expect("Failed to subscribe");
    
    // Let them stabilize
    sleep(Duration::from_secs(2)).await;
    
    // Track partition ownership
    let partitions_c1 = Arc::new(Mutex::new(HashSet::new()));
    let partitions_c2 = Arc::new(Mutex::new(HashSet::new()));
    
    // Consumer 1 processing
    let p1 = partitions_c1.clone();
    let c1_handle = tokio::spawn(async move {
        for _ in 0..50 {
            match timeout(Duration::from_millis(100), consumer1.recv()).await {
                Ok(Ok(message)) => {
                    p1.lock().await.insert(message.partition());
                }
                _ => continue,
            }
        }
        // Simulate failure by dropping consumer
        drop(consumer1);
    });
    
    // Consumer 2 processing.
    //
    // It keeps polling past consumer 1's departure and reports its OWN final
    // assignment, since the surviving consumer taking over the dead one's
    // partitions is the property under test. Counting which partitions its
    // messages came from can't distinguish "not assigned" from "assigned but
    // idle", and this topic is fed by a background producer whose records may
    // land anywhere.
    //
    // The window must outlast session.timeout.ms (10s) — the group cannot
    // declare consumer 1 dead before its session expires.
    let p2 = partitions_c2.clone();
    let c2_handle = tokio::spawn(async move {
        let mut message_count = 0;
        let start = std::time::Instant::now();

        while start.elapsed() < Duration::from_secs(25) {
            match timeout(Duration::from_millis(100), consumer2.recv()).await {
                Ok(Ok(message)) => {
                    p2.lock().await.insert(message.partition());
                    message_count += 1;
                }
                _ => continue,
            }

            // Stop early once the takeover has happened.
            if start.elapsed() > Duration::from_secs(12)
                && assigned_partitions(&consumer2).len() == 4
            {
                break;
            }
        }

        (message_count, assigned_partitions(&consumer2))
    });

    // Wait for consumer 1 to "fail"
    c1_handle.await.unwrap();

    // Consumer 2 should now hold all partitions
    let (final_count, c2_assignment) = c2_handle.await.unwrap();

    assert_eq!(
        c2_assignment.len(),
        4,
        "consumer 2 should hold all 4 partitions after consumer 1 dropped, holds {:?}",
        c2_assignment
    );
    assert!(final_count > 0, "Consumer 2 should continue processing after rebalance");
    
    producer_handle.abort();
    
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_incremental_rebalance() -> Result<()> {
    test_setup::init();
    let _serial = common::exclusive().await;
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-incremental", 8, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .as_ref().expect("Failed to create topic");
    
    // Produce initial data
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    for i in 0..80 {
        producer
            .send(
                FutureRecord::to("test-incremental")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i)),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Create consumers with cooperative-sticky assignment
    let create_consumer = |client_id: &str| -> StreamConsumer {
        ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .set("group.id", "incremental-test-group")
            .set("client.id", client_id)
            .set("partition.assignment.strategy", "cooperative-sticky")
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest")
            .create()
            .expect("Failed to create consumer")
    };
    
    // Start consumers gradually
    let mut consumers = Vec::new();
    let mut partition_history = Vec::new();
    
    for i in 0..4 {
        let consumer = create_consumer(&format!("consumer-{}", i));
        consumer
            .subscribe(&["test-incremental"])
            .expect("Failed to subscribe");
        
        consumers.push(consumer);
        
        // Wait for rebalance
        sleep(Duration::from_secs(2)).await;
        
        // Record partition assignments
        let mut current_assignments = HashMap::new();
        
        for (j, consumer) in consumers.iter().enumerate() {
            let mut partitions = HashSet::new();
            let start = std::time::Instant::now();
            
            while start.elapsed() < Duration::from_secs(1) {
                match timeout(Duration::from_millis(50), consumer.recv()).await {
                    Ok(Ok(message)) => {
                        partitions.insert(message.partition());
                    }
                    _ => continue,
                }
            }
            
            if !partitions.is_empty() {
                current_assignments.insert(format!("consumer-{}", j), partitions);
            }
        }
        
        partition_history.push(current_assignments);
    }
    
    // Verify incremental behavior
    // Each consumer should keep most of its partitions when new ones join
    for i in 1..partition_history.len() {
        let prev = &partition_history[i - 1];
        let curr = &partition_history[i];
        
        for (consumer_id, prev_partitions) in prev {
            if let Some(curr_partitions) = curr.get(consumer_id) {
                // Calculate retention rate
                let retained: HashSet<_> = prev_partitions.intersection(curr_partitions).collect();
                let retention_rate = retained.len() as f64 / prev_partitions.len() as f64;
                
                // With cooperative rebalancing, consumers should retain most partitions
                assert!(retention_rate >= 0.5,
                    "Consumer {} only retained {}% of partitions",
                    consumer_id, retention_rate * 100.0);
            }
        }
    }
    
    Ok(())
}


/// Set up a topic with records and one live consumer in `group`.
///
/// Returns the consumer, still joined, so the caller decides when it leaves.
async fn one_live_consumer(
    bootstrap_servers: &str,
    topic: &str,
    group: &str,
) -> StreamConsumer {
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    admin
        .create_topics(
            &[NewTopic::new(topic, 1, TopicReplication::Fixed(1))],
            &AdminOptions::new(),
        )
        .await
        .expect("Failed to create topics")[0]
        .as_ref()
        .expect("Failed to create topic");

    let producer = create_test_producer(bootstrap_servers);
    for i in 0..10 {
        producer
            .send(
                FutureRecord::to(topic).key(&format!("k{}", i)).payload(&format!("v{}", i)),
                Duration::from_secs(5),
            )
            .await
            .expect("produce failed");
    }

    let consumer = create_test_consumer(bootstrap_servers, group);
    consumer.subscribe(&[topic]).expect("subscribe failed");
    let _ = timeout(Duration::from_secs(30), consumer.recv()).await;
    assert!(
        !assigned_partitions(&consumer).is_empty(),
        "consumer never received an assignment; the test would be vacuous"
    );
    consumer
}

/// DescribeGroups must see the same groups ListGroups does.
///
/// `kafka-consumer-groups.sh --describe` returned `GROUP_ID_NOT_FOUND` for a
/// group `--list` was returning in the same second: DescribeGroups was answered
/// from a map on the protocol handler that nothing on the server path writes to,
/// while ListGroups read the live GroupManager. Admin tooling could therefore
/// show no membership for any group, and so no consumer lag.
///
/// This asserts on `state()` rather than `members()` deliberately: rdkafka
/// 0.36.2's `GroupInfo::members` calls `slice::from_raw_parts` on the raw member
/// pointer unconditionally, and librdkafka passes NULL with a count of 0 for a
/// group with no members — undefined behaviour that aborts the test process
/// instead of failing an assertion. Membership content is covered by the unit
/// test `consumer_group::tests::describe_sees_a_live_group_with_its_members`.
#[tokio::test]
async fn describe_groups_agrees_with_list_groups() -> Result<()> {
    test_setup::init();
    let _serial = common::exclusive().await;

    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    let group = "describe-agrees-group";
    let consumer = one_live_consumer(&bootstrap_servers, "describe-agrees", group).await;

    let listed = consumer
        .fetch_group_list(Some(group), Duration::from_secs(15))
        .expect("fetch_group_list failed while a member was connected");
    let described = listed
        .groups()
        .iter()
        .find(|g| g.name() == group)
        .expect("the group is not listed even though a member is connected");

    // A group DescribeGroups could not find comes back with an empty state, which
    // is how the disagreement surfaces here.
    assert!(
        !described.state().is_empty() && described.state() != "Dead",
        "DescribeGroups gave state {:?} for a group with a live consumer that \
         ListGroups is returning — the two APIs disagree",
        described.state()
    );

    Ok(())
}

/// A group whose last member left must stay visible.
///
/// It is `Empty`, not gone: its committed offsets survive and a restarting
/// consumer resumes from them. `list_groups` read only the in-memory registry,
/// which `leave_group` clears, so the group vanished from
/// `kafka-consumer-groups.sh --list` and from Kafka UI the moment the last
/// consumer disconnected — while its offsets were still stored.
#[tokio::test]
async fn an_empty_group_is_still_visible() -> Result<()> {
    test_setup::init();
    let _serial = common::exclusive().await;

    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    let group = "empty-visible-group";
    let consumer = one_live_consumer(&bootstrap_servers, "empty-visible", group).await;

    drop(consumer);
    sleep(Duration::from_secs(3)).await;

    // A second client, so this reads the broker's state rather than the departed
    // consumer's own cached view.
    let observer = create_test_consumer(&bootstrap_servers, "observer-group");
    let after = observer
        .fetch_group_list(Some(group), Duration::from_secs(15))
        .expect("fetch_group_list failed after the last member left");

    assert!(
        after.groups().iter().any(|g| g.name() == group),
        "the group disappeared when its last member left. Its committed offsets \
         are still stored and a restarting consumer resumes from them, so admin \
         tooling must still see it — Kafka reports it as Empty."
    );

    Ok(())
}
