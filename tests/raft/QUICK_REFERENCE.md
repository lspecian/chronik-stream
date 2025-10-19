# Raft Testing Quick Reference

One-page reference for common testing operations.

## Setup (Run Once)

```bash
# Build Chronik server
cargo build --release --bin chronik-server

# Pull Toxiproxy (optional)
docker pull ghcr.io/shopify/toxiproxy:2.5.0
```

## Running Tests

```bash
# All tests
cargo test --manifest-path tests/raft/Cargo.toml

# Specific test file
cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml

# Specific test function
cargo test --test test_leader_election test_initial_leader_election --manifest-path tests/raft/Cargo.toml

# With output
cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml -- --nocapture

# With debug logs
RUST_LOG=debug cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml -- --nocapture
```

## Test Utilities API

### Spawn Cluster

```rust
// 3-node cluster
let cluster = spawn_test_cluster(3).await?;

// Custom config
let cluster = spawn_test_cluster_with_config(ClusterConfig {
    node_count: 5,
    use_toxiproxy: true,
    election_timeout_ms: 2000,
    ..Default::default()
}).await?;
```

### Leader Operations

```rust
// Wait for leader (with timeout)
let leader_id = cluster.wait_for_leader(Duration::from_secs(10)).await?;

// Get current leader (None if no leader)
let leader_id = cluster.get_leader().await;

// Wait for consensus
let leader_id = cluster.wait_for_consensus(Duration::from_secs(10)).await?;
```

### Node Operations

```rust
// Get node
let node = cluster.get_node(1).unwrap();

// Kill node
cluster.kill_node(1).await?;

// Restart node
cluster.restart_node(1).await?;

// Check if alive
if node.is_alive().await { ... }
```

### Fault Injection (requires Toxiproxy)

```rust
// Network partition
cluster.partition_network(vec![1, 2]).await?;
cluster.heal_partition().await?;

// Add latency
cluster.add_latency(2, 500).await?; // 500ms
```

### Cleanup

```rust
// Explicit cleanup
cluster.cleanup().await;

// Automatic on drop (best-effort)
```

## Common Test Patterns

### Basic Test

```rust
#[tokio::test]
async fn test_scenario() -> Result<()> {
    let mut cluster = spawn_test_cluster(3).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    // Test logic

    cluster.cleanup().await;
    Ok(())
}
```

### Test with Fault Injection

```rust
#[tokio::test]
async fn test_partition() -> Result<()> {
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    cluster.partition_network(vec![4, 5]).await?;
    // Test with partition
    cluster.heal_partition().await?;
    // Test after healing

    cluster.cleanup().await;
    Ok(())
}
```

## Toxiproxy Manual Control

```bash
# Start Toxiproxy
docker run -d --name toxiproxy -p 8474:8474 ghcr.io/shopify/toxiproxy:2.5.0

# List proxies
curl http://localhost:8474/proxies | jq

# Add latency
curl -X POST http://localhost:8474/proxies/node_1/toxics \
  -H "Content-Type: application/json" \
  -d '{"type":"latency","attributes":{"latency":200}}'

# Disable proxy (partition)
curl -X POST http://localhost:8474/proxies/node_1 \
  -d '{"enabled":false}'

# Enable proxy
curl -X POST http://localhost:8474/proxies/node_1 \
  -d '{"enabled":true}'

# Stop Toxiproxy
docker rm -f toxiproxy
```

## Debugging

```bash
# Debug logs
RUST_LOG=debug cargo test --test test_leader_election -- --nocapture

# Trace Raft only
RUST_LOG=chronik_raft=trace cargo test --test test_leader_election -- --nocapture

# Single-threaded (easier debugging)
cargo test --test test_leader_election -- --test-threads=1 --nocapture
```

## Inspect Running Test

Add sleep to test:

```rust
sleep(Duration::from_secs(60)).await;
```

Then query nodes:

```bash
# Check node status
curl http://localhost:8000/api/v1/raft/info | jq
curl http://localhost:8001/api/v1/raft/info | jq
curl http://localhost:8002/api/v1/raft/info | jq

# Check all nodes
for port in 8000 8001 8002; do
  curl -s http://localhost:$port/api/v1/raft/info | jq '.node_id, .is_leader'
done
```

## Port Ranges

Default ports (customize if conflicts):

- Raft: 7000-7010
- Kafka: 9000-9010
- HTTP: 8000-8010
- Toxiproxy API: 8474

```bash
# Check port usage
lsof -i :9000
lsof -i :7000-7010

# Kill process on port
lsof -ti :9000 | xargs kill -9
```

## Property-Based Testing

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_property(input in any::<YourType>()) {
        // Test logic
    }
}
```

Run with more cases:

```bash
PROPTEST_CASES=100 cargo test --test test_single_partition_replication proptests
```

## Troubleshooting

| Issue | Solution |
|-------|----------|
| Tests hang | Add `RUST_TEST_TIMEOUT=60` or check for missing timeouts |
| Port conflicts | Kill processes: `lsof -ti :9000 \| xargs kill -9` |
| Binary not found | Run: `cargo build --release --bin chronik-server` |
| Docker errors | Check Docker is running: `docker ps` |
| Flaky tests | Run single-threaded: `--test-threads=1` |

## Cheat Sheet

```bash
# Full test suite
cargo test --manifest-path tests/raft/Cargo.toml

# Quick smoke test (3-node leader election)
cargo test --test test_leader_election test_initial_leader_election

# Full debug
RUST_LOG=trace cargo test --test test_leader_election -- --test-threads=1 --nocapture

# Property tests (quick)
PROPTEST_CASES=10 cargo test proptests

# Property tests (thorough)
PROPTEST_CASES=100 cargo test proptests

# Build only
cargo build --release --bin chronik-server

# Clean up Docker
docker rm -f toxiproxy

# Clean up ports
lsof -ti :7000-7010,8000-8010,9000-9010 | xargs kill -9
```
