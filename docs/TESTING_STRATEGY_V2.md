# Chronik Testing Strategy V2 - Comprehensive Test Suite Plan

**Date:** 2025-11-08
**Status:** PROPOSAL
**Priority:** CRITICAL

---

## Executive Summary

The current test suite has **critical gaps** that allowed v2.2.0, v2.2.1, and v2.2.2 to be released with non-functional cluster mode. This document proposes a comprehensive testing strategy with:

1. **Regression test suite** - Prevent fixed bugs from reappearing
2. **TDD workflow** - Write tests before fixes
3. **Functional test pyramid** - Unit → Integration → E2E
4. **Load & performance tests** - Catch issues before production
5. **Automated cluster testing** - Long-running validation
6. **CI/CD integration** - Tests run automatically

---

## Current State Analysis

### What We Have ✅

- **336 unit tests** (`#[test]`)
- **333 async tests** (`#[tokio::test]`)
- **53 test files** (Rust)
- **~292 Python test files** (CHAOS - most are debug scripts)
- **Integration tests** for some features

### What We're Missing ❌

1. **Cluster mode tests** - No long-running cluster validation
2. **Regression tests** - Fixed bugs can reappear
3. **TDD workflow** - Tests written after code
4. **Performance benchmarks** - No latency/throughput tracking
5. **Load tests** - Can't validate at scale
6. **Chaos testing** - Node failures, network partitions
7. **Real client testing** - Not enough multi-language client tests
8. **Test organization** - 292 Python files, most duplicate/debug

---

## The Test Pyramid

```
                  ┌─────────────────┐
                  │   E2E Tests     │  ← 5% (Real clients, full cluster)
                  │  (Slow, Brittle)│
                  └─────────────────┘
                ┌────────────────────┐
                │ Integration Tests   │  ← 25% (Multi-component, Docker optional)
                │ (Medium speed)      │
                └────────────────────┘
          ┌──────────────────────────────┐
          │      Unit Tests              │  ← 70% (Fast, isolated)
          │   (Fast, Reliable)           │
          └──────────────────────────────┘
```

### Current Distribution (BROKEN)

- Unit Tests: ~40% (too few)
- Integration Tests: ~30% (disorganized)
- E2E Tests: ~5% (insufficient)
- Debug Scripts: ~25% (DELETE THIS)

### Target Distribution

- Unit Tests: ~70% (increase coverage)
- Integration Tests: ~25% (organize better)
- E2E Tests: ~5% (add critical paths)
- Debug Scripts: ~0% (delete all)

---

## Proposed Test Categories

### 1. Unit Tests (70% of suite)

**Goal**: Test individual components in isolation

**Location**: `crates/*/src/*.rs` (inline) or `crates/*/tests/`

**Coverage**:
- ✅ Protocol encoding/decoding
- ✅ Metadata operations
- ✅ Storage layer
- ❌ **MISSING**: Raft leadership logic
- ❌ **MISSING**: Partition assignment logic
- ❌ **MISSING**: Leader election service

**Example** (what we need):
```rust
#[test]
fn test_am_i_leader_returns_true_only_for_leader() {
    // Test the exact bug we just fixed!
    let raft = create_test_raft_leader();
    assert!(raft.am_i_leader());

    let raft_follower = create_test_raft_follower();
    assert!(!raft_follower.am_i_leader());  // v2.2.2 would FAIL this!
}
```

**New Tests Needed**:
- [ ] `test_is_leader_ready_returns_true_for_followers()` - Document the API behavior
- [ ] `test_am_i_leader_only_true_for_leader()` - Prevent v2.2.3 regression
- [ ] `test_partition_leader_assignment_prefers_raft_leader()` - Verify fix
- [ ] `test_meta_topic_uses_cluster_replication_factor()` - Verify RF=3 in cluster

---

### 2. Integration Tests (25% of suite)

**Goal**: Test multi-component interactions

**Location**: `tests/integration/`

**Categories**:

#### 2a. Cluster Integration Tests ⭐ CRITICAL

**Missing**: Long-running cluster validation

**New Tests Needed**:

```rust
// tests/integration/cluster_partition_leadership.rs
#[tokio::test]
#[ignore] // Run explicitly: cargo test --test integration -- --ignored
async fn test_cluster_partition_leadership_after_30_seconds() {
    // THE TEST THAT WOULD HAVE CAUGHT v2.2.0/v2.2.1/v2.2.2 BUGS

    // 1. Start 3-node cluster
    let cluster = start_3_node_cluster().await;

    // 2. Wait for Raft leader election
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 3. Create test topic
    cluster.create_topic("test", 3).await;

    // 4. CRITICAL: Wait for leader election service to run (12+ seconds)
    tokio::time::sleep(Duration::from_secs(30)).await;

    // 5. Verify NO errors
    let logs = cluster.get_all_logs().await;
    assert_no_errors_matching(&logs, "Cannot propose");
    assert_no_errors_matching(&logs, "Failed to elect leader");

    // 6. Verify partition leaders are valid
    for partition in 0..3 {
        let leader = cluster.get_partition_leader("test", partition).await;
        let is_raft_leader = cluster.is_raft_leader(leader).await;
        // After v2.2.3, most partitions should have Raft leader as partition leader
        // (Not 100% guaranteed, but high probability with 3 nodes)
    }
}
```

**Key Insight**: Our v2.2.1 test only ran for 15 seconds. The leader election service runs every 12 seconds. **We needed to wait 30+ seconds** to catch the bug!

#### 2b. Consumer Group Tests

**Status**: Disorganized (10+ duplicate files)

**Consolidate to**:
- `test_consumer_group_basic_kafka_python.rs`
- `test_consumer_group_rebalance.rs`
- `test_consumer_group_offset_commit.rs`

#### 2c. WAL Recovery Tests

**Status**: Exists but incomplete

**Add**:
- [ ] Test recovery after crash during flush
- [ ] Test recovery with corrupt segment
- [ ] Test recovery with V1 and V2 records mixed

#### 2d. Client Compatibility Tests

**Status**: Ad-hoc

**Organize**:
```
tests/compatibility/
├── kafka_python/
│   ├── test_produce_consume.rs
│   ├── test_consumer_groups.rs
│   └── test_admin_api.rs
├── confluent_kafka/  # librdkafka
│   └── test_suite.rs
├── java/
│   ├── test_kafka_clients.rs
│   └── test_ksqldb_integration.rs
└── rust/
    └── test_rdkafka.rs
```

---

### 3. Regression Tests (Dedicated Suite)

**Goal**: Ensure fixed bugs never reappear

**Location**: `tests/regression/`

**Structure**:
```
tests/regression/
├── v1_3_28_crc_fix/
│   └── test_crc32c_preserved.rs
├── v1_3_63_wal_checksum/
│   └── test_wal_v2_checksum_skip.rs
├── v2_2_3_partition_leadership/  ← NEW!
│   ├── test_am_i_leader_api.rs
│   ├── test_only_leader_runs_elections.rs
│   ├── test_prefer_raft_leader_as_partition_leader.rs
│   └── test_meta_topic_replication_factor.rs
└── consumer_group_rebalance/
    └── test_rebalance_protocol.rs
```

**Process**:
1. **When a bug is fixed**, create a regression test IMMEDIATELY
2. Test must fail WITHOUT the fix
3. Test must pass WITH the fix
4. Tag with issue number: `#[regression("v2.2.3", "partition-leadership")]`

**Example**:
```rust
// tests/regression/v2_2_3_partition_leadership/test_am_i_leader_api.rs

#[test]
#[regression("v2.2.3", "partition-leadership")]
fn test_am_i_leader_returns_false_for_followers() {
    // This test would FAIL in v2.2.2 due to is_leader_ready() misuse

    let cluster = create_test_cluster();
    let leader = cluster.get_leader();
    let follower = cluster.get_follower();

    assert!(leader.am_i_leader());
    assert!(!follower.am_i_leader());  // v2.2.2 bug: would return true!
}
```

---

### 4. E2E Tests (5% of suite)

**Goal**: Test complete user workflows with real clients

**Location**: `tests/e2e/`

**Categories**:

#### 4a. Single-Node E2E
- [ ] Producer → Consumer (Python client)
- [ ] KSQLDB stream processing
- [ ] Admin API operations
- [ ] Search API

#### 4b. Cluster E2E ⭐ CRITICAL
- [ ] 3-node cluster full workflow
- [ ] Producer to all 3 nodes
- [ ] Consumer from all 3 nodes
- [ ] Node failure and recovery
- [ ] Partition rebalancing
- [ ] Admin API (add/remove node)

**Example**:
```python
# tests/e2e/test_cluster_full_workflow.py

def test_cluster_produce_consume_with_node_failure():
    """
    End-to-end test that would have caught v2.2.0/v2.2.1/v2.2.2 bugs
    """
    # 1. Start 3-node cluster
    cluster = start_cluster(nodes=3)

    # 2. Create topic (auto-creation via produce)
    producer = KafkaProducer(bootstrap_servers=['localhost:9092'])
    producer.send('test-topic', b'message1')
    producer.flush()

    # 3. Wait for leader election service (CRITICAL!)
    time.sleep(30)  # v2.2.1 test failed because we didn't wait!

    # 4. Check logs for errors
    logs = cluster.get_all_logs()
    assert 'Cannot propose' not in logs  # v2.2.0/v2.2.1/v2.2.2 FAIL
    assert 'Failed to elect leader' not in logs  # v2.2.0/v2.2.1/v2.2.2 FAIL

    # 5. Verify can consume
    consumer = KafkaConsumer('test-topic', ...)
    messages = list(consumer)
    assert len(messages) == 1
```

---

### 5. Performance & Load Tests

**Goal**: Catch performance regressions and validate scale

**Location**: `tests/performance/`

**Tests Needed**:

#### 5a. Throughput Benchmarks
```rust
#[bench]
fn bench_produce_throughput_single_partition() {
    // Measure: messages/second
    // Target: 100K msg/s (single partition)
}

#[bench]
fn bench_produce_throughput_multi_partition() {
    // Measure: messages/second across 10 partitions
    // Target: 500K msg/s (10 partitions)
}
```

#### 5b. Latency Benchmarks
```rust
#[bench]
fn bench_produce_latency_p99() {
    // Measure: p50, p95, p99 latency
    // Target: p99 < 10ms (single-node)
    // Target: p99 < 50ms (cluster)
}
```

#### 5c. Load Tests
```python
# tests/performance/load_test_cluster.py

def test_cluster_under_sustained_load():
    # 1. Start 3-node cluster
    # 2. Run 10 producers × 1000 msg/s each = 10K msg/s
    # 3. Run for 10 minutes
    # 4. Verify:
    #    - No errors
    #    - No memory leaks
    #    - Latency stable
    #    - All messages consumed
```

#### 5d. Chaos Testing
```python
def test_cluster_with_random_node_failures():
    # 1. Start 3-node cluster
    # 2. Run producers/consumers
    # 3. Randomly kill/restart nodes
    # 4. Verify cluster remains functional
```

---

## TDD Workflow

**New Process for ALL Features/Fixes**:

### Step 1: Write Failing Test FIRST
```rust
// BEFORE writing any fix code

#[test]
#[should_panic]  // Or use assert! that will fail
fn test_partition_leader_election_only_on_raft_leader() {
    let cluster = create_3_node_cluster();

    // This SHOULD fail in v2.2.2
    let follower = cluster.get_follower(1);

    // Simulate leader election service running on follower
    let result = follower.trigger_partition_election("test", 0);

    // Test PASSES if follower correctly SKIPS election
    assert!(result.is_ok());
    assert!(result.unwrap() == ElectionResult::Skipped);
}
```

### Step 2: Implement Fix
```rust
// crates/chronik-server/src/leader_election.rs

if !raft_cluster.am_i_leader() {
    return Ok(ElectionResult::Skipped);  // Fix implementation
}
```

### Step 3: Verify Test Passes
```bash
cargo test test_partition_leader_election_only_on_raft_leader
```

### Step 4: Add to Regression Suite
```bash
mv test_partition_leader_election_only_on_raft_leader.rs \
   tests/regression/v2_2_3_partition_leadership/
```

---

## Test Infrastructure

### Test Utilities We Need

#### 1. Cluster Test Harness
```rust
// tests/common/cluster_harness.rs

pub struct TestCluster {
    nodes: Vec<TestNode>,
    data_dir: TempDir,
}

impl TestCluster {
    pub async fn start_3_node() -> Self { ... }
    pub async fn create_topic(&self, name: &str, partitions: u32) { ... }
    pub async fn get_partition_leader(&self, topic: &str, partition: i32) -> u64 { ... }
    pub async fn get_all_logs(&self) -> Vec<String> { ... }
    pub async fn assert_no_errors(&self, pattern: &str) { ... }
    pub async fn kill_node(&mut self, node_id: u64) { ... }
    pub async fn start_node(&mut self, node_id: u64) { ... }
}
```

#### 2. Real Client Test Harness
```python
# tests/common/client_harness.py

class KafkaClientHarness:
    """Manage real Kafka client instances for testing"""

    def __init__(self, bootstrap_servers):
        self.producer = KafkaProducer(bootstrap_servers=bootstrap_servers)
        self.consumer = KafkaConsumer(bootstrap_servers=bootstrap_servers)

    def produce_and_verify(self, topic, messages):
        """Produce messages and verify they can be consumed"""
        ...
```

#### 3. Performance Measurement
```rust
// tests/common/perf.rs

pub struct PerfMeasurement {
    latencies: Vec<Duration>,
}

impl PerfMeasurement {
    pub fn record(&mut self, latency: Duration) { ... }
    pub fn p50(&self) -> Duration { ... }
    pub fn p95(&self) -> Duration { ... }
    pub fn p99(&self) -> Duration { ... }
    pub fn throughput(&self) -> f64 { ... }
}
```

---

## CI/CD Integration

### GitHub Actions Workflow

```yaml
# .github/workflows/test.yml

name: Test Suite

on: [push, pull_request]

jobs:
  unit-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run unit tests
        run: cargo test --workspace --lib --bins
      - name: Upload coverage
        uses: codecov/codecov-action@v3

  integration-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run integration tests
        run: cargo test --test integration

  cluster-tests:
    runs-on: ubuntu-latest
    timeout-minutes: 10  # CRITICAL: Allow time for long-running tests
    steps:
      - uses: actions/checkout@v4
      - name: Run cluster tests (long-running)
        run: cargo test --test cluster -- --ignored --test-threads=1
      - name: Upload logs on failure
        if: failure()
        uses: actions/upload-artifact@v3
        with:
          name: cluster-logs
          path: test-data/node*/logs/

  regression-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run regression tests
        run: cargo test --test regression

  compatibility-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Python clients
        run: pip install kafka-python confluent-kafka
      - name: Run compatibility tests
        run: python3 tests/run_compatibility_tests.py
```

### Pre-Commit Hooks

```bash
# .git/hooks/pre-commit

#!/bin/bash
set -e

echo "Running unit tests..."
cargo test --workspace --lib --bins

echo "Running cluster regression tests..."
cargo test --test regression::v2_2_3_partition_leadership

echo "All pre-commit tests passed ✓"
```

---

## Test Cleanup Plan

### Phase 1: Delete Debug Scripts (Week 1)

**Action**: Delete ~150 files

```bash
# tests/scripts/cleanup_debug_scripts.sh

# Delete all debug scripts
rm -rf tests/python/debug/
rm -rf tests/librdkafka_debug/
rm tests/debug_*.py
rm tests/analyze_*.py
rm tests/capture_*.py
rm tests/decode_*.py

# Delete duplicate consumer group tests (keep only canonical)
rm tests/consumer-group/test_consumer_debug.py
rm tests/consumer-group/test_consumer_fix.py
rm tests/consumer-group/test_consumer_final.py
# ... (keep test_consumer_basic.py)
```

**Expected Result**: 292 files → ~50 files

### Phase 2: Organize Remaining Tests (Week 2)

**Action**: Reorganize into standard structure

```
tests/
├── unit/              # Rust unit tests (inline or here)
├── integration/       # Multi-component tests
│   ├── cluster/
│   ├── consumer_groups/
│   ├── wal_recovery/
│   └── admin_api/
├── regression/        # Fixed bugs → tests
│   ├── v1_3_28_crc_fix/
│   ├── v1_3_63_wal_checksum/
│   └── v2_2_3_partition_leadership/
├── compatibility/     # Real client tests
│   ├── kafka_python/
│   ├── confluent_kafka/
│   └── java/
├── e2e/              # End-to-end workflows
│   ├── single_node/
│   └── cluster/
├── performance/      # Benchmarks & load tests
│   ├── throughput/
│   ├── latency/
│   └── chaos/
└── common/           # Shared test utilities
    ├── cluster_harness.rs
    ├── client_harness.py
    └── perf.rs
```

### Phase 3: Write Missing Tests (Weeks 3-4)

**Priority Order**:

1. ⭐ **Cluster regression tests** (v2.2.3 bugs)
2. ⭐ **Long-running cluster test** (30+ seconds)
3. Consumer group rebalance regression
4. WAL V2 checksum regression
5. CRC-32C preservation regression
6. Performance baseline benchmarks

---

## Test Naming Conventions

### Unit Tests
```rust
#[test]
fn test_<function_name>_<scenario>_<expected_result>()

// Examples:
fn test_am_i_leader_returns_true_for_leader()
fn test_am_i_leader_returns_false_for_follower()
fn test_partition_assignment_prefers_raft_leader_when_available()
```

### Integration Tests
```rust
#[tokio::test]
async fn test_<feature>_<scenario>()

// Examples:
async fn test_cluster_partition_leadership_after_30_seconds()
async fn test_consumer_group_rebalance_on_member_leave()
```

### Regression Tests
```rust
#[test]
#[regression("<version>", "<issue>")]
fn test_regression_<version>_<bug_name>()

// Examples:
#[regression("v2.2.3", "partition-leadership")]
fn test_regression_v2_2_3_am_i_leader_api()
```

---

## Success Metrics

### Current State (v2.2.3)
- **Test Count**: 669 tests
- **Coverage**: Unknown (need coverage tool)
- **Cluster Tests**: ~5 (insufficient)
- **Regression Tests**: 0 (none!)
- **CI/CD**: Minimal
- **Release Quality**: 3 broken cluster releases (v2.2.0/v2.2.1/v2.2.2)

### Target State (v2.3.0+)
- **Test Count**: 800+ tests
- **Coverage**: >80% for critical paths
- **Cluster Tests**: 20+ (including long-running)
- **Regression Tests**: 1 per fixed bug (10+ immediately)
- **CI/CD**: All tests automated
- **Release Quality**: 0 broken releases

### KPIs to Track
1. **Test Coverage** - % of code covered by tests
2. **Regression Test Count** - # of bugs with regression tests
3. **CI/CD Pass Rate** - % of builds passing tests
4. **Time to Detect Bugs** - How fast tests catch bugs
5. **Release Quality** - # of bugs in production

---

## Implementation Timeline

### Week 1: Foundation
- [ ] Delete debug scripts (~150 files)
- [ ] Create test directory structure
- [ ] Write cluster test harness
- [ ] Write v2.2.3 regression tests (4 tests)

### Week 2: Regression Suite
- [ ] Write v1.3.28 CRC regression test
- [ ] Write v1.3.63 WAL checksum regression test
- [ ] Write consumer group rebalance regression test
- [ ] Set up CI/CD for regression tests

### Week 3: Cluster Tests
- [ ] Write long-running cluster test (30+ seconds)
- [ ] Write cluster node failure test
- [ ] Write cluster partition rebalance test
- [ ] Integrate into CI/CD

### Week 4: Performance Tests
- [ ] Baseline throughput benchmarks
- [ ] Baseline latency benchmarks
- [ ] Load test framework
- [ ] Document performance targets

### Ongoing: TDD Process
- **Every bug fix**: Write failing test FIRST
- **Every feature**: Write tests BEFORE code
- **Every release**: Run FULL test suite

---

## Conclusion

The v2.2.0/v2.2.1/v2.2.2 cluster bugs teach us:

1. **Short tests miss bugs** - Need 30+ second cluster tests
2. **No regression tests = recurring bugs** - Need dedicated regression suite
3. **Manual testing insufficient** - Need automated CI/CD
4. **Code coverage gaps** - Need unit tests for ALL critical logic
5. **Missing TDD** - Tests should come BEFORE fixes

**Recommendation**: Implement this plan over next 4 weeks to prevent future cluster mode disasters.

The investment in testing infrastructure will pay dividends in:
- ✅ Faster development (catch bugs early)
- ✅ Fewer production bugs
- ✅ Confidence in releases
- ✅ Better code quality
- ✅ Easier refactoring

**Next Step**: Start with Week 1 - delete debug scripts and write v2.2.3 regression tests.
