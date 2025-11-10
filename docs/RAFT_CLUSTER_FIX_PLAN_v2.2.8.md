# Raft Cluster Fix Plan (v2.2.8)

**Date**: 2025-11-10  
**Status**: COMPREHENSIVE PRODUCTION-READY PLAN  
**Target**: Fix high-concurrency cluster failures and unstable log bugs

---

## Executive Summary

**Current State**:
- ✅ **Low concurrency works**: 1,000 messages → 328 msg/s produce, 94 msg/s consume
- ❌ **High concurrency fails**: 128+ producers → 0 msg/s, cluster becomes unresponsive
- ⚠️ **Band-aid fix in place**: Return empty Vec instead of `Compacted` error (prevents panic but breaks Raft assumptions)

**Root Causes**:
1. **Unstable log exhaustion**: Raft expects historical entries for conflict resolution, but our fix returns empty Vec
2. **High concurrency bottleneck**: Unknown (metadata locking? WAL backpressure? Connection limits?)

**Goal**: Production-ready Raft cluster that handles high concurrency (128+ producers) with correct Raft semantics.

---

## Part 1: Root Cause Analysis

### Investigation Tasks

#### 1.1 High Concurrency Bottleneck Analysis

**Hypothesis Tree**:
```
High concurrency → 0 msg/s
├─ Metadata Store Contention
│  ├─ RwLock starvation (readers block writers)
│  ├─ Raft propose serialization (single-threaded propose)
│  └─ State machine lock held too long
├─ WAL Backpressure
│  ├─ GroupCommitWal queue full (max_queue_depth exceeded)
│  ├─ fsync bottleneck (disk I/O saturation)
│  └─ Segment rotation slowdown
├─ Connection Limits
│  ├─ TCP connection exhaustion (ulimit, ephemeral ports)
│  ├─ Tokio task queue saturation
│  └─ Memory exhaustion (OOM killer)
├─ Raft Message Queue Overload
│  ├─ Unbounded message channel fills up
│  ├─ gRPC transport failures (retry storm)
│  └─ Network partition (nodes can't communicate)
└─ Empty Vec Fix Side Effects
   ├─ Raft state machine inconsistency
   ├─ Followers can't catch up (missing entries)
   └─ Leader election failures
```

**Action**: Add comprehensive telemetry and run controlled experiments.

**Steps**:
1. **Instrument critical paths**:
   ```rust
   // crates/chronik-server/src/produce_handler.rs
   tracing::info!("PRODUCE_START: concurrency={}, topic={}, partition={}", 
       active_producers, topic, partition);
   
   // crates/chronik-server/src/raft_metadata_store.rs
   tracing::info!("METADATA_PROPOSE_START: cmd={:?}", cmd);
   let start = Instant::now();
   self.raft_cluster.propose(cmd).await?;
   tracing::info!("METADATA_PROPOSE_DONE: elapsed={:?}", start.elapsed());
   
   // crates/chronik-wal/src/group_commit.rs
   tracing::info!("WAL_ENQUEUE: queue_depth={}, total_queued_bytes={}", 
       pending.len(), total_bytes);
   ```

2. **Run progressive load test**:
   ```bash
   # Gradual increase: 1 → 8 → 16 → 32 → 64 → 128 producers
   for concurrency in 1 8 16 32 64 128; do
       echo "Testing concurrency=$concurrency"
       ./target/release/chronik-bench \
           --bootstrap-servers localhost:9092 \
           --topic test-conc-$concurrency \
           --mode produce \
           --concurrency $concurrency \
           --message-size 256 \
           --duration 30s \
           --acks 1
       
       # Check where it breaks
       tail -100 tests/cluster/logs/node1.log | grep -E "(queue_depth|PROPOSE|ERROR)"
   done
   ```

3. **Isolate failure mode**:
   - If breaks at 16: Likely metadata contention (low threshold)
   - If breaks at 64: Likely WAL backpressure (medium threshold)
   - If breaks at 128: Likely connection/memory limits (high threshold)

**Expected Outcome**: Identify the PRIMARY bottleneck (one of the 5 hypothesis branches).

---

#### 1.2 Empty Vec Fix Impact Analysis

**Current Implementation** (`crates/chronik-wal/src/raft_storage_impl.rs:697-703`):
```rust
if low < first_index {
    tracing::warn!(
        "entries({}, {}): requested entries older than first_index={}, returning EMPTY",
        low, high, first_index
    );
    return Ok(Vec::new());  // ⚠️ Band-aid fix
}
```

**Problem**: Raft interprets empty Vec as "no entries exist" NOT "entries are compacted".

**Impact**:
- Follower catchup: Leader sends AppendEntries with prev_log_index < first_index → empty Vec → follower thinks "no conflict, append" → WRONG
- Leader transfer: New leader tries to read old entries → empty Vec → can't validate log consistency → election fails
- Snapshot installation: Should trigger snapshot transfer, but empty Vec prevents it

**Action**: Test Raft correctness with current fix.

**Steps**:
1. **Network partition test**:
   ```bash
   # Start cluster
   ./tests/cluster/start.sh
   
   # Disconnect node 3
   sudo iptables -A INPUT -s localhost -p tcp --dport 9094 -j DROP
   
   # Produce 10,000 messages (forces log divergence)
   python3 -c "
   from kafka import KafkaProducer
   p = KafkaProducer(bootstrap_servers='localhost:9092')
   for i in range(10000):
       p.send('divergence-test', f'msg-{i}'.encode())
   p.flush()
   "
   
   # Reconnect node 3 (will need to catch up)
   sudo iptables -D INPUT -s localhost -p tcp --dport 9094 -j DROP
   
   # Check if node 3 catches up correctly
   tail -f tests/cluster/logs/node3.log | grep -E "(entries|catch|conflict)"
   ```

2. **Leader transfer test**:
   ```bash
   # Force leadership transfer from node 1 → node 2
   # (Requires implementing transfer_leadership RPC - may skip)
   ```

3. **Crash-recover test**:
   ```bash
   # Kill node 2 mid-produce
   pkill -f "chronik-server.*node2"
   
   # Produce 5,000 more messages
   # Restart node 2
   ./target/release/chronik-server start --config tests/cluster/node2.toml
   
   # Verify node 2 catches up
   ```

**Expected Outcome**: Determine if empty Vec fix causes catchup failures or data loss.

---

## Part 2: Fix Options Evaluation

### Option A: Implement Async WAL Read Fallback ⭐ RECOMMENDED

**Approach**: When `Storage::entries(low, high)` misses in-memory log, asynchronously read from WAL on disk.

**Pros**:
- ✅ **Semantically correct**: Returns actual entries (not empty Vec)
- ✅ **No Raft library changes**: Works within existing `Storage` trait
- ✅ **Handles all cases**: Conflict resolution, catchup, leader transfer
- ✅ **Production-ready**: Similar to etcd's approach (hybrid in-memory + disk storage)

**Cons**:
- ⚠️ **Requires trait change**: `Storage` trait is synchronous, but WAL read is async
- ⚠️ **Performance overhead**: Disk read adds 1-5ms latency (vs μs for in-memory)

**Implementation**:

1. **Make Storage::entries() blocking-async**:
   ```rust
   // crates/chronik-wal/src/raft_storage_impl.rs
   fn entries(&self, low: u64, high: u64, ...) -> raft::Result<Vec<Entry>> {
       let log = self.entries.read().unwrap();
       let first_index = *self.first_index.read().unwrap();
       
       // Phase 1: Try in-memory (hot path)
       if low >= first_index {
           return Ok(log[(low - first_index) as usize..(high - first_index) as usize].to_vec());
       }
       
       // Phase 2: Fallback to async WAL read (cold path)
       tracing::info!("entries({}, {}): Miss in-memory, reading from WAL", low, high);
       
       // CRITICAL: Use tokio::task::block_in_place to run async in sync context
       let entries = tokio::task::block_in_place(|| {
           tokio::runtime::Handle::current().block_on(async {
               self.read_entries_from_wal(low, high).await
           })
       })?;
       
       Ok(entries)
   }
   
   async fn read_entries_from_wal(&self, low: u64, high: u64) -> Result<Vec<Entry>> {
       // Scan WAL segments for entries [low, high)
       // Similar to recover() logic but for specific range
       // ...
   }
   ```

2. **WAL segment scanning logic**:
   ```rust
   async fn read_entries_from_wal(&self, low: u64, high: u64) -> Result<Vec<Entry>> {
       let wal_dir = self.data_dir.join("wal/__meta/__raft_metadata/0");
       let mut entries = Vec::new();
       
       for wal_file in fs::read_dir(&wal_dir)? {
           let file_path = wal_file?.path();
           let mut file = fs::File::open(&file_path)?;
           let mut contents = Vec::new();
           file.read_to_end(&mut contents)?;
           
           // Parse WalRecords
           let mut offset = 0;
           while offset < contents.len() {
               let record = WalRecord::from_bytes(&contents[offset..])?;
               if let Some(data) = record.get_canonical_data() {
                   let mut entry = Entry::default();
                   if entry.merge_from_bytes(data).is_ok() {
                       if entry.index >= low && entry.index < high {
                           entries.push(entry);
                       }
                   }
               }
               offset += record.size();
           }
       }
       
       entries.sort_by_key(|e| e.index);
       Ok(entries)
   }
   ```

**Complexity**: Medium (200-300 LOC)  
**Timeline**: 4-6 hours (implementation + testing)  
**Risk**: Low (fallback path, hot path unchanged)

---

### Option B: Implement Proper Raft Snapshots

**Approach**: Use raft-rs snapshot mechanism to communicate "entries are gone, here's the snapshot".

**Pros**:
- ✅ **Raft-native solution**: Uses intended snapshot mechanism
- ✅ **Space-efficient**: Old entries deleted from disk
- ✅ **Handles catchup**: Followers install snapshot instead of replaying entries

**Cons**:
- ❌ **Already partially implemented**: We DO create snapshots (v2.2.8), but Raft still requests old entries
- ❌ **Doesn't solve root cause**: Raft will STILL call `entries(low, high)` for conflict resolution BEFORE snapshot
- ❌ **Complex**: Snapshot transfer, installation, truncation logic

**Why This Won't Work**:
- Raft's conflict resolution (`maybe_append()`) happens BEFORE snapshot installation
- It checks `prev_log_index` and `prev_log_term` against local log
- If local log doesn't have those entries → calls `entries()` → empty Vec → wrong

**Verdict**: ❌ **Not viable** as standalone solution (already implemented, still have bug).

---

### Option C: Keep ALL Entries in Memory (Unbounded Growth)

**Approach**: Never trim unstable log, keep all entries forever.

**Pros**:
- ✅ **Simple**: Remove trimming logic
- ✅ **Fast**: All entries in-memory

**Cons**:
- ❌ **Memory exhaustion**: 1M entries × 1KB/entry = 1GB RAM
- ❌ **Not production-ready**: Cluster will OOM after days/weeks
- ❌ **Defeats WAL purpose**: Why persist to disk if keeping in memory?

**Verdict**: ❌ **Not viable** for production.

---

### Option D: Switch to Different Raft Library

**Approach**: Replace tikv/raft-rs with openraft or raftify.

**Pros**:
- ✅ **Potentially better async integration**: openraft is fully async
- ✅ **More active maintenance**: openraft has recent commits

**Cons**:
- ❌ **Massive rewrite**: ~5,000 LOC affected (RaftCluster, RaftWalStorage, message loop)
- ❌ **Re-test everything**: All Raft features (snapshots, membership changes, etc.)
- ❌ **Unproven**: We already invested in tikv/raft-rs, know its quirks

**Verdict**: ❌ **Not viable** for v2.2.8 (too risky, too much work).

---

### Option E: Increase Unstable Log Retention Window

**Approach**: Keep 100,000 entries instead of 10,000.

**Pros**:
- ✅ **Trivial change**: One-line config change
- ✅ **Buys time**: Reduces frequency of misses

**Cons**:
- ⚠️ **Doesn't solve root cause**: Still fails after 100K entries
- ⚠️ **Higher memory usage**: 100K × 1KB = 100MB per node

**Verdict**: ⚠️ **Temporary workaround** only, pair with Option A.

---

## Part 3: Recommended Solution

### Hybrid Approach: Option A + Option E

**Phase 1: Immediate (Emergency Fix)**
- Increase unstable log retention to 50,000 entries
- Document as temporary workaround
- **Timeline**: 30 minutes
- **Risk**: Very low

**Phase 2: Production Fix (v2.2.8 Final)**
- Implement async WAL read fallback (Option A)
- Keep 10,000 entries in-memory (hot path)
- Fallback to WAL read for older entries (cold path)
- **Timeline**: 6-8 hours
- **Risk**: Low (well-scoped change)

**Phase 3: Optimization (v2.2.9)**
- Add LRU cache for WAL-read entries (avoid repeated disk reads)
- Benchmark and tune cache size
- **Timeline**: 4 hours
- **Risk**: Very low

---

## Part 4: Implementation Plan (Phase 2)

### Step 1: Implement `read_entries_from_wal()` Helper

**File**: `crates/chronik-wal/src/raft_storage_impl.rs`

**Code**:
```rust
impl RaftWalStorage {
    /// Read Raft entries from WAL segments (cold path for old entries)
    async fn read_entries_from_wal(&self, low: u64, high: u64) -> Result<Vec<Entry>> {
        use std::fs;
        use std::io::Read;
        
        tracing::info!("Reading entries [{}, {}) from WAL (cold path)", low, high);
        
        let data_dir = self.data_dir.read().unwrap().clone();
        let wal_dir = data_dir.join("wal/__meta/__raft_metadata/0");
        
        if !wal_dir.exists() {
            return Ok(Vec::new());
        }
        
        // Collect all WAL files
        let mut wal_files = Vec::new();
        for entry in fs::read_dir(&wal_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("log") {
                wal_files.push(path);
            }
        }
        wal_files.sort();
        
        let mut entries = Vec::new();
        
        for wal_file in &wal_files {
            let mut file = fs::File::open(wal_file)?;
            let mut contents = Vec::new();
            file.read_to_end(&mut contents)?;
            
            // Parse WalRecords
            let mut offset = 0;
            while offset < contents.len() {
                match WalRecord::from_bytes(&contents[offset..]) {
                    Ok(record) => {
                        let length = match &record {
                            WalRecord::V1 { length, .. } => *length,
                            WalRecord::V2 { length, .. } => *length,
                        };
                        let record_size = (length as usize) + 12;
                        offset += record_size;
                        
                        if let Some(data) = record.get_canonical_data() {
                            if !data.is_empty() {
                                let mut entry = Entry::default();
                                if entry.merge_from_bytes(data).is_ok() && entry.index > 0 {
                                    // Only include entries in requested range
                                    if entry.index >= low && entry.index < high {
                                        entries.push(entry);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        
        // Deduplicate and sort
        entries.sort_by_key(|e| e.index);
        entries.dedup_by_key(|e| e.index);
        
        tracing::info!("Read {} entries from WAL", entries.len());
        Ok(entries)
    }
}
```

**Testing**:
```bash
# Unit test
cargo test --package chronik-wal read_entries_from_wal -- --nocapture
```

---

### Step 2: Modify `Storage::entries()` to Use Fallback

**File**: `crates/chronik-wal/src/raft_storage_impl.rs`

**Changes**:
```rust
fn entries(&self, low: u64, high: u64, max_size: impl Into<Option<u64>>, _context: GetEntriesContext) -> raft::Result<Vec<Entry>> {
    let log = self.entries.read().unwrap();
    let first_index = *self.first_index.read().unwrap();
    
    // Handle empty range
    if low >= high {
        return Ok(Vec::new());
    }
    
    // HOT PATH: Entries in memory
    if low >= first_index {
        let lo = (low - first_index) as usize;
        let hi = (high - first_index) as usize;
        
        if hi > log.len() {
            return Err(raft::Error::Store(StorageError::Unavailable));
        }
        
        let mut entries = log[lo..hi].to_vec();
        
        // Apply size limit
        if let Some(max_size) = max_size.into() {
            let mut total_size = 0u64;
            let mut limit = entries.len();
            
            for (i, entry) in entries.iter().enumerate() {
                let entry_size = 24 + entry.data.len() as u64;
                total_size += entry_size;
                if total_size > max_size {
                    limit = i;
                    break;
                }
            }
            entries.truncate(limit);
        }
        
        return Ok(entries);
    }
    
    // COLD PATH: Read from WAL (async fallback)
    tracing::warn!(
        "entries({}, {}): Miss in-memory (first_index={}), reading from WAL",
        low, high, first_index
    );
    
    // Use block_in_place to run async in sync context
    let entries = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            self.read_entries_from_wal(low, high).await
        })
    })
    .map_err(|e| {
        tracing::error!("Failed to read entries from WAL: {}", e);
        raft::Error::Store(StorageError::Unavailable)
    })?;
    
    // Apply size limit
    let max_size = max_size.into();
    if let Some(max_size) = max_size {
        let mut limited = Vec::new();
        let mut total_size = 0u64;
        
        for entry in entries {
            let entry_size = 24 + entry.data.len() as u64;
            if total_size + entry_size > max_size {
                break;
            }
            total_size += entry_size;
            limited.push(entry);
        }
        
        Ok(limited)
    } else {
        Ok(entries)
    }
}
```

---

### Step 3: Testing Strategy

#### 3.1 Unit Tests

**File**: `crates/chronik-wal/tests/raft_storage_fallback_test.rs`

```rust
#[tokio::test]
async fn test_entries_in_memory() {
    // Entries [1, 10] in memory, first_index=1
    // Request [1, 10] → should return from memory
}

#[tokio::test]
async fn test_entries_wal_fallback() {
    // Entries [1, 100] persisted to WAL
    // Trim in-memory to [90, 100]
    // Request [1, 50] → should read from WAL
}

#[tokio::test]
async fn test_entries_partial_overlap() {
    // Entries [1, 100] persisted
    // In-memory [90, 100]
    // Request [80, 95] → should combine WAL [80, 90) + memory [90, 95)
}
```

#### 3.2 Integration Tests

**Scenario 1: Network Partition Catchup**
```bash
# Start cluster
./tests/cluster/start.sh

# Partition node 3
sudo iptables -A INPUT -s localhost -p tcp --dport 9094 -j DROP

# Produce 50,000 messages (exceeds 10K in-memory window)
python3 scripts/produce_bulk.py --topic partition-test --count 50000

# Rejoin node 3
sudo iptables -D INPUT -s localhost -p tcp --dport 9094 -j DROP

# Wait for catchup (should use WAL fallback)
python3 scripts/wait_for_catchup.py --node 3 --expected-offset 50000

# Verify no data loss
python3 scripts/verify_consumption.py --topic partition-test --expected-count 50000
```

**Scenario 2: High Concurrency Stress**
```bash
# Run benchmark with 128 producers
./target/release/chronik-bench \
    --bootstrap-servers localhost:9092,localhost:9093,localhost:9094 \
    --topic stress-test \
    --mode produce \
    --concurrency 128 \
    --message-size 256 \
    --duration 60s \
    --acks 1

# Expected: > 10,000 msg/s sustained
# Monitor: WAL fallback calls (should be rare)
grep "reading from WAL" tests/cluster/logs/*.log
```

**Scenario 3: Crash Recovery**
```bash
# Kill node 2 mid-produce
pkill -f "chronik-server.*node2"

# Produce 30,000 more messages
python3 scripts/produce_bulk.py --topic crash-test --count 30000

# Restart node 2 (will need to catch up from WAL)
./target/release/chronik-server start --config tests/cluster/node2.toml

# Verify catchup completes
python3 scripts/wait_for_catchup.py --node 2 --expected-offset 30000
```

---

## Part 5: Performance Expectations

### Latency Impact

**Hot Path (entries in-memory)**:
- Current: ~1-5μs (Vec slice)
- After change: ~1-5μs (unchanged)

**Cold Path (entries in WAL)**:
- Current: Panic or empty Vec (incorrect)
- After change: 1-5ms (disk read)

**Frequency**:
- Normal operation: 99.9% hot path (recent entries)
- Catchup scenarios: 50% cold path (reading old entries)
- Impact: Acceptable (correctness > speed for rare cold path)

### Throughput Impact

**Steady-state (no catchup)**:
- Before: 9,287 msg/s (v2.2.8 with snapshot storm fix)
- After: 9,287 msg/s (unchanged, hot path only)

**During catchup**:
- Before: Panic or incorrect state
- After: Slower catchup (1-5ms per batch) but CORRECT

---

## Part 6: Rollout Plan

### Phase 1: Emergency Fix (v2.2.8-rc1)
**Timeline**: 30 minutes

```bash
# Increase unstable log retention
# File: crates/chronik-wal/src/raft_storage_impl.rs:361
const MAX_UNSTABLE_ENTRIES: usize = 50_000;  // Was 10_000

# Build and deploy
cargo build --release
./tests/cluster/start.sh

# Test high concurrency
./target/release/chronik-bench --concurrency 128 --duration 30s
```

**Expected Result**: Cluster survives longer under load (buys time).

---

### Phase 2: Production Fix (v2.2.8-rc2)
**Timeline**: 8 hours (implementation + testing)

1. Implement `read_entries_from_wal()` helper (2 hours)
2. Modify `Storage::entries()` with fallback (1 hour)
3. Write unit tests (2 hours)
4. Run integration tests (2 hours)
5. Performance validation (1 hour)

**Acceptance Criteria**:
- ✅ All unit tests pass
- ✅ Network partition catchup works correctly
- ✅ High concurrency (128 producers) sustained for 5 minutes
- ✅ No panics, no data loss
- ✅ Performance: > 8,000 msg/s (acceptable vs 9,287 baseline)

---

### Phase 3: GA Release (v2.2.8)
**Timeline**: After 24 hours of soak testing

```bash
# Soak test: 24-hour cluster run
./tests/cluster/start.sh
./target/release/chronik-bench \
    --concurrency 64 \
    --duration 86400s \  # 24 hours
    --mode produce-consume

# Monitor logs for:
# - WAL fallback frequency (should be low)
# - Memory usage (should be stable)
# - Error rate (should be 0%)
```

**Go/No-Go Checklist**:
- [ ] Soak test completes 24 hours with 0 crashes
- [ ] Memory usage stable (no leaks)
- [ ] WAL fallback works in all catchup scenarios
- [ ] Performance regression < 10% (target: > 8,000 msg/s)
- [ ] All integration tests green

---

## Part 7: Monitoring & Alerting

### Metrics to Track

**Production Metrics**:
```rust
// crates/chronik-wal/src/raft_storage_impl.rs
use chronik_monitoring::MetricsRecorder;

fn entries(&self, low: u64, high: u64, ...) -> raft::Result<Vec<Entry>> {
    if low < first_index {
        // Cold path
        MetricsRecorder::record_raft_wal_fallback();
        let start = Instant::now();
        let entries = self.read_entries_from_wal(low, high).await?;
        MetricsRecorder::record_raft_wal_fallback_duration(start.elapsed());
        return Ok(entries);
    }
    
    // Hot path
    MetricsRecorder::record_raft_entries_hit();
    // ...
}
```

**Prometheus Queries**:
```promql
# WAL fallback rate (should be low)
rate(chronik_raft_wal_fallback_total[5m])

# WAL fallback latency (should be < 10ms p99)
histogram_quantile(0.99, rate(chronik_raft_wal_fallback_duration_seconds_bucket[5m]))

# Hot path hit rate (should be > 99%)
rate(chronik_raft_entries_hit_total[5m]) / 
  (rate(chronik_raft_entries_hit_total[5m]) + rate(chronik_raft_wal_fallback_total[5m]))
```

**Alerts**:
```yaml
# Alert if WAL fallback rate too high (indicates memory pressure)
- alert: RaftWalFallbackHigh
  expr: rate(chronik_raft_wal_fallback_total[5m]) > 10
  annotations:
    summary: "Raft WAL fallback rate > 10/sec (may need more in-memory entries)"

# Alert if fallback latency too high (indicates disk I/O issues)
- alert: RaftWalFallbackSlow
  expr: histogram_quantile(0.99, rate(chronik_raft_wal_fallback_duration_seconds_bucket[5m])) > 0.050
  annotations:
    summary: "Raft WAL fallback p99 latency > 50ms (check disk performance)"
```

---

## Part 8: Risks & Mitigations

### Risk 1: WAL Read Performance Degrades Under Load

**Scenario**: 1000s of catchup requests → disk I/O saturation → cluster slow

**Mitigation**:
- LRU cache for recently-read WAL entries (Phase 3)
- Rate-limit catchup requests (protect disk from storm)
- Monitor disk I/O metrics

**Rollback Plan**: Revert to increased unstable log retention (50K entries).

---

### Risk 2: `block_in_place` Causes Tokio Scheduler Issues

**Scenario**: Blocking async runtime → other tasks starved

**Mitigation**:
- Tokio's `block_in_place` is designed for this (moves task to blocking thread pool)
- Monitor task queue depth
- Set timeout on WAL reads (max 100ms)

**Rollback Plan**: Use separate thread pool for WAL reads instead of `block_in_place`.

---

### Risk 3: WAL File Corruption Causes Catchup Failures

**Scenario**: Corrupted WAL segment → `from_bytes()` fails → catchup fails

**Mitigation**:
- Already have CRC validation in WalRecord (skip for V2, but V1 has it)
- Return `StorageError::Unavailable` on parse error
- Raft will retry or trigger snapshot installation

**Rollback Plan**: None needed (already handled by error propagation).

---

## Part 9: Testing Checklist

### Pre-Merge Testing

**Unit Tests**:
- [ ] `test_entries_in_memory` (hot path)
- [ ] `test_entries_wal_fallback` (cold path)
- [ ] `test_entries_partial_overlap` (hybrid)
- [ ] `test_wal_read_empty_range`
- [ ] `test_wal_read_with_size_limit`

**Integration Tests**:
- [ ] Network partition catchup (50K messages)
- [ ] High concurrency stress (128 producers, 60s)
- [ ] Crash recovery (node restart mid-produce)
- [ ] Leader transfer (if implemented)
- [ ] Snapshot installation (follower far behind)

**Performance Tests**:
- [ ] Baseline: 9,287 msg/s (current v2.2.8)
- [ ] Target: > 8,000 msg/s (< 10% regression)
- [ ] Latency: p99 < 20ms (vs 15.8ms baseline)

**Soak Test**:
- [ ] 24-hour continuous run
- [ ] No crashes, no OOM
- [ ] Memory stable (no leaks)
- [ ] WAL fallback rare (< 1% of calls)

---

## Part 10: Documentation Updates

### User-Facing Docs

**File**: `docs/RAFT_DEPLOYMENT_GUIDE.md`

Add section:
```markdown
## Log Retention Tuning

Chronik keeps recent Raft entries in-memory for fast access. Older entries
are read from WAL when needed (e.g., follower catchup).

**Default**: 10,000 entries (~10MB per node)

**Tuning**:
- Increase if followers frequently fall behind (reduces disk reads)
- Decrease if memory-constrained (trades disk I/O for RAM)

**Environment Variable**:
```bash
CHRONIK_RAFT_LOG_RETENTION=50000  # Keep 50K entries in-memory
```

**Metrics to Monitor**:
- `chronik_raft_wal_fallback_total` - WAL reads (should be low)
- `chronik_raft_entries_hit_total` - In-memory hits (should be high)
```

### Developer Docs

**File**: `crates/chronik-wal/README.md`

Add section:
```markdown
## Raft Storage Architecture

### Two-Tier Storage

1. **In-Memory (Hot)**: Last 10K entries, ~1μs latency
2. **WAL (Cold)**: All historical entries, ~1-5ms latency

### Automatic Fallback

When `Storage::entries(low, high)` requests entries older than in-memory:
1. Log warning (expected during catchup)
2. Read from WAL asynchronously using `block_in_place`
3. Cache results (Phase 3 optimization)
4. Return to Raft

### Performance Characteristics

- **Normal operation**: 99.9% hot path (in-memory)
- **Catchup scenarios**: 10-50% cold path (WAL read)
- **Impact**: Correctness guaranteed, minimal latency overhead
```

---

## Part 11: Success Criteria

### MVP (Minimum Viable Product)

**Must Have**:
- ✅ High concurrency (128 producers) works for 5+ minutes
- ✅ No panics (raft log_unstable or otherwise)
- ✅ Network partition catchup works correctly
- ✅ Performance: > 8,000 msg/s (< 10% regression)

### Stretch Goals

**Nice to Have**:
- ⭐ Performance: > 9,000 msg/s (match baseline)
- ⭐ LRU cache for WAL reads (Phase 3)
- ⭐ Snapshot compression (reduce transfer time)

---

## Appendix: Alternative Approaches Considered

### A1: Use raft-rs's `Storage::snapshot()` API

**Idea**: Return `Compacted` error, Raft calls `snapshot()` to get state.

**Why Rejected**: Raft calls `entries()` BEFORE checking `snapshot()` during conflict resolution. Doesn't solve the panic.

### A2: Patch raft-rs Library

**Idea**: Fork tikv/raft-rs, fix `log_unstable.rs:201` to handle stable storage.

**Why Rejected**: Maintenance burden, miss upstream fixes, complex change in core Raft algorithm.

### A3: Pre-load Entries on Follower Startup

**Idea**: On startup, read ALL WAL entries into memory.

**Why Rejected**: OOM for large logs (1M entries = 1GB RAM), doesn't scale.

---

## Timeline Summary

| Phase | Task | Duration | Outcome |
|-------|------|----------|---------|
| 1 (Emergency) | Increase unstable log retention | 30 min | Buys time, reduces panic frequency |
| 2 (Investigation) | Root cause analysis (concurrency) | 4 hours | Identify PRIMARY bottleneck |
| 3 (Implementation) | WAL read fallback | 3 hours | Semantically correct solution |
| 4 (Testing) | Unit + integration tests | 3 hours | Verify correctness |
| 5 (Validation) | Performance + soak testing | 26 hours | Confirm production-ready |
| **Total** | | **~36 hours** | Production-ready v2.2.8 |

---

## Conclusion

**Recommended Path**: Hybrid Option A + E

**Why**:
- ✅ Production-ready (correct Raft semantics)
- ✅ Low risk (hot path unchanged, cold path is fallback)
- ✅ Testable (clear acceptance criteria)
- ✅ Maintainable (no library forks, no massive rewrites)

**Next Step**: Execute Phase 1 (emergency fix) immediately, then proceed with Phase 2 (production fix).

---

**Document Version**: 1.0  
**Last Updated**: 2025-11-10  
**Status**: COMPREHENSIVE PLAN - READY FOR EXECUTION
