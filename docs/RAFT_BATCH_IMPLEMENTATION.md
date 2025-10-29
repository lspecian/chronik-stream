# Raft Batch Proposal Implementation Plan

## Problem Statement

Current Raft implementation has 750x performance degradation (27K msg/s → 37 msg/s) due to **RwLock contention** on tikv/raft-rs's `RawNode`. Every produce request blocks on `raw_node.write()` lock, serializing all 64 concurrent producers through a single bottleneck.

**Measured with instrumentation:**
- Single message: ~40ms Raft commit time
- With concurrency=64: p50 latency = 1,900ms (queuing at RwLock)
- Throughput: 64 / 1.9s = 33.7 msg/s ✓

## Solution: WAL-Batch-to-Raft (KIP-1150 BatchCoordinator Pattern)

**Key Insight:** Propose WAL *batches* to Raft, not individual messages.

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                   Standalone Mode (UNCHANGED)                   │
│  Producer → ProduceHandler → WAL → Return (27K+ msg/s)         │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                   Raft Cluster Mode (NEW)                       │
│                                                                 │
│  Step 1: Producer → ProduceHandler → WAL (immediate, no block) │
│          └─> Return to producer (acks=0,1)                     │
│                                                                 │
│  Step 2: WAL batches messages (GroupCommitWal, 10-100ms)      │
│          └─> On commit_batch() → Trigger batch proposal       │
│                                                                 │
│  Step 3: Batch proposer → Raft propose(BATCH metadata) once   │
│          └─> ONE RwLock acquisition per batch                 │
│                                                                 │
│  Step 4: Raft replicates → Followers apply → Done            │
│          └─> Notify waiting producers (acks=-1)               │
└─────────────────────────────────────────────────────────────────┘
```

### Expected Performance

**Current:**
- 64 messages × 40ms each (serialized) = 2,560ms
- Throughput: 25 msg/s

**With batching:**
- 64 messages → WAL (parallel, <1ms each)
- 1 batch × 40ms Raft proposal = 40ms
- Throughput: 64 / 0.04s = **1,600 msg/s** (64x improvement!)

**With pipelined batches:**
- Multiple batches overlapping
- Throughput: **10,000+ msg/s** (only 3x slower than standalone)

## Implementation Steps

### Phase 1: Refactor Produce Path (Raft Mode)

**Current flow:**
```rust
// produce_handler.rs (lines 1204-1305)
if raft_manager.has_replica(topic, partition) {
    // Serialize canonical record
    let serialized = bincode::serialize(&canonical_record)?;

    // BLOCKS HERE until Raft commit!
    let raft_index = raft_manager.propose(topic, partition, serialized).await?;

    return Ok(response);
}
```

**New flow:**
```rust
if raft_manager.has_replica(topic, partition) {
    // Step 1: Write to WAL immediately (no Raft blocking!)
    let base_offset = self.wal_manager.append_canonical(
        topic, partition, canonical_bytes, ...
    ).await?;

    // Step 2: For acks=0,1 - return immediately
    if acks < 2 {
        return Ok(response_with_offset(base_offset));
    }

    // Step 3: For acks=-1 - wait for Raft commit notification
    let commit_rx = raft_manager.register_offset_waiter(topic, partition, base_offset).await;
    commit_rx.await?;

    return Ok(response_with_offset(base_offset));
}
```

### Phase 2: Implement Batch Proposal Hook

**Add to `GroupCommitWal::commit_batch()`:**
```rust
// group_commit.rs after fsync (line ~700)
async fn commit_batch(...) -> Result<()> {
    // ... existing fsync logic ...

    // NEW: Notify Raft batch proposer (if Raft enabled)
    if let Some(batch_notifier) = queue.batch_notifier.as_ref() {
        let batch_metadata = BatchMetadata {
            topic: topic.clone(),
            partition,
            base_offset: batch_start_offset,
            last_offset: batch_end_offset,
            record_count: batch_size,
            batch_id: segment_id, // For idempotency
        };

        // Fire-and-forget async proposal (don't block WAL commit!)
        let _ = batch_notifier.try_send(batch_metadata);
    }

    Ok(())
}
```

### Phase 3: Batch Proposal Worker

**New component: `RaftBatchProposer`**
```rust
// raft_integration.rs
pub struct RaftBatchProposer {
    raft_manager: Arc<RaftReplicaManager>,
    batch_rx: mpsc::Receiver<BatchMetadata>,
    pending_batches: Arc<DashMap<(String, i32, i64), Vec<oneshot::Sender<()>>>>,
}

impl RaftBatchProposer {
    pub async fn run(self) {
        while let Some(batch) = self.batch_rx.recv().await {
            // Propose batch metadata to Raft (ONE lock acquisition)
            let serialized = bincode::serialize(&batch)?;

            match self.raft_manager.propose(
                &batch.topic, batch.partition, serialized
            ).await {
                Ok(_) => {
                    // Notify all waiting producers for offsets in this batch
                    self.notify_batch_committed(&batch).await;
                }
                Err(e) => {
                    error!("Batch proposal failed: {}", e);
                    self.notify_batch_failed(&batch, e).await;
                }
            }
        }
    }
}
```

### Phase 4: State Machine Changes

**Current:** State machine writes to WAL on apply
**New:** State machine validates batch is already in WAL

```rust
// raft_integration.rs ChronikStateMachine::apply()
async fn apply(&mut self, entry: RaftEntry) -> Result<()> {
    let batch: BatchMetadata = bincode::deserialize(&entry.data)?;

    // Validate batch exists in local WAL (should already be there)
    self.wal_manager.validate_batch_exists(
        &batch.topic, batch.partition, batch.base_offset, batch.last_offset
    ).await?;

    // Update high watermark
    self.metadata.update_partition_offset(
        &batch.topic, batch.partition as u32, batch.last_offset + 1, 0
    ).await?;

    info!("Batch committed via Raft: {}-{} offsets={}-{}",
        batch.topic, batch.partition, batch.base_offset, batch.last_offset);

    Ok(())
}
```

## Critical Design Decisions

### 1. Standalone Mode Must Remain Unchanged

**Requirement:** Standalone mode should have ZERO overhead from Raft code.

**Implementation:**
```rust
// produce_handler.rs
#[cfg(feature = "raft")]
{
    if let Some(ref raft_manager) = self.raft_manager {
        if raft_manager.has_replica(topic, partition) {
            // Raft path (new batched flow)
        }
    }
}

// If not Raft-enabled, fall through to existing standalone path
// (UNCHANGED - preserves 27K+ msg/s performance)
```

### 2. Acks Semantics

- **acks=0**: Return immediately after WAL write (no Raft wait)
- **acks=1**: Return after local WAL fsync (no Raft wait)
- **acks=-1 (all)**: Wait for Raft quorum commit

### 3. Batch Granularity

Reuse `GroupCommitWal` batching (already has `max_wait_time_ms` config):
- Low-latency: 10ms batches
- Balanced: 100ms batches (default)
- High-throughput: 500ms batches

### 4. Failure Handling

**Scenario:** WAL write succeeds, but Raft proposal fails
- Message is in local WAL (durable)
- But NOT replicated to followers
- **Solution:** Leader re-proposes uncommitted batches on startup

**Scenario:** Producer timeout waiting for acks=-1
- Message is in WAL (safe)
- Producer retries with different base_offset
- Idempotency via producer_id + sequence prevents duplicates

## Testing Plan

### Test 1: Standalone Mode Regression
```bash
# Should maintain 27K+ msg/s
cargo build --release --bin chronik-server
./target/release/chronik-server --advertised-addr localhost standalone &
./target/release/chronik-bench --concurrency 64 --duration 20s
```

**Expected:** 27,000+ msg/s (no regression)

### Test 2: Raft Cluster with Batching
```bash
# Start 3-node cluster
./scripts/start_3node_cluster.sh

# Benchmark
./target/release/chronik-bench --concurrency 64 --duration 20s
```

**Expected:** 10,000+ msg/s (vs current 37 msg/s)

### Test 3: Acks Semantics
```python
# Test acks=0,1,-1 behavior
producer_acks_0 = KafkaProducer(acks=0)  # Should be fastest
producer_acks_1 = KafkaProducer(acks=1)  # Wait for local
producer_acks_all = KafkaProducer(acks='all')  # Wait for Raft
```

## Rollout Strategy

1. Implement Phase 1-4 in feature branch
2. Test standalone mode (no regression)
3. Test Raft cluster (10K+ msg/s target)
4. Merge to main
5. Document in CLAUDE.md

## Risks and Mitigations

**Risk:** Breaking standalone mode performance
**Mitigation:** Keep Raft code behind `#[cfg(feature = "raft")]` and runtime checks

**Risk:** Complex state management for batch tracking
**Mitigation:** Use DashMap for lock-free concurrent access

**Risk:** Message loss if leader fails mid-batch
**Mitigation:** Raft log recovery + WAL replay on follower promotion

## Success Criteria

- ✅ Standalone: 27K+ msg/s (no regression)
- ✅ Raft cluster: 10K+ msg/s (270x improvement vs 37 msg/s)
- ✅ Latency: p99 < 200ms (vs current 2,824ms)
- ✅ All Kafka clients work (kafka-python, java, KSQL)
