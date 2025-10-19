# FetchHandler Raft Integration Design

## Overview

This document describes the design for integrating Raft consensus into the FetchHandler to support multi-partition replicated reads in cluster mode.

## Current Architecture (Standalone Mode)

**FetchHandler v1.3.65:**
- Serves reads from local WAL and segments only
- No cluster awareness
- No leader/follower concept
- Single-node assumption

**Key Components:**
```rust
pub struct FetchHandler {
    segment_reader: Arc<SegmentReader>,
    metadata_store: Arc<dyn MetadataStore>,
    object_store: Arc<dyn ObjectStoreTrait>,
    wal_manager: Option<Arc<WalManager>>,
    segment_index: Option<Arc<SegmentIndex>>,
    produce_handler: Option<Arc<ProduceHandler>>,
    state: Arc<RwLock<FetchState>>,
}
```

**Read Path:**
1. Check if topic/partition exists
2. Get high watermark from ProduceHandler (or metadata)
3. Try to fetch from ProduceHandler's in-memory buffer (hot path)
4. On miss, try WAL (Tier 1)
5. On miss, try local segments (Tier 2)
6. On miss, try Tantivy archives in S3 (Tier 3)

## Raft Integration Requirements

### Design Goal
Enable reads in a Raft cluster with multiple nodes replicating data for the same partition.

### Challenges
1. **Data Consistency**: Followers may lag behind leader
2. **Read-After-Write**: Clients expect to read their own writes
3. **Stale Reads**: Followers may serve old data
4. **Network Partitions**: Leaders may change during fetch

### Two Approaches

#### Option A: Follower Reads (RECOMMENDED)

**Pros:**
- ✅ Load balancing across cluster
- ✅ Better read scalability
- ✅ Lower latency for nearby consumers
- ✅ Kafka-compatible behavior (Kafka allows follower reads since KIP-392)
- ✅ No additional client-side retry logic needed

**Cons:**
- ⚠️ Potential stale reads (bounded by commit lag)
- ⚠️ Complexity in determining "committed" data

**Implementation Strategy:**
```rust
// In fetch_partition():
if let Some(ref raft_manager) = self.raft_manager {
    if raft_manager.is_enabled() {
        // Check committed offset from Raft
        let committed_offset = raft_manager.get_committed_offset(topic, partition)?;

        // Only serve data up to committed offset
        if fetch_offset >= committed_offset {
            // Wait or return empty (data not yet committed)
            return empty_response_with_high_watermark(committed_offset);
        }
    }
}

// Normal fetch path (but capped at committed_offset)
```

**Consistency Guarantees:**
- **Read Committed**: Only serve data that's been committed by Raft (replicated to majority)
- **Monotonic Reads**: A consumer will never see data go backwards (committed offset is monotonic)
- **Bounded Staleness**: Max lag = time since last Raft commit

#### Option B: Leader-Only Reads with Redirect

**Pros:**
- ✅ Strong consistency (always read latest committed data)
- ✅ Simple implementation
- ✅ No stale reads

**Cons:**
- ❌ No read scalability (all reads hit leader)
- ❌ Leader becomes bottleneck
- ❌ Higher latency for remote consumers
- ❌ Requires client retry logic
- ❌ Not standard Kafka behavior

**Implementation Strategy:**
```rust
// In fetch_partition():
if let Some(ref raft_manager) = self.raft_manager {
    if raft_manager.is_enabled() {
        if !raft_manager.is_leader(topic, partition) {
            let leader_id = raft_manager.get_leader(topic, partition)?;
            return error_response(ErrorCode::NOT_LEADER_FOR_PARTITION, leader_id);
        }
    }
}

// Normal fetch path (only on leader)
```

**Consistency Guarantees:**
- **Linearizable Reads**: Always see latest committed data
- **Read-Your-Writes**: Guaranteed if client reads from same leader

## RECOMMENDATION: Option A (Follower Reads)

**Rationale:**
1. **Kafka Compatibility**: Kafka added follower reads in KIP-392 (v2.4.0)
2. **Scalability**: Critical for high-throughput clusters
3. **Operational Excellence**: Load balancing is a core requirement
4. **Consistency is Acceptable**: "Read Committed" is sufficient for streaming use cases
5. **Performance**: Avoids single leader bottleneck

**Trade-offs Accepted:**
- Bounded staleness (typically <100ms in healthy cluster)
- Consumers may see slightly delayed data on followers

## Implementation Plan

### Phase 1: Add Raft Manager to FetchHandler

```rust
pub struct FetchHandler {
    // Existing fields...
    raft_manager: Option<Arc<RaftReplicaManager>>,
}

impl FetchHandler {
    pub fn new_with_wal_and_raft(
        segment_reader: Arc<SegmentReader>,
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        wal_manager: Arc<WalManager>,
        produce_handler: Arc<ProduceHandler>,
        raft_manager: Arc<RaftReplicaManager>,
    ) -> Self {
        Self {
            segment_reader,
            metadata_store,
            object_store,
            wal_manager: Some(wal_manager),
            segment_index: None,
            produce_handler: Some(produce_handler),
            raft_manager: Some(raft_manager),
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
                segment_cache: HashMap::new(),
            })),
        }
    }
}
```

### Phase 2: Implement Follower Read Logic

```rust
async fn fetch_partition(&self, ...) -> Result<FetchResponsePartition> {
    // Existing checks (topic exists, partition valid)...

    // NEW: Raft-aware high watermark and commit offset
    let (high_watermark, committed_offset) = if let Some(ref raft_manager) = self.raft_manager {
        if raft_manager.is_enabled() {
            let replica = raft_manager.get_replica(topic, partition)
                .ok_or_else(|| Error::NotFound(format!("No replica for {}-{}", topic, partition)))?;

            // committed_offset = highest offset that's been committed by Raft (safe to serve)
            // high_watermark = next offset to be produced (may be uncommitted on followers)
            let committed = replica.committed_index(); // Raft commit index
            let hwm = self.produce_handler
                .as_ref()
                .map(|ph| ph.get_high_watermark(topic, partition).await.unwrap_or(0))
                .unwrap_or(0);

            (hwm, committed)
        } else {
            // Standalone mode: no Raft, everything is immediately "committed"
            let hwm = get_high_watermark_from_produce_handler();
            (hwm, hwm)
        }
    } else {
        // No Raft manager: standalone mode
        let hwm = get_high_watermark_from_produce_handler();
        (hwm, hwm)
    };

    // Only serve data up to committed offset
    if fetch_offset >= committed_offset {
        // Data not yet committed - return empty with current committed offset
        info!(
            "Fetch for {}-{} at offset {} waiting for commit (committed_offset={})",
            topic, partition, fetch_offset, committed_offset
        );

        // Option 1: Wait for commit (if max_wait_ms > 0)
        if max_wait_ms > 0 {
            let timeout = Duration::from_millis(max_wait_ms as u64);
            // Poll for commit (with exponential backoff)
            if let Ok(new_committed) = wait_for_commit(replica, fetch_offset, timeout).await {
                // Data committed during wait, proceed with fetch
                committed_offset = new_committed;
            } else {
                // Timeout: return empty
                return Ok(empty_response(partition, high_watermark, committed_offset));
            }
        } else {
            // No wait: return empty immediately
            return Ok(empty_response(partition, high_watermark, committed_offset));
        }
    }

    // Normal fetch path (cap at committed_offset)
    let actual_max_offset = std::cmp::min(
        fetch_offset + (max_bytes as i64 / 100), // Estimate records
        committed_offset
    );

    // Fetch records [fetch_offset, actual_max_offset)
    fetch_records_from_storage(topic, partition, fetch_offset, actual_max_offset).await
}
```

### Phase 3: Add Configuration

```rust
// Environment variable: CHRONIK_FETCH_FROM_FOLLOWERS (default: true)
pub struct FetchHandlerConfig {
    /// Enable follower reads (default: true)
    /// If false, only leaders can serve fetches (Option B behavior)
    pub allow_follower_reads: bool,

    /// Maximum wait time for commit in follower reads (ms)
    pub follower_read_max_wait_ms: u64,
}

impl Default for FetchHandlerConfig {
    fn default() -> Self {
        let allow_follower_reads = std::env::var("CHRONIK_FETCH_FROM_FOLLOWERS")
            .unwrap_or_else(|_| "true".to_string())
            .parse()
            .unwrap_or(true);

        Self {
            allow_follower_reads,
            follower_read_max_wait_ms: 1000, // 1 second max wait
        }
    }
}
```

### Phase 4: Handle Leader Changes

```rust
// If replica loses leadership during fetch, client will get partial data
// and retry with next fetch (standard Kafka behavior)

// Optional: Add preferred_read_replica to guide clients to stable replicas
fn fetch_partition(&self, ...) -> Result<FetchResponsePartition> {
    // After successful fetch...
    let preferred_replica = if let Some(ref raft_manager) = self.raft_manager {
        if raft_manager.is_enabled() {
            // Suggest this node if we're leader or stable follower
            let replica = raft_manager.get_replica(topic, partition)?;
            if replica.is_leader() || is_stable_follower(replica) {
                Some(self.config.node_id)
            } else {
                // Suggest leader node
                raft_manager.get_leader(topic, partition)
            }
        } else {
            None
        }
    } else {
        None
    };

    Ok(FetchResponsePartition {
        partition,
        error_code: 0,
        high_watermark: committed_offset, // Advertise committed offset as HWM
        preferred_read_replica: preferred_replica.unwrap_or(-1),
        records: fetched_records,
        ...
    })
}
```

## RaftReplicaManager API Additions

Need to add these methods to `RaftReplicaManager`:

```rust
impl RaftReplicaManager {
    /// Get the committed offset for a partition (safe to read up to this offset)
    pub fn get_committed_offset(&self, topic: &str, partition: i32) -> Option<i64> {
        let replica = self.get_replica(topic, partition)?;
        Some(replica.committed_index() as i64)
    }

    /// Check if a replica is a stable follower (not lagging)
    pub fn is_stable_follower(&self, topic: &str, partition: i32, max_lag_ms: u64) -> bool {
        let replica = self.get_replica(topic, partition)?;
        if replica.is_leader() {
            return true; // Leader is always stable
        }

        // Check if follower lag is within acceptable threshold
        let last_contact = replica.last_leader_contact_ms();
        last_contact < max_lag_ms
    }
}
```

## Integration with IntegratedKafkaServer

```rust
// In IntegratedKafkaServer::new_with_raft()
let fetch_handler = if let Some(ref raft_manager) = raft_manager {
    Arc::new(FetchHandler::new_with_wal_and_raft(
        segment_reader.clone(),
        metadata_store.clone(),
        object_store.clone(),
        wal_manager.clone(),
        produce_handler.clone(),
        raft_manager.clone(),
    ))
} else {
    Arc::new(FetchHandler::new_with_wal(
        segment_reader.clone(),
        metadata_store.clone(),
        object_store.clone(),
        wal_manager.clone(),
        produce_handler.clone(),
    ))
};
```

## Testing Strategy

### Unit Tests
1. Test follower read with committed data (should succeed)
2. Test follower read with uncommitted data (should wait or return empty)
3. Test leader read (should always see latest)
4. Test commit wait timeout
5. Test preferred_read_replica selection

### Integration Tests
1. **3-node cluster test**: Produce to leader, fetch from follower, verify consistency
2. **Lag test**: Disconnect follower, produce, reconnect, verify follower catches up
3. **Leader failover test**: Kill leader during fetch, verify client retries work
4. **Concurrent fetch test**: Multiple consumers fetching from different replicas

### Test Plan File
See `FETCH_HANDLER_RAFT_TEST_PLAN.md`

## Performance Considerations

### Latency
- **Leader reads**: No additional latency (same as standalone)
- **Follower reads**: +0-100ms depending on commit lag
- **Wait for commit**: Up to max_wait_ms (configurable)

### Throughput
- **Read scalability**: Linear with number of followers (3 nodes = 3x read capacity)
- **Write throughput**: Unchanged (still constrained by Raft replication)

### Memory
- **Minimal overhead**: No additional buffering required
- **Raft state**: Already tracked by PartitionReplica

## Configuration Summary

```bash
# Enable follower reads (default: true)
CHRONIK_FETCH_FROM_FOLLOWERS=true

# Max wait for commit on follower reads (default: 1000ms)
CHRONIK_FETCH_FOLLOWER_MAX_WAIT_MS=1000

# Example: Disable follower reads (force leader-only, Option B)
CHRONIK_FETCH_FROM_FOLLOWERS=false ./chronik-server --raft standalone
```

## Backward Compatibility

- **Standalone mode**: No changes, works exactly as before
- **Raft mode without config**: Defaults to follower reads enabled
- **Existing clients**: No protocol changes required

## Open Questions

1. **Q:** Should we implement read-your-writes guarantee?
   **A:** Not in Phase 1. This requires sticky sessions or client-side tracking. Add in Phase 2 if needed.

2. **Q:** What if committed_offset regresses during leader change?
   **A:** Kafka protocol handles this - client will get OFFSET_OUT_OF_RANGE and reset.

3. **Q:** Should we expose follower lag metrics?
   **A:** Yes, add Prometheus metrics for follower lag per partition.

## Next Steps

1. ✅ Design document complete
2. ⬜ Implement RaftReplicaManager API additions
3. ⬜ Implement FetchHandler Raft integration
4. ⬜ Add configuration support
5. ⬜ Write unit tests
6. ⬜ Write integration tests
7. ⬜ Performance benchmarking
8. ⬜ Documentation updates

## References

- **Kafka KIP-392**: Fetch from Followers
- **Raft Paper**: Section 8 (Client Interaction)
- **Chronik Architecture**: CLAUDE.md, RAFT_IMPLEMENTATION_PLAN.md
- **Existing Code**: `produce_handler.rs` (Raft integration pattern)
