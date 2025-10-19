# Raft Integration Test Examples

Practical examples for running and debugging Raft integration tests.

## Table of Contents
- [Quick Start](#quick-start)
- [Running Tests](#running-tests)
- [Debugging](#debugging)
- [Writing Custom Tests](#writing-custom-tests)
- [Toxiproxy Examples](#toxiproxy-examples)
- [Common Patterns](#common-patterns)

## Quick Start

### Prerequisites

```bash
# 1. Build Chronik server (required)
cd /Users/lspecian/Development/chronik-stream/.conductor/lahore
cargo build --release --bin chronik-server

# 2. Verify binary exists
ls -lh target/release/chronik-server

# 3. (Optional) Install Docker for Toxiproxy tests
docker --version

# 4. (Optional) Pull Toxiproxy image
docker pull ghcr.io/shopify/toxiproxy:2.5.0
```

### Run Your First Test

```bash
# Run all Raft tests
cargo test --test 'test_*' --manifest-path tests/raft/Cargo.toml

# Run specific test
cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml

# Run with output (see logs)
cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml -- --nocapture

# Run with debug logging
RUST_LOG=debug cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml -- --nocapture
```

## Running Tests

### All Tests

```bash
# Run everything
cargo test --manifest-path tests/raft/Cargo.toml

# Run only integration tests (skip unit tests)
cargo test --manifest-path tests/raft/Cargo.toml --test '*'

# Run tests in parallel (default)
cargo test --manifest-path tests/raft/Cargo.toml --test-threads=4

# Run tests sequentially (better for debugging)
cargo test --manifest-path tests/raft/Cargo.toml --test-threads=1
```

### Specific Test Suites

```bash
# Leader election tests
cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml

# Replication tests
cargo test --test test_single_partition_replication --manifest-path tests/raft/Cargo.toml

# Network partition tests (requires Toxiproxy)
cargo test --test test_network_partition --manifest-path tests/raft/Cargo.toml
```

### Individual Tests

```bash
# Run specific test function
cargo test --test test_leader_election test_initial_leader_election --manifest-path tests/raft/Cargo.toml

# Run all tests matching pattern
cargo test --test test_leader_election election --manifest-path tests/raft/Cargo.toml

# Run with exact match
cargo test --test test_leader_election --exact test_initial_leader_election --manifest-path tests/raft/Cargo.toml
```

### Property-Based Tests

```bash
# Run proptest with default cases (10)
cargo test --test test_single_partition_replication proptests --manifest-path tests/raft/Cargo.toml

# Run with more cases for thorough testing
PROPTEST_CASES=100 cargo test --test test_single_partition_replication proptests --manifest-path tests/raft/Cargo.toml

# Run with verbose output
RUST_LOG=debug PROPTEST_CASES=50 cargo test --test test_single_partition_replication proptests --manifest-path tests/raft/Cargo.toml -- --nocapture
```

## Debugging

### Enable Logging

```bash
# Debug logs for everything
RUST_LOG=debug cargo test --test test_leader_election -- --nocapture

# Trace logs for Raft only
RUST_LOG=chronik_raft=trace cargo test --test test_leader_election -- --nocapture

# Multiple targets
RUST_LOG=chronik_raft=debug,chronik_server=info cargo test --test test_leader_election -- --nocapture

# Filter by test name
RUST_LOG=debug cargo test --test test_leader_election test_initial_leader_election -- --nocapture
```

### Inspect Cluster State

Add sleep to test and inspect nodes via HTTP:

```rust
#[tokio::test]
async fn test_debug_cluster() -> Result<()> {
    let cluster = spawn_test_cluster(3).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    println!("Leader: {}", leader_id);
    println!("Sleeping 60s for manual inspection...");

    // Inspect cluster while test is running
    sleep(Duration::from_secs(60)).await;

    cluster.cleanup().await;
    Ok(())
}
```

Then in another terminal:

```bash
# Check node 1 (HTTP on port 8000)
curl http://localhost:8000/api/v1/raft/info | jq

# Check all nodes
for port in 8000 8001 8002; do
  echo "Node on port $port:"
  curl -s http://localhost:$port/api/v1/raft/info | jq '.node_id, .is_leader, .term'
done
```

### Preserve Test Artifacts

```rust
// Don't use TempDir - use persistent directory
let data_dir = PathBuf::from("/tmp/raft-test-data");
std::fs::create_dir_all(&data_dir)?;

// Now you can inspect logs/data after test
```

### Use Test Harness

```bash
# Run test with custom test runner options
cargo test --test test_leader_election -- \
  --nocapture \
  --test-threads=1 \
  --show-output
```

## Writing Custom Tests

### Basic Test Template

```rust
mod common;

use anyhow::Result;
use common::*;
use std::time::Duration;

#[tokio::test]
async fn test_my_scenario() -> Result<()> {
    // 1. Initialize tracing (optional)
    tracing_subscriber::fmt::init();

    // 2. Spawn cluster
    let mut cluster = spawn_test_cluster(3).await?;

    // 3. Wait for initial leader
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    // 4. Test logic here
    // ...

    // 5. Assertions
    assert!(some_condition, "Error message");

    // 6. Cleanup
    cluster.cleanup().await;

    Ok(())
}
```

### Custom Cluster Configuration

```rust
#[tokio::test]
async fn test_custom_config() -> Result<()> {
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,              // 5 nodes
        use_toxiproxy: true,        // Enable fault injection
        base_raft_port: 17000,      // Custom port range
        base_kafka_port: 19000,
        base_http_port: 18000,
        election_timeout_ms: 3000,  // Longer timeout
        heartbeat_interval_ms: 300,
    }).await?;

    // ... test logic

    cluster.cleanup().await;
    Ok(())
}
```

### Testing Leader Behavior

```rust
#[tokio::test]
async fn test_leader_operations() -> Result<()> {
    let cluster = spawn_test_cluster(3).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    let leader_node = cluster.get_node(leader_id).unwrap();

    // Produce to leader
    let producer = create_kafka_producer(leader_node.kafka_addr)?;
    producer.send("my-topic", 0, b"test message").await?;

    // Verify via HTTP API
    let info = get_node_info(leader_node.http_addr).await?;
    assert!(info.is_leader);
    assert!(info.commit_index > 0);

    Ok(())
}
```

### Testing Follower Behavior

```rust
#[tokio::test]
async fn test_follower_operations() -> Result<()> {
    let cluster = spawn_test_cluster(3).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    // Get a follower
    let follower_node = cluster.nodes.iter()
        .find(|n| n.node_id != leader_id)
        .unwrap();

    // Followers should redirect to leader or reject writes
    let result = try_produce_to_follower(follower_node.kafka_addr).await;
    assert!(result.is_err() || result.unwrap().redirected_to_leader);

    Ok(())
}
```

## Toxiproxy Examples

### Manual Toxiproxy Setup

```bash
# Start Toxiproxy
docker run -d \
  --name toxiproxy \
  -p 8474:8474 \
  -p 7000-7010:7000-7010 \
  ghcr.io/shopify/toxiproxy:2.5.0

# Verify it's running
curl http://localhost:8474/version

# View all proxies
curl http://localhost:8474/proxies | jq
```

### Create Proxy Manually

```bash
# Create proxy for node 1
curl -X POST http://localhost:8474/proxies \
  -H "Content-Type: application/json" \
  -d '{
    "name": "node_1_raft",
    "listen": "0.0.0.0:7100",
    "upstream": "127.0.0.1:7001",
    "enabled": true
  }'
```

### Add Toxics (Fault Injection)

```bash
# Add 200ms latency
curl -X POST http://localhost:8474/proxies/node_1_raft/toxics \
  -H "Content-Type: application/json" \
  -d '{
    "type": "latency",
    "name": "latency_toxic",
    "attributes": {
      "latency": 200
    }
  }'

# Add packet loss (10%)
curl -X POST http://localhost:8474/proxies/node_1_raft/toxics \
  -H "Content-Type: application/json" \
  -d '{
    "type": "limit_data",
    "name": "packet_loss",
    "attributes": {
      "bytes": 1000000
    }
  }'

# Add bandwidth limit (100 KB/s)
curl -X POST http://localhost:8474/proxies/node_1_raft/toxics \
  -H "Content-Type: application/json" \
  -d '{
    "type": "bandwidth",
    "name": "slow_network",
    "attributes": {
      "rate": 100
    }
  }'

# Disable proxy (simulate network partition)
curl -X POST http://localhost:8474/proxies/node_1_raft \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# Re-enable proxy
curl -X POST http://localhost:8474/proxies/node_1_raft \
  -H "Content-Type: application/json" \
  -d '{"enabled": true}'
```

### Remove Toxics

```bash
# List toxics
curl http://localhost:8474/proxies/node_1_raft/toxics | jq

# Remove specific toxic
curl -X DELETE http://localhost:8474/proxies/node_1_raft/toxics/latency_toxic

# Remove all toxics (delete and recreate proxy)
curl -X DELETE http://localhost:8474/proxies/node_1_raft
```

### Test with Toxiproxy in Code

```rust
#[tokio::test]
async fn test_with_latency() -> Result<()> {
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 3,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    // Add 500ms latency to node 2
    cluster.add_latency(2, 500).await?;

    // Test operations with latency
    // ...

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_with_partition() -> Result<()> {
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    // Partition nodes 4 and 5
    cluster.partition_network(vec![4, 5]).await?;

    // Test with partition
    // ...

    // Heal partition
    cluster.heal_partition().await?;

    // Test after healing
    // ...

    cluster.cleanup().await;
    Ok(())
}
```

## Common Patterns

### Wait for Condition

```rust
use tokio::time::{timeout, sleep};

// Wait for specific state
let result = timeout(Duration::from_secs(10), async {
    loop {
        if check_condition().await {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
}).await;

assert!(result.is_ok(), "Timeout waiting for condition");
```

### Retry Logic

```rust
use std::cmp::min;

async fn retry_with_backoff<F, T>(mut f: F, max_attempts: u32) -> Result<T>
where
    F: FnMut() -> futures::future::BoxFuture<'static, Result<T>>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match f().await {
            Ok(value) => return Ok(value),
            Err(e) if attempt >= max_attempts => return Err(e),
            _ => {
                let delay = min(1000 * (1 << attempt), 10000);
                sleep(Duration::from_millis(delay)).await;
            }
        }
    }
}
```

### Parallel Operations

```rust
use tokio::task::JoinSet;

#[tokio::test]
async fn test_concurrent_operations() -> Result<()> {
    let cluster = spawn_test_cluster(3).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    let leader_node = cluster.get_node(leader_id).unwrap();

    let mut tasks = JoinSet::new();

    // Spawn 10 concurrent producers
    for i in 0..10 {
        let addr = leader_node.kafka_addr;
        tasks.spawn(async move {
            let producer = create_kafka_producer(addr)?;
            producer.send("test", 0, format!("msg-{}", i).as_bytes()).await
        });
    }

    // Wait for all to complete
    while let Some(result) = tasks.join_next().await {
        result??; // Unwrap JoinHandle and Result
    }

    Ok(())
}
```

### Test with Timeout

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_with_global_timeout() -> Result<()> {
    // Entire test must complete in 30 seconds
    timeout(Duration::from_secs(30), async {
        let cluster = spawn_test_cluster(3).await?;
        // ... test logic
        cluster.cleanup().await;
        Ok::<(), anyhow::Error>(())
    }).await??;

    Ok(())
}
```

### Cleanup Pattern

```rust
#[tokio::test]
async fn test_with_cleanup() -> Result<()> {
    let mut cluster = spawn_test_cluster(3).await?;

    // Use defer-like pattern for cleanup
    let _cleanup_guard = CleanupGuard::new(|| {
        // This runs even if test panics
        let _ = cluster.cleanup();
    });

    // ... test logic that might panic

    Ok(())
}

struct CleanupGuard<F: FnOnce()> {
    cleanup: Option<F>,
}

impl<F: FnOnce()> CleanupGuard<F> {
    fn new(cleanup: F) -> Self {
        Self { cleanup: Some(cleanup) }
    }
}

impl<F: FnOnce()> Drop for CleanupGuard<F> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}
```

## Tips and Tricks

### Speed Up Tests

```bash
# Build in release mode for faster test execution
cargo build --release --bin chronik-server

# Run fewer property-based test cases during development
PROPTEST_CASES=5 cargo test --test test_single_partition_replication proptests

# Use fewer nodes when possible
let cluster = spawn_test_cluster(3).await?; // Instead of 5 or 7
```

### Isolate Failures

```bash
# Run single test to isolate failure
cargo test --test test_leader_election test_initial_leader_election -- --exact

# Run with single thread to avoid race conditions
cargo test --test test_leader_election -- --test-threads=1
```

### Check for Flaky Tests

```bash
# Run test 100 times
for i in {1..100}; do
  echo "Run $i"
  cargo test --test test_leader_election test_initial_leader_election --quiet || break
done
```

### Generate Test Report

```bash
# Install cargo-nextest for better test output
cargo install cargo-nextest

# Run with nextest
cargo nextest run --manifest-path tests/raft/Cargo.toml

# Generate JUnit report
cargo nextest run --manifest-path tests/raft/Cargo.toml --message-format json > test-results.json
```

## Troubleshooting

### Tests Hang

```bash
# Add timeout to test
RUST_TEST_TIMEOUT=60 cargo test --test test_leader_election

# Check for deadlocks with backtrace
RUST_BACKTRACE=1 cargo test --test test_leader_election -- --nocapture
```

### Port Conflicts

```bash
# Find what's using port
lsof -i :9000

# Kill process
kill -9 <PID>

# Or use different port range in test
let cluster = spawn_test_cluster_with_config(ClusterConfig {
    base_raft_port: 17000,
    base_kafka_port: 19000,
    base_http_port: 18000,
    ..Default::default()
}).await?;
```

### Chronik Binary Not Found

```bash
# Verify binary exists
ls -lh target/release/chronik-server

# If not, build it
cargo build --release --bin chronik-server

# Check if binary is executable
chmod +x target/release/chronik-server
```

### Docker Issues

```bash
# Check Docker is running
docker ps

# Restart Docker
# On macOS: Docker Desktop -> Restart

# Pull Toxiproxy image manually
docker pull ghcr.io/shopify/toxiproxy:2.5.0

# Remove stale containers
docker rm -f $(docker ps -aq)
```

## References

- [Rust Testing Guide](https://doc.rust-lang.org/book/ch11-00-testing.html)
- [Tokio Testing](https://tokio.rs/tokio/topics/testing)
- [Proptest Book](https://proptest-rs.github.io/proptest/)
- [Toxiproxy Documentation](https://github.com/Shopify/toxiproxy)
