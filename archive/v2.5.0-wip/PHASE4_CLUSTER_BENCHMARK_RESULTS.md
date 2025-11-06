# Chronik v2.5.0 Phase 4: Cluster Benchmark Results

## Executive Summary

⚠️ **Phase 4 Cluster Testing**: 2-node/3-node cluster tests **NOT COMPLETED** due to replication port configuration issues

**What Was Tested**:
- ✅ Standalone mode: acks=1 (53K msg/s)
- ✅ Standalone mode: acks=-1 (46K msg/s)
- ✅ 2-node cluster: acks=1 (53K msg/s)
- ❌ 2-node/3-node cluster: acks=-1 (ISR quorum timeout - replication not wired correctly)
- ❌ Mixed workload (acks=1 + acks=-1)
- ❌ Consumer benchmarks (incomplete)

---

## Test Configuration

### Hardware
- **Platform**: Linux 6.11.0-28-generic
- **Environment**: Native Ubuntu (no Docker)
- **Test Date**: 2025-10-31

### Software
- **Chronik Version**: v2.5.0 Phase 4
- **Test Client**: chronik-bench (rdkafka)
- **Message Size**: 256B
- **Concurrency**: 128 producers
- **Duration**: 30s + 5s warmup

---

## Benchmark Results

### Test 1: Standalone Mode - acks=1 ✅

| Metric | Value |
|--------|-------|
| **Throughput** | 53,227 msg/s |
| **Bandwidth** | 13.00 MB/s |
| **Total Messages** | 1,863,019 |
| **Failed** | 0 (100% success) |
| **Latency p50** | 1.95ms |
| **Latency p99** | 5.34ms |
| **Latency p99.9** | 10.49ms |
| **Max Latency** | 19.15ms |

### Test 2: Standalone Mode - acks=-1 ✅

| Metric | Value |
|--------|-------|
| **Throughput** | 46,130 msg/s |
| **Bandwidth** | 11.26 MB/s |
| **Total Messages** | 1,614,659 |
| **Failed** | 0 (100% success) |
| **Latency p50** | 1.97ms |
| **Latency p99** | 15.51ms |
| **Latency p99.9** | 28.51ms |
| **Max Latency** | 87.87ms |

**acks=-1 Overhead (Standalone)**:
- Throughput: -13.3%
- p50 latency: +1.0%
- p99 latency: +190.4%

**Note**: In standalone mode, acks=-1 is effectively acks=1 (ISR size = 1, quorum = 1/1). The overhead comes from IsrAckTracker logic, not actual replication.

### Test 3: 2-Node Cluster - acks=1 ✅

| Metric | Value |
|--------|-------|
| **Throughput** | 53,411 msg/s |
| **Bandwidth** | 13.04 MB/s |
| **Total Messages** | 1,869,541 |
| **Failed** | 0 (100% success) |
| **Latency p50** | 1.94ms |
| **Latency p99** | 5.32ms |
| **Latency p99.9** | 11.78ms |
| **Max Latency** | 25.18ms |

**Result**: 2-node cluster with acks=1 performs identically to standalone (53K msg/s).

### Test 4: 2-Node Cluster - acks=-1 ❌ FAILED

**Status**: NOT COMPLETED

**Issue**: ISR quorum timeout errors

**Root Cause**:
```
ERROR: acks=-1: ISR quorum timeout for chronik-bench-0 offset 10342 after 30s
WARN: Failed to connect to follower localhost:9193: Connection refused
WARN: Failed to connect to follower localhost:9194: Connection refused
```

**Analysis**:
1. Node 1 configured with `CHRONIK_REPLICATION_FOLLOWERS="localhost:9193,localhost:9194"`
2. But actual nodes running on ports 9092 and 9094
3. WalReplicationManager cannot connect to non-existent followers
4. acks=-1 requests wait 30s for quorum, then timeout
5. **Throughput**: ~10 msg/s (500 messages produced in 50+ seconds before timeout)

**What This Means**:
- The acks=-1 implementation is partially working (it waits for quorum)
- But WAL replication between nodes is not properly configured
- Need to either:
  - (A) Start nodes with correct replication port mapping, OR
  - (B) Test with proper Raft cluster mode (`raft-cluster` subcommand), OR
  - (C) Document that acks=-1 only works in Raft mode, not standalone clustering

---

## Architecture Issues Discovered

### Issue 1: Standalone Mode Doesn't Support Multi-Node ISR

**Problem**: The `standalone` mode doesn't properly support WAL replication between multiple nodes.

**Current Implementation**:
- `CHRONIK_REPLICATION_FOLLOWERS` env var specifies follower addresses
- `WalReplicationManager` tries to connect to followers
- But in `standalone` mode, there's no Raft coordination
- Followers don't know they should act as followers
- No automatic ISR membership tracking

**Implications**:
- acks=-1 in standalone mode only makes sense for single-node deployments
- For multi-node acks=-1, users MUST use `raft-cluster` mode
- The standalone mode documentation needs to clarify this limitation

### Issue 2: Port Configuration Complexity

**Problem**: Setting up a 3-node cluster requires configuring multiple ports per node:
- Kafka port (9092, 9093, 9094)
- Metrics port (derived as kafka_port + 1, conflicts at 9093!)
- Search API port (6080, 6081, 6082)
- Raft port (9192, 9193, 9194) - only in raft-cluster mode
- Replication port (9193, 9194) - only in standalone with followers

**What Happened**:
- Node 1: kafka=9092, metrics=9093 ✅
- Node 2: kafka=9093, metrics=9093 ❌ PORT CONFLICT
- Node 3: kafka=9094, metrics=9095 ✅

**Root Cause**: Metrics port auto-derives as kafka_port + 1, causing conflicts when kafka_port overlaps with another node's metrics port.

---

## Recommended Next Steps

### Option A: Fix Standalone Multi-Node Replication

**Tasks**:
1. Add proper follower discovery mechanism to standalone mode
2. Make followers listen on their Kafka port for replication connections
3. Implement ISR membership tracking without Raft
4. Test 3-node standalone cluster with acks=-1
5. Benchmark and compare vs Raft cluster mode

**Estimated Time**: 4-6 hours

### Option B: Document Raft-Only Requirement

**Tasks**:
1. Update CLAUDE.md to clarify acks=-1 requires `raft-cluster` mode
2. Add error message in standalone mode when acks=-1 used with >1 node config
3. Remove `CHRONIK_REPLICATION_FOLLOWERS` from standalone mode
4. Update Phase 4 plan to focus on Raft cluster testing
5. Set up proper 3-node Raft cluster and benchmark acks=-1

**Estimated Time**: 1-2 hours + 2-3 hours for Raft cluster testing

### Option C: Hybrid Approach

**Tasks**:
1. Make standalone acks=-1 work ONLY for single-node (reject >1 followers)
2. For multi-node acks=-1, require `raft-cluster` mode
3. Update documentation to reflect this architecture decision
4. Complete Raft cluster benchmarks for multi-node acks=-1

**Estimated Time**: 2-3 hours

---

## Current Phase 4 Status

### What Works ✅
1. ✅ IsrAckTracker implementation (lock-free, DashMap-based)
2. ✅ ACK protocol (magic number, WalAckMessage struct)
3. ✅ Integration with ProduceHandler (oneshot channels for quorum wait)
4. ✅ Standalone single-node acks=-1 (46K msg/s, -13% vs acks=1)
5. ✅ Timeout handling (30s REPLICATION_TIMEOUT)
6. ✅ Error reporting ("ISR quorum timeout")

### What Doesn't Work ❌
1. ❌ Multi-node standalone mode with acks=-1 (replication not wired)
2. ❌ WAL follower discovery and connection setup
3. ❌ ISR membership tracking without Raft
4. ❌ Metrics port conflict resolution for 3-node clusters
5. ❌ Consumer benchmarks (not completed)

### What Wasn't Tested ⚠️
1. ⚠️ Raft cluster mode with acks=-1 (requires different setup)
2. ⚠️ Mixed workload (acks=1 + acks=-1 simultaneously)
3. ⚠️ Consumer performance from cluster
4. ⚠️ Replication lag metrics
5. ⚠️ Follower failover and recovery

---

## Performance Summary

**Standalone Mode** (Single Node):
- **acks=1**: 53K msg/s, p99=5.34ms
- **acks=-1**: 46K msg/s, p99=15.51ms
- **Overhead**: -13% throughput, +190% tail latency

**2-Node Cluster** (Kafka Protocol Load Balancing):
- **acks=1**: 53K msg/s, p99=5.32ms (identical to standalone)
- **acks=-1**: FAILED (ISR quorum timeout)

**3-Node Cluster**: NOT TESTED (port configuration issues)

---

## Conclusion

### Phase 4 Implementation: ✅ PARTIAL SUCCESS

**What Was Achieved**:
- Core acks=-1 infrastructure implemented correctly
- IsrAckTracker, ACK protocol, and timeout handling work
- Standalone single-node acks=-1 performs well
- Identifies architectural gap: multi-node ISR requires Raft

**What Needs Completion**:
- Multi-node acks=-1 testing (requires Raft cluster mode)
- WAL replication wiring for standalone multi-node
- Consumer benchmarks
- Mixed workload testing
- Architecture decision: standalone vs Raft for acks=-1

**Recommendation**: Phase 4 should be considered INCOMPLETE until multi-node acks=-1 is properly tested, either by:
1. Fixing standalone mode to support multi-node replication, OR
2. Documenting that acks=-1 multi-node requires Raft cluster mode and completing Raft cluster benchmarks

---

**Report Date**: October 31, 2025
**Status**: ⚠️ INCOMPLETE - Multi-node acks=-1 not functional
**Next Action**: Choose Option A, B, or C above and complete testing
