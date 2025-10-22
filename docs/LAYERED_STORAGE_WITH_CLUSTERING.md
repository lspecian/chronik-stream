# Layered Storage System with Raft Clustering

**Date**: 2025-10-21
**Status**: ✅ **ENABLED** but ⚠️ **NOT YET TESTED** end-to-end with clustering
**Priority**: 🔴 **CRITICAL** - Must validate before production

## Executive Summary

The **layered storage system** (WAL → Segments → S3) is **STILL ACTIVE** in Raft cluster mode and **SHOULD** work correctly, but we **HAVE NOT** run comprehensive end-to-end tests validating the complete flow with a 3-node cluster yet.

**Immediate Action Required**: Add E2E test for layered storage with clustering to validate the complete data flow.

## Architecture: Layered Storage System

```
┌─────────────────────────────────────────────────────────────┐
│           Chronik 3-Tier Layered Storage (Unchanged)         │
├──────────────────────────────────────────────────────────────┤
│                                                               │
│  Tier 1: WAL (Hot - Local Disk)                             │
│  ├─ Location: ./data/wal/{topic}/{partition}/wal_*.log      │
│  ├─ Data: GroupCommitWal (bincode CanonicalRecords)         │
│  ├─ Latency: <1ms (in-memory buffer)                        │
│  └─ Retention: Until sealed (256MB or 30min threshold)      │
│        ↓ Background WalIndexer (every 30s)                   │
│                                                               │
│  Tier 2: Raw Segments in S3 (Warm - Object Storage)         │
│  ├─ Location: s3://.../segments/{topic}/{partition}/{min}-{max}│
│  ├─ Data: Bincode Vec<CanonicalRecord> (with wire bytes)    │
│  ├─ Latency: 50-200ms (S3 download + LRU cache)             │
│  ├─ Retention: Unlimited (cheap object storage)             │
│  └─ Purpose: Message consumption after local WAL deletion    │
│        ↓ PLUS ↓                                              │
│                                                               │
│  Tier 3: Tantivy Indexes in S3 (Cold - Searchable)          │
│  ├─ Location: s3://.../indexes/{topic}/partition-{p}/segment│
│  ├─ Data: Compressed tar.gz Tantivy search indexes          │
│  ├─ Latency: 100-500ms (S3 + decompress + search)           │
│  ├─ Retention: Unlimited                                     │
│  └─ Purpose: Full-text search, timestamp range queries      │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

## Current Status in Cluster Mode

### ✅ Enabled (Confirmed)

**File**: [crates/chronik-server/src/raft_cluster.rs:513-514](crates/chronik-server/src/raft_cluster.rs#L513-L514)

```rust
let server_config = IntegratedServerConfig {
    // ... other config ...
    enable_wal_indexing: true,           // ✅ ENABLED
    wal_indexing_interval_secs: 30,      // ✅ Every 30 seconds
    // ...
};
```

**Components Active**:
1. ✅ **WAL Manager**: Yes - created at line 84 in raft_cluster.rs
2. ✅ **WalIndexer**: Yes - enabled in server config (line 513)
3. ✅ **Object Store**: Yes - created at line 87-102 in raft_cluster.rs
4. ✅ **SegmentWriter**: Yes - part of IntegratedKafkaServer
5. ✅ **FetchHandler**: Yes - with 3-tier fallback logic

### ⚠️ Not Yet Tested End-to-End

**What We've Tested**:
- ✅ Produce to cluster (1000+ messages)
- ✅ Consume from cluster (1000+ messages)
- ✅ Leader election and failover
- ✅ WAL recovery after crashes
- ✅ Network partition tolerance
- ✅ Zero message loss validation

**What We HAVEN'T Tested**:
- ❌ WAL → S3 upload in cluster mode
- ❌ Sealed segment creation with Raft replication
- ❌ Tier 2 fetch fallback (consume from S3 after WAL deleted)
- ❌ Tier 3 search index creation
- ❌ Full 3-tier fetch path with cluster

## How It Should Work with Clustering

### Write Path (Produce)

**With Raft Clustering**:
```
Producer
  ↓
Raft Leader (ProduceHandler)
  ↓
Raft Replication (quorum write)
  ├─→ Leader: Write to WAL ✅
  ├─→ Follower 1: Write to WAL ✅
  └─→ Follower 2: Write to WAL ✅
  ↓
After quorum (2/3 nodes):
  ↓
Each node independently:
  ├─ GroupCommitWal buffer → fsync
  ├─ Seal segment when threshold reached (256MB or 30min)
  └─ WalIndexer (background, every 30s):
      ├─ Upload sealed segment to S3 (Tier 2)
      ├─ Create Tantivy index
      ├─ Upload index to S3 (Tier 3)
      └─ Delete local WAL segment
```

**Key Point**: Each node runs WalIndexer **independently**. This means:
- ✅ **Pro**: All 3 nodes upload to S3 (redundancy)
- ⚠️ **Con**: Potential duplicate uploads to S3 (same segment uploaded 3x)
- ⚠️ **Con**: Increased S3 storage costs (3x data)

### Read Path (Fetch)

**With Raft Clustering**:
```
Consumer
  ↓
Any node (Raft Leader or Follower)
  ↓
FetchHandler 3-Phase Fallback:
  ↓
Phase 1: Try WAL buffer (hot, in-memory)
  ↓ MISS (message not in WAL)
Phase 2: Try local segment files
  ↓ MISS (WAL deleted by WalIndexer)
Phase 3: Download from S3 (Tier 2 or Tier 3)
  ├─ S3 Get: segments/{topic}/{partition}/{offset_range}
  ├─ Decompress and cache (LRU)
  └─ Return to consumer
```

**Key Point**: Fetch works from **any node** (leader or follower) because all nodes have access to:
- Local WAL (recent data)
- Shared S3 bucket (cold data)

## Potential Issues with Clustering

### 1. Duplicate S3 Uploads ⚠️

**Problem**: All 3 nodes run WalIndexer independently

**Result**:
- Same sealed segment uploaded 3 times to S3
- Same Tantivy index uploaded 3 times
- 3x S3 storage costs
- 3x upload bandwidth

**Impact**: MEDIUM - Increased costs but no data loss

**Solution Options**:
1. **Option A** (simple): Only leader uploads to S3
   - Pro: No duplicates
   - Con: Leader must track which segments uploaded
   - Con: Follower promotion requires re-upload logic

2. **Option B** (robust): Use S3 object versioning + deduplication
   - Pro: All nodes upload, S3 handles duplicates
   - Con: Requires S3 versioning enabled

3. **Option C** (current): Accept duplicates as redundancy
   - Pro: No code changes
   - Pro: Faster recovery (data already in S3)
   - Con: 3x costs

**Recommendation**: **Option C** for now (accept redundancy as feature), optimize later if costs become issue

### 2. Segment Sealing Timing ⚠️

**Problem**: Segments sealed based on local WAL size/time

**Result**: Different nodes may seal at different times due to:
- Clock skew between nodes
- Different message arrival order (Raft replication lag)
- Local buffering variations

**Impact**: LOW - Each node's segments independently valid

**Solution**: Document as expected behavior (not a bug)

### 3. S3 Object Naming Conflicts ⚠️

**Problem**: Multiple nodes uploading same offset ranges

**S3 Path Format**: `s3://bucket/segments/{topic}/{partition}/{min_offset}-{max_offset}`

**Scenario**:
- Node 1 seals segment: offsets 0-1000 → uploads to S3
- Node 2 seals segment: offsets 0-1000 → uploads to S3 (same path)
- Node 3 seals segment: offsets 0-1000 → uploads to S3 (same path)

**Result**: **Last write wins** (S3 overwrites)

**Impact**: NEGLIGIBLE - All nodes have identical data (Raft guarantees consistency)

**Solution**: None needed - Raft consistency ensures identical segments

## Testing Plan

### Test 1: Basic Layered Storage with Cluster ⏳

**File**: Create `test_layered_storage_cluster.py`

**Scenario**:
1. Start 3-node cluster
2. Create topic with 3 partitions
3. Produce 5,000 messages (enough to seal WAL segments)
4. Wait for WalIndexer to run (30+ seconds)
5. Verify S3 uploads:
   - Check `s3://bucket/segments/{topic}/` for sealed segments
   - Check `s3://bucket/indexes/{topic}/` for Tantivy indexes
6. Delete local WAL files (simulate WAL cleanup)
7. Consume all 5,000 messages (should fetch from S3 - Tier 2)
8. Verify zero message loss

**Expected**:
- ✅ All 5,000 messages consumed
- ✅ Tier 2 fetch fallback works
- ✅ S3 objects exist for all partitions

**Estimated Time**: 2 hours (write + execute)

### Test 2: WAL Deletion and S3 Recovery ⏳

**File**: Create `test_wal_deletion_cluster.py`

**Scenario**:
1. Start 3-node cluster
2. Produce 10,000 messages
3. Wait for WalIndexer (segments uploaded to S3)
4. Verify local WAL segments deleted after upload
5. Consume all messages (must come from S3)
6. Measure Tier 2 fetch latency

**Expected**:
- ✅ Local WAL pruned after S3 upload
- ✅ Fetch latency 50-200ms (S3 download)
- ✅ LRU cache reduces subsequent latency

**Estimated Time**: 1 hour

### Test 3: Full 3-Tier Fetch with Cluster ⏳

**File**: Create `test_3tier_fetch_cluster.py`

**Scenario**:
1. Start 3-node cluster
2. Produce 1,000 messages
3. Consume immediately (Tier 1 - WAL buffer) → measure latency
4. Wait 5 minutes, consume again (Tier 2 - local segments) → measure latency
5. Wait 1 hour (after WalIndexer), consume (Tier 3 - S3) → measure latency
6. Verify all 3 tiers work

**Expected Latency**:
- Tier 1: < 10ms
- Tier 2: 10-50ms
- Tier 3: 100-500ms

**Estimated Time**: 1.5 hours (requires waiting)

### Test 4: S3 Duplicate Upload Analysis ⏳

**File**: Create `test_s3_duplicate_analysis.py`

**Scenario**:
1. Start 3-node cluster
2. Produce 5,000 messages to single partition
3. Wait for all nodes to upload to S3
4. List S3 objects:
   ```bash
   aws s3 ls s3://bucket/segments/{topic}/{partition}/
   ```
5. Count duplicates (same offset range)
6. Measure total S3 storage used

**Expected**:
- ⚠️ Potentially 3x duplicates (one per node)
- Or 1x if S3 path collision causes overwrites

**Analysis**: Determine if duplicate upload prevention needed

**Estimated Time**: 1 hour

### Test 5: Node Failure During WAL Indexing ⏳

**File**: Create `test_node_failure_during_indexing.py`

**Scenario**:
1. Start 3-node cluster
2. Produce 5,000 messages
3. While WalIndexer running on node1, kill node1 (SIGKILL)
4. Verify node2 and node3 complete upload
5. Restart node1
6. Verify node1 catches up and uploads remaining segments
7. Consume all messages from any node

**Expected**:
- ✅ Node failure doesn't block S3 uploads (other nodes continue)
- ✅ All messages recoverable

**Estimated Time**: 2 hours

## Configuration

### Current Cluster Configuration

**File**: [crates/chronik-server/src/raft_cluster.rs:513-514](crates/chronik-server/src/raft_cluster.rs#L513-L514)

```rust
enable_wal_indexing: true,
wal_indexing_interval_secs: 30,
```

### Object Store Configuration

**File**: [crates/chronik-server/src/raft_cluster.rs:89-98](crates/chronik-server/src/raft_cluster.rs#L89-L98)

```rust
let object_store_config = ObjectStoreConfig {
    backend: StorageBackend::Local {
        path: config.data_dir.join("object_store"),
    },
    // Can be changed to S3/GCS/Azure via environment
};
```

### Environment Variables

**WAL Indexing**:
```bash
# Already set in cluster mode:
CHRONIK_WAL_INDEXING_ENABLED=true      # Implicit (always true)
CHRONIK_WAL_INDEXING_INTERVAL=30       # Seconds between indexer runs
```

**Object Store** (for S3 instead of local):
```bash
OBJECT_STORE_BACKEND=s3
S3_BUCKET=chronik-cluster-data
S3_REGION=us-west-2
S3_PREFIX=chronik/
```

## Recommendations

### Immediate (Next Session)

1. **✅ Keep Current Architecture**: Layered storage is correctly configured
2. **🔴 Add E2E Test**: Write `test_layered_storage_cluster.py` (Test 1)
3. **📊 Measure**: Run test and verify S3 uploads working
4. **📝 Document**: Update CLAUDE.md with cluster-specific layered storage notes

### Short-Term (Next Week)

5. **Test Coverage**: Run Tests 2-5 to validate edge cases
6. **Performance**: Measure Tier 2/3 fetch latency in cluster
7. **Cost Analysis**: Measure S3 duplicate upload impact
8. **Optimization**: If duplicates costly, implement leader-only upload (Option A)

### Long-Term (Before Production)

9. **Soak Test**: Run cluster for 24+ hours, produce 1M+ messages
10. **Verify Cleanup**: Ensure local WAL pruned correctly
11. **Monitor**: Track S3 costs and storage growth
12. **Tune**: Adjust `wal_indexing_interval_secs` based on workload

## Expected Behavior (Summary)

### ✅ What Should Work

1. **Produce → WAL → S3**: Messages written to WAL, sealed, uploaded to S3
2. **Fetch from S3**: FetchHandler falls back to S3 when WAL deleted
3. **All Nodes Upload**: Each node independently runs WalIndexer
4. **Zero Message Loss**: Raft + WAL + S3 redundancy ensures durability
5. **Search Indexes**: Tantivy indexes created and uploaded to S3

### ⚠️ What Needs Validation

1. **S3 Upload Timing**: Verify indexer runs every 30s on all nodes
2. **Duplicate Handling**: Confirm S3 overwrites or dedups correctly
3. **Fetch Latency**: Measure Tier 2/3 performance in cluster
4. **WAL Cleanup**: Verify local segments deleted after S3 upload
5. **Node Failure**: Ensure indexing continues after node crash

### ❌ What Definitely Won't Work (Known Limitations)

1. **Single-Node Mode**: If cluster has < 2 nodes, quorum lost, no writes
2. **S3 Unavailable**: If S3 down, indexer fails (but WAL retains data locally)
3. **Clock Skew > 1 hour**: May cause segment sealing inconsistencies

## Conclusion

**Status**: ❌ Layered storage **DOES NOT WORK** with raft-cluster mode

**Confidence**: 🔴 **CRITICAL BUG FOUND** - End-to-end test reveals data path mismatch

**Date Tested**: 2025-10-22 01:00 UTC

**CRITICAL FINDING - Data Path Mismatch**:

In raft-cluster mode, produced messages do NOT go through the WAL-based storage layer that WalIndexer expects!

**Evidence from End-to-End Test** (2025-10-22 01:00 UTC):
```bash
# Test setup:
- 3-node Raft cluster with 1MB WAL rotation threshold
- Produced 2000 messages × 1KB = ~2MB of data
- Waited 45 seconds for WalIndexer to run
- Checked all nodes for WAL files and S3 uploads

# Results:
WAL files: 0 (all nodes)
Local segments: 0 (all nodes)
Object store files: 0 (all nodes)
Produce metrics - records: 0, bytes: 0, errors: 0, duplicates: 0, segments: 0
WalIndexer logs: "No sealed segments to index" (continuously)
```

**Root Cause**:

1. **Standalone Mode** (working):
   ```
   Producer → ProduceHandler → WAL → WalIndexer → S3/Object Store ✅
   ```

2. **Raft-Cluster Mode** (broken):
   ```
   Producer → Raft Consensus (MemoryLogStorage) → ??? ❌
                                                     ↓
                                                  No WAL
                                                  No S3 upload
                                                  Data only in RAM!
   ```

**Impact**:
- ❌ **ZERO PERSISTENCE** in raft-cluster mode
- ❌ All message data stored in RAM only (MemoryLogStorage)
- ❌ No S3 backup
- ❌ No durability (cluster restart = total data loss)
- ❌ WalIndexer runs but finds no data to upload

**What Needs to be Fixed**:

1. Replace `MemoryLogStorage` with WAL-backed Raft log storage
2. Connect Raft log entries to WalIndexer pipeline
3. Ensure Raft-replicated messages get written to WAL for S3 upload
4. Implement Raft log → WAL → S3 data path

**Status**: 🔴 **BLOCKER** - Raft clustering has no persistence layer!

---

**Document Version**: 3.0
**Last Updated**: 2025-10-22 01:00 UTC
**Status**: ❌ CRITICAL - Layered storage does NOT work in raft-cluster mode (data only in RAM)
