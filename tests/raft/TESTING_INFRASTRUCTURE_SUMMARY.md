# Chronik Raft Testing Infrastructure Summary

Complete testing infrastructure for Raft clustering with fault injection capabilities.

## Overview

The Raft testing infrastructure provides:

- **Multi-node cluster spawning** - Programmatically start N-node Raft clusters
- **Fault injection** - Network partitions, latency, node crashes via Toxiproxy
- **Leader election testing** - Validate consensus under various failure scenarios
- **Replication testing** - Verify data consistency across cluster
- **Property-based testing** - Use proptest for exhaustive scenario coverage
- **Idempotent tests** - Can run multiple times without conflicts
- **Fast failure** - All tests have timeouts to prevent hangs

## Directory Structure

```
tests/raft/
├── common/
│   └── mod.rs                           # Test utilities and cluster management
│
├── test_leader_election.rs              # Leader election scenarios
├── test_single_partition_replication.rs # Single partition replication
├── test_network_partition.rs            # Network partition handling
│
├── Cargo.toml                           # Test dependencies
├── README.md                            # Main documentation
├── EXAMPLES.md                          # Practical examples
├── QUICK_REFERENCE.md                   # One-page cheat sheet
└── TESTING_INFRASTRUCTURE_SUMMARY.md    # This file
```

## Components

### 1. Test Utilities (`common/mod.rs`)

**Core Types:**
- `TestNode` - Represents a Chronik node in test cluster
- `TestCluster` - Manages multi-node cluster lifecycle
- `ClusterConfig` - Configuration for cluster spawning
- `ToxiproxyContainer` - Fault injection via Toxiproxy
- `NodeInfo` - Node state from HTTP API

**Key Functions:**

```rust
// Spawn cluster
spawn_test_cluster(node_count: usize) -> Result<TestCluster>
spawn_test_cluster_with_config(config: ClusterConfig) -> Result<TestCluster>

// Leader operations
cluster.wait_for_leader(timeout: Duration) -> Result<u64>
cluster.get_leader() -> Option<u64>
cluster.wait_for_consensus(timeout: Duration) -> Result<u64>

// Node operations
cluster.kill_node(node_id: u64) -> Result<()>
cluster.restart_node(node_id: u64) -> Result<()>
node.is_alive() -> bool

// Fault injection (requires Toxiproxy)
cluster.partition_network(isolated_nodes: Vec<u64>) -> Result<()>
cluster.heal_partition() -> Result<()>
cluster.add_latency(node_id: u64, latency_ms: u32) -> Result<()>
```

**Features:**
- Automatic port allocation (no conflicts)
- TempDir for each node (no filesystem conflicts)
- Process lifecycle management
- HTTP health checks
- Graceful cleanup (Drop impl)

### 2. Test Suites

#### `test_leader_election.rs`

Tests Raft leader election under various scenarios:

- `test_initial_leader_election` - Fresh cluster election
- `test_leader_crash_reelection` - Re-election after leader crashes
- `test_leader_isolation_reelection` - Network partition of leader
- `test_majority_required_for_election` - Quorum validation
- `test_rapid_leader_changes` - Stability under successive failures
- `test_election_with_different_timeouts` - Timeout configuration
- `test_simultaneous_node_failures` - Multiple concurrent failures
- Property tests for random failure scenarios

#### `test_single_partition_replication.rs`

Tests data replication across cluster:

- `test_basic_replication_3_nodes` - Basic produce→replicate→consume
- `test_replication_after_follower_restart` - Recovery after restart
- `test_replication_with_lag` - Catch-up after being offline
- `test_replication_consistency` - All nodes have identical state
- Property tests for arbitrary message sequences

#### `test_network_partition.rs`

Tests network partition handling:

- `test_majority_partition_elects_leader` - Majority has leader
- `test_partition_healing_converges` - Convergence after healing
- `test_symmetric_partition` - Split-brain prevention
- `test_cascading_partitions` - Multiple successive partitions
- `test_partition_with_leader_in_minority` - Leader isolation
- `test_flapping_network` - Stability under rapid partition/heal

### 3. Toxiproxy Integration

**Toxiproxy Container:**
- Automatically started via testcontainers
- HTTP API on port 8474
- Proxy creation for each node
- Fault injection: latency, partition, packet loss, bandwidth limit

**Fault Types:**
- **Network partition** - Disable proxy (simulates firewall/network split)
- **Latency** - Add artificial delay (e.g., 200ms)
- **Bandwidth limit** - Throttle throughput (e.g., 100 KB/s)
- **Packet loss** - Drop percentage of packets

**Manual Control:**

```bash
# Start Toxiproxy
docker run -d --name toxiproxy -p 8474:8474 ghcr.io/shopify/toxiproxy:2.5.0

# List proxies
curl http://localhost:8474/proxies | jq

# Add latency
curl -X POST http://localhost:8474/proxies/node_1/toxics \
  -d '{"type":"latency","attributes":{"latency":200}}'

# Partition (disable proxy)
curl -X POST http://localhost:8474/proxies/node_1 -d '{"enabled":false}'

# Heal (enable proxy)
curl -X POST http://localhost:8474/proxies/node_1 -d '{"enabled":true}'
```

### 4. Property-Based Testing

Uses `proptest` for exhaustive scenario coverage:

```rust
proptest! {
    #[test]
    fn test_cluster_survives_random_failures(
        cluster_size in 3..7usize,
        failures in prop::collection::vec(0..6u64, 0..3)
    ) {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut cluster = spawn_test_cluster(cluster_size).await.unwrap();
            // Apply random failures
            // Verify cluster survives if quorum maintained
        });
    }
}
```

**Benefits:**
- Finds edge cases humans miss
- Tests arbitrary inputs
- Configurable case count (`PROPTEST_CASES=100`)
- Shrinking on failure (minimal reproducing case)

## Usage

### Prerequisites

```bash
# 1. Build Chronik server (required)
cargo build --release --bin chronik-server

# 2. (Optional) Pull Toxiproxy image
docker pull ghcr.io/shopify/toxiproxy:2.5.0
```

### Running Tests

```bash
# All tests
cargo test --manifest-path tests/raft/Cargo.toml

# Specific test file
cargo test --test test_leader_election --manifest-path tests/raft/Cargo.toml

# Specific test function
cargo test --test test_leader_election test_initial_leader_election

# With output and debug logs
RUST_LOG=debug cargo test --test test_leader_election -- --nocapture

# Property tests with more cases
PROPTEST_CASES=100 cargo test proptests
```

### Writing Tests

```rust
mod common;

use anyhow::Result;
use common::*;
use std::time::Duration;

#[tokio::test]
async fn test_my_scenario() -> Result<()> {
    // 1. Spawn cluster
    let mut cluster = spawn_test_cluster(3).await?;

    // 2. Wait for leader
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    // 3. Test logic
    cluster.kill_node(leader_id).await?;
    let new_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    // 4. Assertions
    assert_ne!(new_leader, leader_id);

    // 5. Cleanup
    cluster.cleanup().await;
    Ok(())
}
```

## Testing Scenarios Covered

### Leader Election

- [x] Initial election in new cluster
- [x] Re-election after leader crash
- [x] Re-election after leader network isolation
- [x] Majority requirement for election
- [x] Rapid successive leader failures
- [x] Different election timeout configurations
- [x] Simultaneous node failures
- [x] Random failure scenarios (property-based)

### Replication

- [x] Basic produce→replicate→consume flow
- [x] Replication after follower restart
- [x] Follower catch-up after lag
- [x] Consistency across all nodes
- [x] Arbitrary message sequences (property-based)

### Network Partitions

- [x] Majority partition elects leader
- [x] Minority partition cannot elect leader
- [x] Partition healing converges to single leader
- [x] Symmetric partition (split-brain prevention)
- [x] Cascading partitions
- [x] Leader isolated in minority
- [x] Flapping network (rapid partition/heal)

### Fault Injection

- [x] Node crashes
- [x] Node restarts
- [x] Network partitions
- [x] Network latency
- [x] Multiple concurrent failures

## Test Guarantees

### Idempotency

All tests are idempotent:
- Fresh `TempDir` for each node
- Dynamic port allocation (no hardcoded ports)
- Automatic cleanup on test completion
- No shared state between tests

### Fast Failure

All tests have timeouts:
- `wait_for_leader` has timeout parameter
- `wait_for_consensus` has timeout parameter
- Tests fail fast (no 5-minute hangs)
- Default timeout: 10 seconds for most operations

### Isolation

Tests are isolated:
- Each test spawns its own cluster
- No shared infrastructure
- Parallel execution safe (different ports)
- Docker containers are per-test

## Performance

### Test Execution Times

Typical execution times (on Apple M1):

- `test_initial_leader_election`: ~2s
- `test_leader_crash_reelection`: ~5s
- `test_basic_replication_3_nodes`: ~3s
- `test_network_partition`: ~10s (includes Toxiproxy startup)
- Full suite: ~2-3 minutes

### Optimization Tips

```bash
# Build release binary (faster node startup)
cargo build --release --bin chronik-server

# Reduce proptest cases during development
PROPTEST_CASES=5 cargo test proptests

# Use fewer nodes when possible
spawn_test_cluster(3) // Instead of 5 or 7

# Run tests in parallel (default)
cargo test -- --test-threads=4
```

## Debugging

### Enable Logging

```bash
# All debug logs
RUST_LOG=debug cargo test --test test_leader_election -- --nocapture

# Raft-specific logs
RUST_LOG=chronik_raft=trace cargo test --test test_leader_election -- --nocapture

# Multiple targets
RUST_LOG=chronik_raft=debug,chronik_server=info cargo test
```

### Inspect Running Test

Add sleep and query via HTTP:

```rust
let cluster = spawn_test_cluster(3).await?;
let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

println!("Sleeping 60s for inspection...");
sleep(Duration::from_secs(60)).await;
```

Then in another terminal:

```bash
# Check all nodes
for port in 8000 8001 8002; do
  curl -s http://localhost:$port/api/v1/raft/info | jq '.node_id, .is_leader, .term'
done
```

### Common Issues

| Issue | Solution |
|-------|----------|
| Tests hang | Check for missing timeouts, add `RUST_TEST_TIMEOUT=60` |
| Port conflicts | Kill processes: `lsof -ti :9000 \| xargs kill -9` |
| Binary not found | Run: `cargo build --release --bin chronik-server` |
| Docker errors | Verify Docker is running: `docker ps` |
| Flaky tests | Run single-threaded: `cargo test -- --test-threads=1` |

## Future Enhancements

### Planned Test Scenarios

- [ ] Multi-partition replication (multiple topics)
- [ ] Log compaction during leader changes
- [ ] Snapshot transfer to lagging followers
- [ ] Configuration changes (add/remove nodes)
- [ ] Client request retries during election
- [ ] Linearizable reads
- [ ] Membership changes (joint consensus)
- [ ] Large message replication (>1MB)
- [ ] Disk full scenarios
- [ ] Clock skew between nodes

### Planned Fault Injection

- [ ] Packet reordering
- [ ] Bandwidth limits
- [ ] Disk I/O throttling
- [ ] Memory pressure
- [ ] CPU throttling
- [ ] Asymmetric partitions (A→B works, B→A fails)

### Infrastructure Improvements

- [ ] Test result reporting (JUnit XML)
- [ ] Coverage metrics
- [ ] Performance benchmarks
- [ ] Chaos testing mode (random faults)
- [ ] Visual cluster state viewer
- [ ] Automated flaky test detection

## References

### Documentation

- `README.md` - Main documentation with detailed explanations
- `EXAMPLES.md` - Practical examples and usage patterns
- `QUICK_REFERENCE.md` - One-page cheat sheet

### External Resources

- [Raft Paper](https://raft.github.io/raft.pdf) - Consensus algorithm
- [Toxiproxy](https://github.com/Shopify/toxiproxy) - Fault injection
- [Proptest](https://proptest-rs.github.io/proptest/) - Property-based testing
- [Testcontainers](https://docs.rs/testcontainers/latest/testcontainers/) - Docker in tests
- [Tokio Testing](https://tokio.rs/tokio/topics/testing) - Async test patterns

## Summary

The Raft testing infrastructure provides production-grade testing capabilities:

✅ **Comprehensive** - 20+ test scenarios covering all major Raft behaviors
✅ **Reliable** - Idempotent, isolated, fast-failing tests
✅ **Realistic** - Toxiproxy fault injection mirrors real-world failures
✅ **Thorough** - Property-based testing finds edge cases
✅ **Fast** - Optimized for quick iteration (<3 min full suite)
✅ **Well-documented** - Multiple guides for all skill levels
✅ **Production-ready** - Used for validating Chronik Raft clustering

**Quick Start:**

```bash
# 1. Build server
cargo build --release --bin chronik-server

# 2. Run tests
cargo test --manifest-path tests/raft/Cargo.toml

# 3. Read examples
cat tests/raft/EXAMPLES.md
```

**Next Steps:**

1. Review test scenarios in individual test files
2. Run tests to verify setup
3. Write custom tests for specific scenarios
4. Enable Toxiproxy for advanced fault injection
5. Use property-based testing for exhaustive coverage

---

**Maintained by:** Chronik Stream Contributors
**License:** Apache-2.0
**Version:** 1.0.0 (Parallel with Chronik v1.3.65)
