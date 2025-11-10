# Raft log_unstable Panic - Node 3 Crash (v2.2.8)

## Summary

**CRITICAL BUG**: Node 3 crashes with a panic in the raft-rs library (`log_unstable.rs:201`) when trying to handle conflicting log entries. This causes the 3-node cluster to operate with only 2 nodes, leading to leadership instability and all downstream issues (auto-topic creation failures, poor performance, message timeouts).

## Timeline

**Discovered**: 2025-11-09 during cluster performance testing  
**Impact**: **CLUSTER MODE COMPLETELY BROKEN** - Node 3 crashes on startup after initial ConfChange  
**Status**: ❌ **BLOCKING** - Cluster cannot function with 2/3 nodes

## The Panic

**Location**: `raft-0.7.0/src/log_unstable.rs:201`  
**Error Message**:
```
found conflict at index 622, raft_id: 3, conflicting term: 4, existing term: 1, index: 622

thread 'tokio-runtime-worker' panicked at /home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/raft-0.7.0/src/log_unstable.rs:201:13:
unstable.slice[620, 622] out of bound[620, 620], raft_id: 3
```

**When it happens**: Node 3 receives AppendEntries RPC from leader (node 1) with entries that conflict with its local log at index 622.

## Root Cause Analysis

### What Raft is trying to do:

1. Leader (node 1) sends AppendEntries to follower (node 3)
2. Entries include index 622 at term 4
3. Node 3 has index 622 at term 1 (conflict!)
4. Raft tries to truncate node 3's log from index 622 onward
5. **BUG**: `log_unstable.slice[620, 622]` fails because unstable log only has entries [620, 620]

### Why the out-of-bounds error:

The raft-rs library assumes that if there's a conflict at index 622, the unstable log contains entries from some earlier index up to at least index 622. But in this case:
- `unstable.offset` = 620
- `unstable.entries.len()` = 0 (no unstable entries)
- Trying to slice [620, 622) fails because there are no entries

### Likely cause:

This is a known issue in raft-rs 0.7.0 related to log truncation when there are conflicting entries. The library doesn't handle the case where:
1. The conflicting entry is beyond the unstable log range
2. The entry needs to be removed from stable storage, not unstable

## Evidence

### Node 3 logs (18:29:47):

```
2025-11-09T18:29:47.759308Z INFO raft::raft_log: found conflict at index 622, raft_id: 3, conflicting term: 4, existing term: 1, index: 622

thread 'tokio-runtime-worker' panicked at raft-0.7.0/src/log_unstable.rs:201:13:
unstable.slice[620, 622] out of bound[620, 620], raft_id: 3
```

### Cluster state after crash:

```bash
$ ps aux | grep chronik-server
ubuntu   3135704  ... chronik-server start --config node1.toml  # ✓ Running
ubuntu   3135724  ... chronik-server start --config node2.toml  # ✓ Running
#                     NO NODE 3!                                # ❌ Crashed
```

### Leadership instability (node 1 logs 18:29:48):

```
18:29:48.264 became follower at term 5   ← Lost leadership
18:29:48.345 Failed to auto-create topic: not the leader ← Auto-create fails
18:29:48.961 became follower at term 6
18:29:49.566 became follower at term 7
18:29:50.065 became follower at term 8
18:29:52.161 became leader at term 11   ← Regains leadership
```

**Why**: With only 2 nodes, there's minimal quorum. Node 2 starts elections because it's missing heartbeats, node 1 rejects votes because it has more entries (index 625 vs 622), cluster flip-flops.

## Downstream Impact

### 1. Auto-Topic Creation Failures

**Error**: `Failed to auto-create topic: Cannot propose: this node is not the leader (state=Follower, leader=0)`

**Why**: During leadership transitions (4-second window), requests arrive when node 1 is temporarily a follower.

### 2. Poor Performance

**Benchmark results**:
- Only 55 msg/s vs standalone 50K+ msg/s (~900x slower!)
- High latency: 153ms p50 vs standalone 2.47ms p50
- 3.18% message timeouts

**Why**: 
- With 2 nodes, every Raft proposal needs both nodes to ack
- Node 2 is 3 entries behind, causing delays
- Leadership instability adds latency to every operation

### 3. Leadership Instability

**Symptoms**:
- Node 1 loses and regains leadership repeatedly
- Terms jump from 4 → 5 → 6 → 7 → 8 → 11 in 4 seconds
- No stable leader for clients to route to

**Why**: 2-node cluster has minimal fault tolerance. Any delay triggers election.

## Attempted Workarounds

### Option 1: Downgrade raft-rs

**Status**: Not tested yet  
**Approach**: Try raft-rs 0.6.x which may not have this bug  
**Risk**: May lose other features or introduce different bugs

### Option 2: Fix raft-rs 0.7.0

**Status**: Would require forking and patching  
**Approach**: Handle the case where conflicting entry is in stable storage  
**Risk**: Complex, touches core Raft algorithm

### Option 3: Prevent the conflict

**Status**: Under investigation  
**Approach**: Ensure all nodes have consistent initial state before joining  
**Risk**: May just delay the problem, not fix it

## Recommendation

**IMMEDIATE**: Document this as a known blocker for v2.2.8 cluster mode.

**SHORT-TERM**: 
1. Test with raft-rs 0.6.x to see if panic goes away
2. Report issue to raft-rs maintainers with reproduction case
3. Consider adding panic recovery to restart node 3 automatically

**LONG-TERM**:
1. Switch to a more mature Raft library (e.g., tikv/raft from TiKV)
2. Or contribute fix to raft-rs and wait for next release
3. Add comprehensive integration tests for log conflicts

## Related Issues

- [BROKER_REGISTRATION_BUG_v2.2.8.md](BROKER_REGISTRATION_BUG_v2.2.8.md) - ConfChange deadlock (FIXED)
- [INTEGRATED_SERVER_HANGS_v2.2.8.md](INTEGRATED_SERVER_HANGS_v2.2.8.md) - Original cluster hang report
- [V2.2.8_STATUS.md](V2.2.8_STATUS.md) - Overall v2.2.8 status

---

**Investigation Date**: 2025-11-09  
**Version**: v2.2.8 (pre-release)  
**Raft Version**: raft-rs 0.7.0  
**Status**: ❌ **BLOCKER** - Cluster mode unusable
