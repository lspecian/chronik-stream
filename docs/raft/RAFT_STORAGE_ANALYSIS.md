# Raft Storage: MemStorage vs Persistent - Do We Need It?

**Date**: 2025-11-01
**Question**: Should we replace MemStorage with persistent storage (RocksDB/file)?

---

## TL;DR

**For testing and getting cluster working**: MemStorage is FINE ✅

**For production**: Persistent storage is RECOMMENDED but not strictly required ⚠️

**Priority**: MEDIUM (implement after cluster is working, not before)

---

## What MemStorage Stores

Raft storage contains:
1. **Hard state**: Current term, voted_for, commit index
2. **Log entries**: Metadata commands (AssignPartition, SetPartitionLeader, UpdateISR)
3. **Snapshots**: Compressed state machine checkpoints (optional)

**NOT stored in Raft**:
- ❌ Actual message data (in WAL)
- ❌ Topic/partition metadata (in Chronik metadata store)
- ❌ Consumer offsets (in Chronik metadata store)

---

## What Happens with MemStorage (Current State)

### On Normal Operation ✅
```
Node 1: Leader (MemStorage)
    ↓
Topic created → Raft proposal → Committed
    ↓
All nodes apply to state machine
    ↓
Metadata replicated ✅
```

**Everything works perfectly!**

### On Node Restart 🤔
```
Node 1 restarts
    ↓
MemStorage is empty (lost)
    ↓
Raft log gone
    ↓
Metadata state machine empty
    ↓
BUT: Chronik metadata store STILL HAS THE DATA!
    ↓
Can bootstrap Raft from Chronik metadata ✅
```

**Cluster recovers, just needs re-initialization**

### On All Nodes Restart 🤔
```
All 3 nodes restart
    ↓
All MemStorage instances empty
    ↓
All Raft logs gone
    ↓
Metadata state machines empty
    ↓
BUT: Chronik metadata WAL STILL HAS THE DATA!
    ↓
Read from Chronik metadata store
    ↓
Re-propose to Raft
    ↓
Cluster re-initializes ✅
```

**Still recoverable!**

---

## The Real Question: Single Source of Truth

### Option A: Raft as Source of Truth (Needs Persistent Storage)

```
Raft Storage (RocksDB)
    ↓ (only source)
Metadata State Machine
    ↓
Cluster knows: topics, partitions, leaders, ISR
```

**Pros**:
- ✅ Single source of truth
- ✅ Strong consistency guarantees
- ✅ Fast recovery (just replay Raft log)

**Cons**:
- ❌ Need to implement persistent Raft storage
- ❌ Another storage layer to maintain
- ❌ Raft log can grow unbounded (need snapshots)

### Option B: Chronik Metadata as Source of Truth (MemStorage OK)

```
Chronik Metadata Store (WAL-based, already persistent!)
    ↓
Bootstrap Raft on startup
    ↓
Raft provides consensus (in-memory only)
    ↓
Committed entries also saved to Chronik metadata
```

**Pros**:
- ✅ No new storage layer needed
- ✅ Chronik metadata already durable
- ✅ Simpler architecture
- ✅ Single source of truth (Chronik metadata)

**Cons**:
- ⚠️ Slower recovery (need to re-propose from Chronik metadata)
- ⚠️ Raft log lost on restart (but can rebuild)

---

## Current Architecture (Already Hybrid!)

Looking at your code, you ALREADY have both:

### 1. Raft Metadata State Machine
**File**: `crates/chronik-server/src/raft_metadata.rs`

```rust
impl MetadataStateMachine {
    pub fn apply(&mut self, cmd: MetadataCommand) -> Result<()> {
        match cmd {
            MetadataCommand::AssignPartition { topic, partition, replicas } => {
                // Store in Raft state machine (in-memory)
                self.partition_assignments.insert((topic, partition), replicas);
            }
        }
    }
}
```

### 2. Chronik Metadata Store
**File**: `crates/chronik-common/src/metadata/`

```rust
pub trait MetadataStore {
    async fn create_topic(&self, topic: &str, config: TopicConfig) -> Result<TopicMetadata>;
    async fn get_topic(&self, topic: &str) -> Result<Option<TopicMetadata>>;
    // ... persisted to disk!
}
```

**You have BOTH systems running in parallel!**

---

## The Smart Approach: Use What You Have

### Recommended Strategy

**Phase 1: Get cluster working with MemStorage** (4 hours - from the plan)
- Use MemStorage for Raft
- Metadata also saved to Chronik metadata store
- Cluster works perfectly for testing

**Phase 2: Add bootstrap-from-Chronik logic** (2 hours - optional)
```rust
impl RaftCluster {
    pub async fn bootstrap_from_metadata(
        node_id: u64,
        peers: Vec<(u64, String)>,
        metadata_store: Arc<dyn MetadataStore>,
    ) -> Result<Self> {
        // Create Raft cluster
        let cluster = Self::bootstrap(node_id, peers).await?;

        // Load existing metadata from Chronik store
        let topics = metadata_store.list_topics().await?;

        for topic in topics {
            for partition in 0..topic.num_partitions {
                // Re-propose partition assignments
                cluster.propose(MetadataCommand::AssignPartition {
                    topic: topic.name.clone(),
                    partition,
                    replicas: topic.replica_assignments[partition].clone(),
                }).await?;

                // Re-propose partition leaders
                cluster.propose(MetadataCommand::SetPartitionLeader {
                    topic: topic.name.clone(),
                    partition,
                    leader: topic.partition_leaders[partition],
                }).await?;
            }
        }

        Ok(cluster)
    }
}
```

**Phase 3: Add persistent Raft storage** (1-2 hours - optional)
- Only if you want faster recovery
- Only if Chronik metadata bootstrap is too slow

---

## When You MUST Have Persistent Raft Storage

### Scenario 1: Raft is the ONLY metadata store
If you removed Chronik metadata store and used ONLY Raft for metadata:
- ✅ MUST persist Raft storage (it's your only source of truth)

### Scenario 2: Very frequent restarts
If nodes restart every few minutes:
- ✅ SHOULD persist Raft storage (avoid constant re-bootstrap)

### Scenario 3: Very large clusters
If you have 100+ topics with 1000+ partitions:
- ✅ SHOULD persist Raft storage (re-proposing 100K entries is slow)

---

## When You DON'T Need Persistent Raft Storage

### Scenario 1: Chronik metadata is source of truth (YOUR CASE!)
If Chronik metadata store already persists everything:
- ✅ MemStorage is fine
- ✅ Bootstrap from Chronik on restart

### Scenario 2: Infrequent restarts
If nodes run for days/weeks between restarts:
- ✅ MemStorage is fine
- ✅ 1-minute bootstrap delay is acceptable

### Scenario 3: Small clusters
If you have < 100 topics:
- ✅ MemStorage is fine
- ✅ Re-proposing 300 entries takes seconds

---

## Performance Impact

### MemStorage
```
Read:  < 1 microsecond (in-memory HashMap)
Write: < 1 microsecond (in-memory HashMap)
Recovery: 1-2 minutes (re-propose from Chronik metadata)
```

### RocksDB (Persistent)
```
Read:  10-100 microseconds (disk read + cache)
Write: 100-1000 microseconds (fsync)
Recovery: 10-30 seconds (replay Raft log from disk)
```

**Performance during normal operation**: MemStorage is FASTER ✅
**Performance during recovery**: RocksDB is FASTER ✅

---

## My Recommendation

### For Getting Cluster Working (Next 4 Hours)

**Use MemStorage** - Keep the current implementation!

**Why**:
1. ✅ Already implemented and working
2. ✅ Faster for normal operations
3. ✅ You have Chronik metadata as backup
4. ✅ Don't add complexity before cluster is working

### For Production (After Cluster Works)

**Add bootstrap-from-Chronik logic** (2 hours):
```rust
// On startup, if Raft state is empty
if raft_state_machine.is_empty() {
    info!("Raft state empty, bootstrapping from Chronik metadata...");
    bootstrap_from_metadata(metadata_store).await?;
}
```

**Then optionally add RocksDB persistence** (1-2 hours):
- Only if recovery time becomes an issue
- Only if you have 100+ topics
- Only if nodes restart frequently

---

## Implementation Priority

| Priority | Task | Effort | Benefit |
|----------|------|--------|---------|
| 🔴 **P0** | Wire message loop | 30 min | Cluster works! |
| 🔴 **P0** | Fix quorum calculation | 30 min | acks=-1 works! |
| 🔴 **P0** | Test 3-node cluster | 3 hours | Verify everything! |
| 🟡 **P1** | Bootstrap from Chronik | 2 hours | Faster recovery |
| 🟢 **P2** | Persistent Raft storage | 1-2 hours | Even faster recovery |

**Start with P0 tasks. Only do P1/P2 if you need them.**

---

## What Kafka Does

**Fun fact**: Kafka has the SAME dual-storage approach!

### Kafka's Architecture
```
Controller (Raft/KRaft)
    ↓ (metadata consensus)
Metadata log (persistent)
    ↓
Each broker has local metadata cache
    ↓
Broker restarts → reads from metadata log
```

**Kafka uses persistent Raft storage** because:
1. Thousands of topics/partitions (too much to re-propose)
2. Raft IS the source of truth (no separate metadata store)
3. Fast recovery critical (SLA requirements)

**You're different** because:
1. Small/medium cluster (< 1000 partitions)
2. Chronik metadata is source of truth (already persistent!)
3. 1-minute recovery is acceptable for testing

---

## Decision Tree

```
Is the cluster working?
├─ NO → Keep MemStorage, focus on getting it working
└─ YES → Continue below

Do nodes restart frequently?
├─ YES → Add persistent Raft storage
└─ NO → Continue below

Do you have 100+ topics?
├─ YES → Add persistent Raft storage
└─ NO → Continue below

Is 1-minute recovery acceptable?
├─ YES → Keep MemStorage + bootstrap from Chronik
└─ NO → Add persistent Raft storage
```

---

## Conclusion

**Your question**: "Shouldn't we implement persistent Raft storage?"

**My answer**:
- For testing cluster (next 4 hours): **NO, keep MemStorage** ✅
- For production (after cluster works): **MAYBE, add bootstrap-from-Chronik first** ⚠️
- For large-scale production: **YES, add RocksDB persistence** ✅

**Current priority**: Get cluster working with MemStorage (4 hours)
**Next priority**: Add Chronik metadata bootstrap (2 hours)
**Optional**: Add RocksDB persistence (1-2 hours)

---

## Action Plan

### Today (4 hours)
1. ✅ Keep MemStorage (don't change it!)
2. ✅ Wire message loop
3. ✅ Fix quorum calculation
4. ✅ Test cluster end-to-end

### This Week (2 hours - if needed)
5. Add bootstrap-from-Chronik logic
6. Test recovery after restart

### Later (1-2 hours - if needed)
7. Replace MemStorage with RocksDB
8. Implement snapshot support
9. Test recovery performance

**Start with steps 1-4. Only do 5-9 if you need them.**

---

## Summary

**MemStorage is fine for now!**

You have bigger priorities:
1. Get the message loop started (missing 1 line!)
2. Fix quorum calculation (hardcoded value!)
3. Test the cluster end-to-end

Persistent Raft storage is a **production optimization**, not a **blocker for getting cluster working**.

Focus on the 4-hour plan first. Add persistence later if you need it.
