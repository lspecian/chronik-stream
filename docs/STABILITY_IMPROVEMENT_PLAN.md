# Chronik Stream Stability Improvement Plan

## Date: 2025-11-15

## Executive Summary

Recent stability issues (deadlocks, segfaults, partial consumption) indicate we need a comprehensive approach to stability. This document outlines immediate, medium-term, and long-term improvements.

## Root Cause Analysis

### What Went Wrong

1. **Insufficient Integration Testing**: Features tested in isolation, not under realistic cluster load
2. **Concurrency Bugs Not Caught**: DashMap deadlock, try_lock() issues only surfaced at 64+ concurrency
3. **Incomplete Features**: Heartbeat sending, watermark sync partially implemented
4. **No Stability Regression Testing**: No automated suite to catch stability regressions before release

### Pattern of Failures

All recent bugs share common characteristics:
- **Hidden under light load**: Work fine with 1-10 clients
- **Manifest at scale**: Break at 64+ concurrent connections
- **Cluster-specific**: Related to Raft + WAL + metadata coordination
- **Timing-dependent**: Deadlocks occur after minutes/hours

## Immediate Actions (This Week)

### 1. ✅ Stability Test Suite (Created)

**File**: `tests/stability_test_suite.sh`

**Coverage**:
- Cluster startup stability
- Basic produce-consume (1K messages)
- High concurrency (64 producers)
- Cluster responsiveness after load
- Deadlock detection (CPU monitoring)
- Log analysis for critical errors
- Memory leak detection
- Consumer group coordination

**Run before every commit**:
```bash
./tests/stability_test_suite.sh
```

**CI Integration**: Add to GitHub Actions to run on every PR

### 2. Soak Testing

Create long-running stability tests:

**24-Hour Soak Test**:
```bash
# Run cluster under constant load for 24 hours
./tests/soak_test_24h.sh
```

**Test Scenarios**:
- Constant 32 concurrent producers
- Random produce/consume cycles
- Partition leader failover
- Node restart scenarios
- Memory/CPU monitoring every hour

### 3. Deadlock Detection Tooling

**File**: `scripts/detect_deadlocks.sh`

```bash
#!/bin/bash
# Check all chronik-server processes for deadlock symptoms

for pid in $(pgrep chronik-server); do
    cpu=$(ps -p $pid -o %cpu= | awk '{print $1}')
    if [ "$cpu" = "0.0" ]; then
        echo "⚠️  WARNING: PID $pid has 0% CPU (possible deadlock)"
        # Print stack trace
        gdb -batch -ex "thread apply all bt" -p $pid 2>/dev/null
    fi
done
```

Run every 5 minutes in CI/staging.

### 4. Lock Contention Audit

**Action**: Audit all lock usage in critical paths

**Priority Locations**:
- `raft_cluster.rs` - `raft_node` lock
- `wal_replication.rs` - `last_heartbeat` DashMap
- `group_commit.rs` - `pending` queue lock
- `metadata_store.rs` - metadata state machine lock

**Rule**: Never hold multiple locks simultaneously. Use lock-free atomics where possible.

## Medium-Term Improvements (Next 2 Weeks)

### 1. Structured Concurrency Testing

**Framework**: Use `loom` for deterministic concurrency testing

```rust
// Add to Cargo.toml
[dev-dependencies]
loom = "0.7"

// Example test
#[test]
fn test_dashmap_concurrent_remove() {
    loom::model(|| {
        let map = Arc::new(DashMap::new());
        map.insert("key", 1);

        // Concurrent remove while iterating
        let map1 = map.clone();
        let map2 = map.clone();

        let t1 = loom::thread::spawn(move || {
            for entry in map1.iter() {
                // Should NOT remove here
            }
        });

        let t2 = loom::thread::spawn(move || {
            map2.remove("key");
        });

        t1.join().unwrap();
        t2.join().unwrap();
    });
}
```

**Target**: 100% coverage of concurrent code paths

### 2. Observability Improvements

**Metrics to Add**:
- Lock contention counters (how often try_lock fails)
- Queue depth histograms (detect backpressure early)
- Fetch latency p50/p95/p99 (catch slow fetches)
- Heartbeat intervals (detect missed heartbeats)
- Raft apply lag (detect Raft bottlenecks)

**Dashboard**: Grafana dashboard with:
- Red: Deadlock indicators (0% CPU, stuck threads)
- Yellow: Warnings (high queue depth, slow fetches)
- Green: Health (throughput, latency, success rate)

### 3. Fuzzing

**Use `cargo-fuzz` for protocol fuzzing**:

```bash
cargo install cargo-fuzz
cargo fuzz init

# Fuzz Kafka protocol parsing
cargo fuzz run fuzz_kafka_produce
cargo fuzz run fuzz_kafka_fetch
```

**Scenarios**:
- Malformed Kafka frames
- Out-of-order requests
- Concurrent metadata updates
- Partition leader changes during produce

### 4. Architectural Improvements

**Problem**: Too much lock contention between Raft + WAL + metadata

**Solution**: Separate coordination domains

```
Before:
  raft_node.lock() → metadata.lock() → wal.lock() (3-way deadlock risk)

After:
  raft_domain (metadata only) → channels → wal_domain (data only)
  No shared locks between domains
```

**Implementation**:
- Raft only coordinates metadata (topics, partitions, leaders)
- WAL replication uses its own channel-based coordination
- Metadata changes flow through channels (already started with election triggers)

## Long-Term Strategy (Next Month)

### 1. Chaos Engineering

**Use `chaos-mesh` to inject failures**:

```yaml
# chaos-experiment.yaml
apiVersion: chaos-mesh.org/v1alpha1
kind: NetworkChaos
metadata:
  name: chronik-partition
spec:
  action: partition
  mode: all
  selector:
    namespaces:
      - chronik
  duration: "30s"
```

**Scenarios**:
- Network partitions (split-brain)
- Pod crashes (leader failover)
- Disk slowness (fsync latency)
- Clock skew (timeout issues)

### 2. Property-Based Testing

**Use `proptest` for invariant checking**:

```rust
proptest! {
    #[test]
    fn produce_consume_roundtrip(messages in prop::collection::vec(any::<Vec<u8>>(), 1..1000)) {
        // Property: All produced messages are consumable
        let produced = produce_messages(&messages);
        let consumed = consume_messages();
        prop_assert_eq!(produced, consumed);
    }
}
```

**Invariants to Test**:
- Messages produced = messages consumable
- No message duplication
- No message loss
- Offset monotonicity
- ISR membership consistency

### 3. Formal Verification (Raft)

**Consider TLA+ for Raft protocol**:

```tla
THEOREM RaftSafety ==
    \A i, j \in Server:
        currentTerm[i] = currentTerm[j] =>
            \/ votedFor[i] = Nil
            \/ votedFor[j] = Nil
            \/ votedFor[i] = votedFor[j]
```

Use model checker to verify:
- No split-brain
- Leader election safety
- Log consistency

### 4. Code Review Checklist

**Mandatory checklist for PRs**:

- [ ] Adds unit tests (90%+ coverage)
- [ ] Adds integration test (realistic workload)
- [ ] Passes stability test suite (`./tests/stability_test_suite.sh`)
- [ ] No new locks added (or justification + audit)
- [ ] Metrics added for new code paths
- [ ] Load tested at 64+ concurrent clients
- [ ] Memory leak checked (`valgrind` or `heaptrack`)
- [ ] Reviewed for OWASP Top 10 vulnerabilities

## Development Process Changes

### 1. Definition of Done

**Code is NOT done until**:
1. ✅ Unit tests pass
2. ✅ Integration tests pass
3. ✅ **Stability test suite passes**
4. ✅ **Load tested at 128 concurrent clients**
5. ✅ Code reviewed by 2+ engineers
6. ✅ Metrics dashboard shows green
7. ✅ Documentation updated

### 2. Release Criteria

**Before ANY release**:
1. ✅ 24-hour soak test passes
2. ✅ No deadlocks detected
3. ✅ No memory leaks
4. ✅ All integration tests green
5. ✅ Performance regression < 5%

### 3. Rollback Strategy

**Always have a rollback plan**:
- Keep previous stable release binary
- Use canary deployments (5% → 50% → 100%)
- Monitor error rates for 1 hour after each stage
- Auto-rollback if error rate > 1%

## Specific Stability Improvements Needed

### 1. Fix Large Batch Consumption

**Issue**: 5K messages only consumed 4353/5000 (timeout after 30s)

**Root Cause Investigation Needed**:
```bash
# Enable trace logging for fetch handler
RUST_LOG=chronik_server::fetch_handler=trace ./target/release/chronik-server
```

**Likely causes**:
- Fetch pagination not working correctly
- High watermark not syncing fast enough
- Consumer group rebalancing during consumption

**Action**: Create dedicated test and fix

### 2. Watermark Synchronization

**Problem**: Watermarks may not sync immediately across cluster

**Solution**: Add watermark sync protocol
- Leader broadcasts watermark updates every 1s
- Followers ack receipt
- Producer waits for quorum ack before returning

### 3. Backpressure Refinement

**Current**: Simple queue depth check

**Needed**: Adaptive backpressure
- Monitor queue depth trend (increasing/decreasing)
- Adjust batch size based on queue pressure
- Add circuit breaker for sustained overload

## Monitoring Dashboard Requirements

### Real-Time Metrics

**Must-Have Metrics**:
1. **Throughput**: msgs/sec produced, consumed
2. **Latency**: p50/p95/p99 produce, fetch
3. **Error Rate**: failures/sec, timeouts/sec
4. **Queue Depth**: WAL queue, Raft queue
5. **Lock Contention**: try_lock failures/sec
6. **CPU/Memory**: per-node usage
7. **Deadlock Indicator**: 0% CPU processes

**Alerts**:
- 🔴 CRITICAL: Any process with 0% CPU for > 30s
- 🔴 CRITICAL: Error rate > 1%
- 🟡 WARNING: Queue depth > 80%
- 🟡 WARNING: p99 latency > 1s

## Success Metrics

### Short-Term (1 Week)

- [ ] Stability test suite passes 100%
- [ ] No deadlocks in 24-hour soak test
- [ ] Fix large batch consumption issue
- [ ] All lock contention audited

### Medium-Term (2 Weeks)

- [ ] Loom tests added for all concurrent code
- [ ] Metrics dashboard operational
- [ ] Fuzzing finds 0 new bugs
- [ ] Performance regression < 5%

### Long-Term (1 Month)

- [ ] Chaos engineering tests pass
- [ ] Property-based tests cover all invariants
- [ ] 7-day soak test passes
- [ ] Zero production incidents

## Conclusion

Stability is **NON-NEGOTIABLE**. We will:

1. **Stop adding features** until stability improves
2. **Run stability tests** before every commit
3. **Fix bugs completely** (not quick hacks)
4. **Test at scale** (64+ concurrent clients minimum)
5. **Monitor proactively** (catch issues before they manifest)

**Next Steps**:
1. Run stability test suite: `./tests/stability_test_suite.sh`
2. Fix any failures immediately
3. Add to CI pipeline
4. Create 24-hour soak test
5. Begin lock contention audit

Stability first. Features second.
