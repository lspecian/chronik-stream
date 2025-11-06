# Branch Comparison: What to Take from perf/v2.4.1-rwlock-fix

**Current Branch**: `feat/v2.5.0-kafka-cluster`
**Reference Branch**: `perf/v2.4.1-rwlock-fix`

---

## Key Differences

### What perf/v2.4.1-rwlock-fix Has

1. **Complete chronik-raft crate** with proper architecture:
   - ISR manager (isr.rs - 926 lines)
   - Cluster coordinator
   - Group manager
   - Lease management
   - Snapshot support
   - Proper storage abstraction

2. **RaftMetaLog** in chronik-common:
   - Persistent Raft metadata storage
   - Integration with existing metadata store

3. **Performance fixes**:
   - RwLock removed from hot paths
   - Batch proposer for Raft
   - Optimized replication

### What Current Branch Has

1. **Simpler RaftCluster** (raft_cluster.rs):
   - Basic Raft node wrapper
   - Network messaging via TCP
   - Message loop implemented but not started

2. **Metadata proposals** working
3. **ISR tracking** (isr_tracker.rs, isr_ack_tracker.rs)
4. **Leader election** (leader_election.rs)

---

## What to Do

### Option 1: Cherry-pick from perf branch
Take the production-ready chronik-raft crate

### Option 2: Fix current implementation
Wire up what's already there (4 hours)

### Option 3: Merge branches
Combine both approaches

---

## My Recommendation

**Start current branch working FIRST (4 hours), THEN consider merging perf branch.**

Why:
1. Current branch is 95% done
2. Just needs message loop wired + quorum fixed
3. Can test end-to-end quickly
4. Then evaluate if perf improvements are needed

