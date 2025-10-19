# Raft Integration Tests

Comprehensive integration testing framework for Chronik Raft clustering with fault injection capabilities.

## Overview

This test suite provides:
- **Multi-node cluster spawning** - Start N-node Raft clusters programmatically
- **Fault injection** - Network partitions, latency, node crashes via Toxiproxy
- **Leader election testing** - Validate consensus under various failure scenarios
- **Replication testing** - Verify data consistency across cluster
- **Property-based testing** - Use proptest for exhaustive scenario coverage

## Test Structure

```
tests/raft/
├── common/
│   └── mod.rs              # Test utilities and cluster management
├── test_leader_election.rs # Leader election scenarios
├── test_single_partition_replication.rs # Single partition replication
├── test_network_partition.rs # Network partition handling
└── README.md              # This file
```

## Quick Start

### 1. Build Chronik Server

```bash
# Build release binary (required for tests)
cargo build --release --bin chronik-server
```

### 2. Run All Raft Tests

```bash
# Run all Raft integration tests
cargo test --test 'raft_*'

# Run specific test
cargo test --test test_leader_election

# Run with output
cargo test --test test_leader_election -- --nocapture

# Run with RUST_LOG
RUST_LOG=debug cargo test --test test_leader_election -- --nocapture
```

### 3. Run with Toxiproxy (Optional)

For advanced fault injection, use Toxiproxy:

```bash
# Start Toxiproxy container (automatic in tests)
docker run -d --name toxiproxy -p 8474:8474 ghcr.io/shopify/toxiproxy:2.5.0

# Tests will automatically detect and use it
cargo test --test test_network_partition
```

## Test Scenarios

### Leader Election Tests

```rust
#[tokio::test]
async fn test_initial_leader_election() {
    let cluster = spawn_test_cluster(3).await.unwrap();

    // Wait for leader election (should complete within 5 seconds)
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await.unwrap();

    assert!(leader_id >= 1 && leader_id <= 3);
}

#[tokio::test]
async fn test_leader_crash_reelection() {
    let mut cluster = spawn_test_cluster(3).await.unwrap();

    // Wait for initial leader
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await.unwrap();

    // Kill leader
    cluster.kill_node(leader_id).await.unwrap();

    // New leader should be elected within 2 * election_timeout
    let new_leader = cluster.wait_for_leader(Duration::from_secs(3)).await.unwrap();

    assert_ne!(new_leader, leader_id);
}
```

### Replication Tests

```rust
#[tokio::test]
async fn test_replicate_to_all_nodes() {
    let cluster = spawn_test_cluster(3).await.unwrap();
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await.unwrap();

    // Produce to leader
    let producer = KafkaProducer::new(cluster.get_node(leader_id).unwrap().kafka_addr);
    producer.send("test-topic", b"hello raft").await.unwrap();

    // Wait for replication
    sleep(Duration::from_secs(1)).await;

    // Consume from all followers
    for node in &cluster.nodes {
        if node.node_id != leader_id {
            let consumer = KafkaConsumer::new(node.kafka_addr);
            let records = consumer.poll("test-topic", 0).await.unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0], b"hello raft");
        }
    }
}
```

### Network Partition Tests

```rust
#[tokio::test]
async fn test_network_partition_minority() {
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        ..Default::default()
    }).await.unwrap();

    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await.unwrap();

    // Partition: isolate 2 nodes (minority)
    cluster.partition_network(vec![4, 5]).await.unwrap();

    // Majority (3 nodes) should still have leader
    sleep(Duration::from_secs(2)).await;
    let current_leader = cluster.get_leader().await.unwrap();
    assert!(current_leader <= 3); // Leader is in majority partition

    // Heal partition
    cluster.heal_partition().await.unwrap();

    // All nodes should agree on leader again
    cluster.wait_for_consensus(Duration::from_secs(5)).await.unwrap();
}
```

## Toxiproxy Setup

### Manual Setup (Optional)

If you want to run Toxiproxy manually for debugging:

```bash
# Start Toxiproxy
docker run -d \
  --name toxiproxy \
  -p 8474:8474 \
  -p 7000-7010:7000-7010 \
  ghcr.io/shopify/toxiproxy:2.5.0

# Create proxy for node 1
curl -X POST http://localhost:8474/proxies \
  -H "Content-Type: application/json" \
  -d '{
    "name": "node_1",
    "listen": "0.0.0.0:7000",
    "upstream": "host.docker.internal:7001",
    "enabled": true
  }'

# Add latency toxic (100ms)
curl -X POST http://localhost:8474/proxies/node_1/toxics \
  -H "Content-Type: application/json" \
  -d '{
    "type": "latency",
    "attributes": {
      "latency": 100
    }
  }'

# Disable proxy (simulate network partition)
curl -X POST http://localhost:8474/proxies/node_1 \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# List all proxies
curl http://localhost:8474/proxies

# Clean up
docker rm -f toxiproxy
```

### Toxiproxy Fault Injection Examples

```rust
// Add 200ms latency to node 2
cluster.add_latency(2, 200).await.unwrap();

// Partition nodes 1 and 2 from rest of cluster
cluster.partition_network(vec![1, 2]).await.unwrap();

// Heal partition
cluster.heal_partition().await.unwrap();
```

## Property-Based Testing with Proptest

Use proptest for exhaustive scenario coverage:

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_cluster_survives_random_failures(
        cluster_size in 3..7usize,
        failures in prop::collection::vec(0..6u64, 0..3)
    ) {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut cluster = spawn_test_cluster(cluster_size).await.unwrap();

            // Wait for initial leader
            let _ = cluster.wait_for_leader(Duration::from_secs(5)).await.unwrap();

            // Apply random failures
            for node_id in failures {
                if node_id < cluster_size as u64 {
                    let _ = cluster.kill_node(node_id + 1).await;
                    sleep(Duration::from_millis(500)).await;
                }
            }

            // Cluster should still have quorum and elect leader
            // (if majority still alive)
            let alive_count = cluster.nodes.iter()
                .filter(|n| n.process.id().is_some())
                .count();

            if alive_count > cluster_size / 2 {
                let _ = cluster.wait_for_leader(Duration::from_secs(10)).await.unwrap();
            }
        });
    }
}
```

## Test Utilities API

### Cluster Management

```rust
// Spawn 3-node cluster
let cluster = spawn_test_cluster(3).await?;

// Spawn with custom config
let cluster = spawn_test_cluster_with_config(ClusterConfig {
    node_count: 5,
    use_toxiproxy: true,
    election_timeout_ms: 2000,
    heartbeat_interval_ms: 200,
    ..Default::default()
}).await?;

// Wait for leader (with timeout)
let leader_id = cluster.wait_for_leader(Duration::from_secs(10)).await?;

// Wait for all nodes to agree on leader
let leader_id = cluster.wait_for_consensus(Duration::from_secs(10)).await?;

// Get current leader (None if no leader)
if let Some(leader_id) = cluster.get_leader().await {
    println!("Leader: {}", leader_id);
}
```

### Fault Injection

```rust
// Kill node
cluster.kill_node(1).await?;

// Restart node (preserves data)
cluster.restart_node(1).await?;

// Network partition (requires Toxiproxy)
cluster.partition_network(vec![1, 2]).await?;
cluster.heal_partition().await?;

// Add latency (requires Toxiproxy)
cluster.add_latency(3, 500).await?; // 500ms
```

### Node Inspection

```rust
// Check if node is alive
if cluster.get_node(1).unwrap().is_alive().await {
    println!("Node 1 is running");
}

// Get node addresses
let node = cluster.get_node(1).unwrap();
println!("Raft addr: {}", node.raft_addr);
println!("Kafka addr: {}", node.kafka_addr);
println!("HTTP addr: {}", node.http_addr);
```

## Best Practices

### 1. Use Timeouts

Always use timeouts to prevent tests hanging forever:

```rust
// Good
let leader = timeout(Duration::from_secs(10), cluster.wait_for_leader()).await??;

// Also good (built-in timeout)
let leader = cluster.wait_for_leader(Duration::from_secs(10)).await?;
```

### 2. Clean Up Resources

Tests automatically clean up on drop, but explicit cleanup is better:

```rust
#[tokio::test]
async fn my_test() {
    let mut cluster = spawn_test_cluster(3).await.unwrap();

    // ... test logic ...

    cluster.cleanup().await; // Explicit cleanup
}
```

### 3. Check Quorum Before Assertions

After killing nodes, verify quorum still exists:

```rust
cluster.kill_node(1).await?;

// Calculate alive nodes
let alive = cluster.nodes.iter()
    .filter(|n| n.is_alive().await)
    .count();

if alive > cluster.config.node_count / 2 {
    // Safe to test leader election
    let leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
}
```

### 4. Use Property-Based Testing for Edge Cases

Proptest can find edge cases you might miss:

```rust
proptest! {
    #[test]
    fn test_property(input in any::<YourType>()) {
        // Test invariants hold for any input
    }
}
```

### 5. Test Idempotency

All tests should be idempotent (runnable multiple times):

```rust
// Good - uses fresh temp directory each run
let cluster = spawn_test_cluster(3).await?;

// Bad - hardcoded paths could conflict
let data_dir = PathBuf::from("/tmp/raft-test");
```

## Debugging Tips

### Enable Debug Logs

```bash
RUST_LOG=chronik_raft=debug,chronik_server=trace cargo test --test test_leader_election -- --nocapture
```

### Check Node Logs

Each node logs to stderr by default. Use `--nocapture` to see logs:

```bash
cargo test --test my_test -- --nocapture
```

### Inspect Node State via HTTP

```bash
# While test is running (add sleep in test)
curl http://localhost:8000/api/v1/raft/info | jq
curl http://localhost:8001/api/v1/raft/info | jq
curl http://localhost:8002/api/v1/raft/info | jq
```

### Use Toxiproxy UI

```bash
# Start with UI
docker run -d --name toxiproxy -p 8474:8474 -p 8080:8080 ghcr.io/shopify/toxiproxy:2.5.0

# Access UI
open http://localhost:8080
```

## Troubleshooting

### Test Hangs

- Check for missing timeouts in `wait_for_*` calls
- Verify Chronik binary exists: `cargo build --release --bin chronik-server`
- Check if ports are already in use: `lsof -i :9000-9010`

### Leader Election Fails

- Increase `election_timeout_ms` for slower machines
- Check network connectivity between nodes
- Verify quorum (majority of nodes alive)

### Toxiproxy Connection Errors

- Ensure Docker is running
- Check Toxiproxy container: `docker ps | grep toxiproxy`
- Verify port 8474 is available

### Port Conflicts

Tests use ports 7000-7010 (Raft), 8000-8010 (HTTP), 9000-9010 (Kafka). If conflicts occur:

```rust
let cluster = spawn_test_cluster_with_config(ClusterConfig {
    base_raft_port: 17000, // Use different range
    base_kafka_port: 19000,
    base_http_port: 18000,
    ..Default::default()
}).await?;
```

## Contributing

When adding new tests:

1. Follow naming convention: `test_{scenario}.rs`
2. Document test purpose in docstring
3. Use test utilities from `common/mod.rs`
4. Add property-based variants when applicable
5. Include cleanup logic
6. Test both success and failure paths

## References

- [Raft Consensus Paper](https://raft.github.io/raft.pdf)
- [Toxiproxy Documentation](https://github.com/Shopify/toxiproxy)
- [Proptest Guide](https://proptest-rs.github.io/proptest/intro.html)
- [Testcontainers Rust](https://docs.rs/testcontainers/latest/testcontainers/)
