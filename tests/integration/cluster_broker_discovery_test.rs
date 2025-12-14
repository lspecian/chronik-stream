//! Integration test for 3-node cluster broker metadata synchronization
//!
//! This test verifies the fix for the critical bug where broker metadata was not
//! synchronized across cluster nodes via Raft consensus.
//!
//! Test scenario:
//! 1. Start 3-node Raft cluster (nodes 1, 2, 3)
//! 2. Wait for Raft leader election
//! 3. Each node registers itself as a broker and proposes to Raft
//! 4. Verify that ALL nodes can see ALL 3 brokers in metadata responses
//! 5. Test with real Kafka client (rdkafka) to ensure Kafka protocol works
//!
//! Expected behavior:
//! - All 3 nodes should report broker list: [1, 2, 3]
//! - Kafka clients should receive complete broker metadata from any node
//! - Producer/consumer operations should succeed using discovered brokers

use chronik_config::ClusterConfig;
use chronik_server::integrated_server::{IntegratedKafkaServer, IntegratedServerConfig, IntegratedKafkaServerBuilder};
use chronik_server::raft_cluster::RaftCluster;
use std::sync::Arc;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn, error};
use tempfile::TempDir;
use rdkafka::admin::{AdminClient, AdminOptions};
use rdkafka::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::consumer::{Consumer, StreamConsumer};

/// Helper to create a test node configuration
fn create_node_config(
    node_id: u64,
    kafka_port: u16,
    wal_port: u16,
    raft_port: u16,
    data_dir: PathBuf,
    peers: Vec<(u64, String, String, String)>,
) -> (IntegratedServerConfig, ClusterConfig) {
    let cluster_config = ClusterConfig {
        enabled: true,
        node_id,
        replication_factor: 3,
        min_insync_replicas: 2,
        node: chronik_config::NodeConfig {
            addresses: chronik_config::NodeAddresses {
                kafka: format!("0.0.0.0:{}", kafka_port),
                wal: format!("0.0.0.0:{}", wal_port),
                raft: format!("0.0.0.0:{}", raft_port),
            },
            advertise: Some(chronik_config::NodeAddresses {
                kafka: format!("localhost:{}", kafka_port),
                wal: format!("localhost:{}", wal_port),
                raft: format!("localhost:{}", raft_port),
            }),
        },
        peers: peers
            .into_iter()
            .map(|(id, kafka, wal, raft)| chronik_config::PeerConfig {
                id,
                kafka,
                wal,
                raft,
            })
            .collect(),
    };

    let server_config = IntegratedServerConfig {
        node_id: node_id as i32,
        advertised_host: "localhost".to_string(),
        advertised_port: kafka_port as i32,
        data_dir: data_dir.to_string_lossy().to_string(),
        enable_indexing: false,
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor: 3,
        enable_wal_indexing: false,
        wal_indexing_interval_secs: 30,
        object_store_config: None,
        enable_metadata_dr: false,
        metadata_upload_interval_secs: 60,
        cluster_config: Some(cluster_config.clone()),
        tls_config: None,
    };

    (server_config, cluster_config)
}

/// Start a 3-node Raft cluster and verify broker metadata synchronization
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_3_node_broker_metadata_synchronization() -> anyhow::Result<()> {
    // Initialize logging
    let _ = tracing_subscriber::fmt()
        .with_env_filter("chronik_server=info,chronik_raft=debug")
        .try_init();

    info!("=== Starting 3-Node Cluster Broker Discovery Test ===");

    // Create temporary directories for each node
    let temp_dir_1 = TempDir::new()?;
    let temp_dir_2 = TempDir::new()?;
    let temp_dir_3 = TempDir::new()?;

    let data_dir_1 = temp_dir_1.path().join("node1");
    let data_dir_2 = temp_dir_2.path().join("node2");
    let data_dir_3 = temp_dir_3.path().join("node3");

    tokio::fs::create_dir_all(&data_dir_1).await?;
    tokio::fs::create_dir_all(&data_dir_2).await?;
    tokio::fs::create_dir_all(&data_dir_3).await?;

    // Port assignments
    let (kafka_1, wal_1, raft_1) = (19092, 19291, 15001);
    let (kafka_2, wal_2, raft_2) = (19093, 19292, 15002);
    let (kafka_3, wal_3, raft_3) = (19094, 19293, 15003);

    // Define peers (each node needs to know about all other nodes)
    let peers = vec![
        (1u64, format!("localhost:{}", kafka_1), format!("localhost:{}", wal_1), format!("localhost:{}", raft_1)),
        (2u64, format!("localhost:{}", kafka_2), format!("localhost:{}", wal_2), format!("localhost:{}", raft_2)),
        (3u64, format!("localhost:{}", kafka_3), format!("localhost:{}", wal_3), format!("localhost:{}", raft_3)),
    ];

    // Create configurations for each node
    let (server_config_1, cluster_config_1) = create_node_config(1, kafka_1, wal_1, raft_1, data_dir_1.clone(), peers.clone());
    let (server_config_2, cluster_config_2) = create_node_config(2, kafka_2, wal_2, raft_2, data_dir_2.clone(), peers.clone());
    let (server_config_3, cluster_config_3) = create_node_config(3, kafka_3, wal_3, raft_3, data_dir_3.clone(), peers.clone());

    info!("Node 1: Kafka={}, WAL={}, Raft={}", kafka_1, wal_1, raft_1);
    info!("Node 2: Kafka={}, WAL={}, Raft={}", kafka_2, wal_2, raft_2);
    info!("Node 3: Kafka={}, WAL={}, Raft={}", kafka_3, wal_3, raft_3);

    // Start Raft clusters for each node
    info!("Bootstrapping Raft cluster for Node 1...");
    let raft_peers_1 = vec![
        (2, format!("http://localhost:{}", raft_2)),
        (3, format!("http://localhost:{}", raft_3)),
    ];
    let raft_cluster_1 = Arc::new(
        RaftCluster::bootstrap(1, raft_peers_1, data_dir_1.clone()).await?
    );

    info!("Bootstrapping Raft cluster for Node 2...");
    let raft_peers_2 = vec![
        (1, format!("http://localhost:{}", raft_1)),
        (3, format!("http://localhost:{}", raft_3)),
    ];
    let raft_cluster_2 = Arc::new(
        RaftCluster::bootstrap(2, raft_peers_2, data_dir_2.clone()).await?
    );

    info!("Bootstrapping Raft cluster for Node 3...");
    let raft_peers_3 = vec![
        (1, format!("http://localhost:{}", raft_1)),
        (2, format!("http://localhost:{}", raft_2)),
    ];
    let raft_cluster_3 = Arc::new(
        RaftCluster::bootstrap(3, raft_peers_3, data_dir_3.clone()).await?
    );

    // Start Raft message loops (required for consensus)
    info!("Starting Raft message loops...");
    tokio::spawn({
        let raft = raft_cluster_1.clone();
        async move {
            if let Err(e) = raft.run().await {
                error!("Raft cluster 1 error: {:?}", e);
            }
        }
    });

    tokio::spawn({
        let raft = raft_cluster_2.clone();
        async move {
            if let Err(e) = raft.run().await {
                error!("Raft cluster 2 error: {:?}", e);
            }
        }
    });

    tokio::spawn({
        let raft = raft_cluster_3.clone();
        async move {
            if let Err(e) = raft.run().await {
                error!("Raft cluster 3 error: {:?}", e);
            }
        }
    });

    // Wait for Raft leader election
    info!("Waiting for Raft leader election...");
    let mut leader_elected = false;
    for attempt in 1..=30 {
        sleep(Duration::from_secs(1)).await;

        let is_leader_1 = raft_cluster_1.is_leader().await;
        let is_leader_2 = raft_cluster_2.is_leader().await;
        let is_leader_3 = raft_cluster_3.is_leader().await;

        if is_leader_1 || is_leader_2 || is_leader_3 {
            let leader_id = if is_leader_1 { 1 } else if is_leader_2 { 2 } else { 3 };
            info!("✓ Raft leader elected: Node {}", leader_id);
            leader_elected = true;
            break;
        }

        if attempt % 5 == 0 {
            warn!("Still waiting for leader election... ({}s elapsed)", attempt);
        }
    }

    assert!(leader_elected, "Raft leader election failed after 30 seconds");

    // Start integrated Kafka servers with Raft coordination using builder pattern
    info!("Starting IntegratedKafkaServer for Node 1 with builder...");
    let server_1 = IntegratedKafkaServerBuilder::new(server_config_1)
        .with_raft_cluster(raft_cluster_1.clone())
        .build()
        .await?;

    info!("Starting IntegratedKafkaServer for Node 2 with builder...");
    let server_2 = IntegratedKafkaServerBuilder::new(server_config_2)
        .with_raft_cluster(raft_cluster_2.clone())
        .build()
        .await?;

    info!("Starting IntegratedKafkaServer for Node 3 with builder...");
    let server_3 = IntegratedKafkaServerBuilder::new(server_config_3)
        .with_raft_cluster(raft_cluster_3.clone())
        .build()
        .await?;

    // Start server TCP listeners
    info!("Starting Kafka protocol listeners...");
    let listener_1_handle = tokio::spawn({
        let server = server_1.clone();
        async move {
            if let Err(e) = server.run(format!("0.0.0.0:{}", kafka_1)).await {
                error!("Server 1 error: {:?}", e);
            }
        }
    });

    let listener_2_handle = tokio::spawn({
        let server = server_2.clone();
        async move {
            if let Err(e) = server.run(format!("0.0.0.0:{}", kafka_2)).await {
                error!("Server 2 error: {:?}", e);
            }
        }
    });

    let listener_3_handle = tokio::spawn({
        let server = server_3.clone();
        async move {
            if let Err(e) = server.run(format!("0.0.0.0:{}", kafka_3)).await {
                error!("Server 3 error: {:?}", e);
            }
        }
    });

    // Wait for servers to start and broker registration to complete
    info!("Waiting for broker registration and synchronization...");
    sleep(Duration::from_secs(15)).await;

    // CRITICAL TEST: Verify broker metadata from Raft state machine
    info!("=== Verifying Broker Metadata in Raft State Machine ===");

    let raft_brokers_1 = raft_cluster_1.get_all_brokers_from_state_machine();
    let raft_brokers_2 = raft_cluster_2.get_all_brokers_from_state_machine();
    let raft_brokers_3 = raft_cluster_3.get_all_brokers_from_state_machine();

    info!("Node 1 Raft state machine brokers: {} broker(s)", raft_brokers_1.len());
    for (broker_id, host, port, _rack) in &raft_brokers_1 {
        info!("  - Broker {}: {}:{}", broker_id, host, port);
    }

    info!("Node 2 Raft state machine brokers: {} broker(s)", raft_brokers_2.len());
    for (broker_id, host, port, _rack) in &raft_brokers_2 {
        info!("  - Broker {}: {}:{}", broker_id, host, port);
    }

    info!("Node 3 Raft state machine brokers: {} broker(s)", raft_brokers_3.len());
    for (broker_id, host, port, _rack) in &raft_brokers_3 {
        info!("  - Broker {}: {}:{}", broker_id, host, port);
    }

    // Assert that all nodes have complete broker list
    assert_eq!(raft_brokers_1.len(), 3, "Node 1 should see 3 brokers in Raft state machine");
    assert_eq!(raft_brokers_2.len(), 3, "Node 2 should see 3 brokers in Raft state machine");
    assert_eq!(raft_brokers_3.len(), 3, "Node 3 should see 3 brokers in Raft state machine");

    // Verify broker IDs are correct
    let broker_ids_1: Vec<i32> = raft_brokers_1.iter().map(|(id, _, _, _)| *id).collect();
    let broker_ids_2: Vec<i32> = raft_brokers_2.iter().map(|(id, _, _, _)| *id).collect();
    let broker_ids_3: Vec<i32> = raft_brokers_3.iter().map(|(id, _, _, _)| *id).collect();

    assert!(broker_ids_1.contains(&1) && broker_ids_1.contains(&2) && broker_ids_1.contains(&3),
            "Node 1 should see brokers [1, 2, 3], got {:?}", broker_ids_1);
    assert!(broker_ids_2.contains(&1) && broker_ids_2.contains(&2) && broker_ids_2.contains(&3),
            "Node 2 should see brokers [1, 2, 3], got {:?}", broker_ids_2);
    assert!(broker_ids_3.contains(&1) && broker_ids_3.contains(&2) && broker_ids_3.contains(&3),
            "Node 3 should see brokers [1, 2, 3], got {:?}", broker_ids_3);

    info!("✓ All nodes have complete broker metadata in Raft state machine");

    // CRITICAL TEST: Verify Kafka client can see all brokers
    info!("=== Testing Kafka Client Broker Discovery ===");

    // Test metadata request from each node
    for (node_id, kafka_port) in [(1, kafka_1), (2, kafka_2), (3, kafka_3)] {
        info!("Testing metadata request from Node {} (localhost:{})...", node_id, kafka_port);

        let admin: AdminClient<_> = ClientConfig::new()
            .set("bootstrap.servers", &format!("localhost:{}", kafka_port))
            .set("request.timeout.ms", "10000")
            .create()
            .expect(&format!("Failed to create admin client for node {}", node_id));

        let metadata = admin
            .inner()
            .fetch_metadata(None, Duration::from_secs(10))
            .expect(&format!("Failed to fetch metadata from node {}", node_id));

        let broker_count = metadata.brokers().len();
        info!("Node {} metadata response: {} broker(s)", node_id, broker_count);

        for broker in metadata.brokers() {
            info!("  - Broker {}: {}:{}", broker.id(), broker.host(), broker.port());
        }

        assert_eq!(broker_count, 3,
                   "Node {} should return 3 brokers in metadata, got {}",
                   node_id, broker_count);

        // Verify broker IDs
        let broker_ids: Vec<i32> = metadata.brokers().iter().map(|b| b.id()).collect();
        assert!(broker_ids.contains(&1) && broker_ids.contains(&2) && broker_ids.contains(&3),
                "Node {} should return brokers [1, 2, 3], got {:?}", node_id, broker_ids);
    }

    info!("✓ All nodes return complete broker metadata via Kafka protocol");

    // CRITICAL TEST: Test produce/consume using discovered brokers
    info!("=== Testing Producer/Consumer with Broker Discovery ===");

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &format!("localhost:{}", kafka_1))
        .set("request.timeout.ms", "10000")
        .create()
        .expect("Failed to create producer");

    // Produce test message
    info!("Producing test message...");
    let delivery = producer
        .send(
            FutureRecord::to("test-cluster-topic")
                .key("test-key")
                .payload("test-value"),
            Duration::from_secs(10),
        )
        .await;

    match delivery {
        Ok((partition, offset)) => {
            info!("✓ Message produced successfully: partition={}, offset={}", partition, offset);
        }
        Err((e, _)) => {
            panic!("Failed to produce message: {:?}", e);
        }
    }

    // Consume test message
    info!("Consuming test message...");
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &format!("localhost:{}", kafka_2)) // Connect to different node!
        .set("group.id", "test-consumer-group")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("Failed to create consumer");

    consumer.subscribe(&["test-cluster-topic"]).expect("Failed to subscribe");

    info!("Waiting for message...");
    let message = tokio::time::timeout(
        Duration::from_secs(10),
        consumer.recv()
    ).await;

    match message {
        Ok(Ok(msg)) => {
            let key = msg.key().map(|k| String::from_utf8_lossy(k).to_string());
            let payload = msg.payload().map(|p| String::from_utf8_lossy(p).to_string());
            info!("✓ Message consumed successfully: key={:?}, payload={:?}", key, payload);
            assert_eq!(key.as_deref(), Some("test-key"));
            assert_eq!(payload.as_deref(), Some("test-value"));
        }
        Ok(Err(e)) => panic!("Consumer error: {:?}", e),
        Err(_) => panic!("Timeout waiting for message"),
    }

    info!("=== All Tests Passed ===");
    info!("✓ Broker metadata synchronized across all 3 nodes");
    info!("✓ Kafka clients can discover all brokers from any node");
    info!("✓ Producer/consumer operations work correctly");

    // Cleanup
    listener_1_handle.abort();
    listener_2_handle.abort();
    listener_3_handle.abort();

    Ok(())
}
