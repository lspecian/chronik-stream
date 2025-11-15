# Auto-Create Topic Bug on Follower Nodes (v2.2.7)

**Date:** 2025-11-09
**Severity:** CRITICAL
**Impact:** Follower nodes cannot handle auto-create, causing "No ISR found" errors
**Status:** 🐛 ROOT CAUSE IDENTIFIED

---

## Summary

When a Kafka client connects to a **Raft follower node** (not the leader) and tries to produce to a non-existent topic, auto-creation **partially succeeds** but leaves the partition in an inconsistent state without ISR, causing continuous leader election failures and throughput degradation.

---

## The Bug

### Code Flow (Follower Node)

1. **Client produces to non-existent topic** (e.g., `chronik-benchmark-final`)
2. **ProduceHandler.auto_create_topic()** is called ([produce_handler.rs:2034](../crates/chronik-server/src/produce_handler.rs#L2034))
3. **metadata_store.create_topic()** succeeds ([line 2095](../crates/chronik-server/src/produce_handler.rs#L2095))
   - Creates topic metadata **locally only** (not via Raft)
4. **initialize_raft_partitions()** is called ([line 2101](../crates/chronik-stream/crates/chronik-server/src/produce_handler.rs#L2101))
5. **Follower returns early** ([line 2374-2379](../crates/chronik-server/src/produce_handler.rs#L2374-L2379)):
   ```rust
   if !raft.am_i_leader() {
       debug!("Skipping Raft partition initialization - this node is a Raft follower");
       return Ok(());  // ❌ EARLY RETURN - ISR NEVER SET!
   }
   ```
6. **Result**: Topic exists locally, but:
   - ❌ Partition leader not set
   - ❌ ISR not initialized
   - ❌ Partition assignments not proposed to Raft
   - ❌ Other nodes don't know about the topic

### Why This Breaks

```
Node 1 (Leader):  Has topic metadata from Raft replication ✅
Node 2 (Follower): Client connects here, auto-creates topic locally ❌
Node 3 (Follower): Doesn't know about topic at all ❌

Result:
- Node 2 thinks it has the topic, but partition has no leader/ISR
- Leader election fails: "No ISR found for partition chronik-benchmark-final-0"
- Produces either fail or succeed locally without replication
- Cluster inconsistent
```

---

## Evidence from Logs

### Node 1 (Raft Leader) - Continuous Failures

```
[2025-11-08T23:14:32.790320Z] WARN chronik_server::leader_election: Leader timeout for chronik-default-0 (leader=1), triggering election
[2025-11-08T23:14:32.790357Z] ERROR chronik_server::leader_election: ❌ Failed to elect leader for chronik-default-0: No ISR found for partition
[2025-11-08T23:14:35.791886Z] WARN chronik_server::leader_election: Leader timeout for chronik-default-0 (leader=1), triggering election
[2025-11-08T23:14:35.791922Z] ERROR chronik_server::leader_election: ❌ Failed to elect leader for chronik-default-0: No ISR found for partition
... (repeats every 3 seconds)
```

### Why This Happens

The topic `chronik-default` or `chronik-benchmark-final` was created by a client connecting to a follower node. The follower:
1. Created topic metadata locally ✅
2. Skipped Raft initialization (not the leader) ❌
3. Never set ISR via Raft ❌

Result: All 3 nodes have inconsistent metadata, causing continuous leader election failures.

---

## Performance Impact

### Throughput Degradation

**Observed:**
- 1,679 produce requests over 158 seconds = **10.6 req/s**
- Expected: >1,000 req/s for 128 concurrent producers

**Root Cause:**
1. Leader election runs every 3 seconds (triggered by "No ISR found")
2. Each election attempt:
   - Locks partition metadata
   - Blocks produce requests
   - Fails immediately (no ISR)
   - Repeats in 3 seconds
3. Producers experience:
   - High latency (waiting for locks)
   - Retries (metadata inconsistency)
   - Backpressure (librdkafka throttling)

**Math:**
- 10.6 req/s ÷ 128 producers = **0.08 req/s per producer**
- Expected: ~8-10 req/s per producer (1,000 req/s ÷ 128)
- **Performance degradation: 99% loss!**

---

## Why v2.2.3 Fix Didn't Prevent This

The v2.2.3 fix ([PARTITION_LEADERSHIP_FIX.md](./PARTITION_LEADERSHIP_FIX.md)) correctly prevents followers from proposing during **server startup**:

```rust
// At server startup (integrated_server.rs:593-707)
if !this_node_is_leader {
    info!("Skipping metadata initialization - this node is a Raft follower");
    // ✅ CORRECT: Followers don't initialize existing topics at startup
}
```

**But runtime auto-creation still broken:**

```rust
// At runtime produce (produce_handler.rs:2101-2104)
if self.raft_cluster.is_some() {
    if let Err(e) = self.initialize_raft_partitions(...).await {
        warn!("Failed to initialize Raft partitions: {:?}", e);
        // Continue anyway - follower will receive metadata via Raft replication
        // ❌ BUG: Follower returns Ok() without initializing, so "Continue anyway" means
        //         "Continue with broken partition metadata"
    }
}
```

The comment says "follower will receive metadata via Raft replication", but:
1. Metadata was created **locally** (not via Raft)
2. No Raft proposal ever happens
3. Other nodes never receive anything
4. Cluster diverges

---

## The Fix

### Option 1: Forward to Raft Leader (Recommended)

When a follower receives auto-create request, **forward to Raft leader** instead of creating locally:

```rust
pub async fn auto_create_topic(&self, topic_name: &str) -> Result<TopicMetadata> {
    // Check if we're a Raft follower
    if let Some(ref raft) = self.raft_cluster {
        if !raft.am_i_leader() {
            // Follower should NOT create topics locally
            // Option A: Forward to Raft leader via HTTP/gRPC
            let leader_id = raft.get_leader_id()?;
            let leader_addr = raft.get_peer_addr(leader_id)?;

            info!("Follower forwarding topic creation '{}' to Raft leader {}", topic_name, leader_id);
            return self.forward_create_topic_to_leader(leader_addr, topic_name).await;

            // Option B: Return error and let client retry with correct broker
            // return Err(Error::NotLeaderForPartition(leader_id));
        }
    }

    // Only Raft leader continues with actual creation
    info!("Raft leader creating topic '{}'", topic_name);

    let topic_config = TopicConfig { ... };
    let metadata = self.metadata_store.create_topic(topic_name, topic_config).await?;

    // Initialize partitions (only runs on leader)
    if let Err(e) = self.initialize_raft_partitions(topic_name, metadata.config.partition_count).await {
        error!("Failed to initialize Raft partitions: {:?}", e);
        return Err(e);  // ✅ FAIL FAST instead of continuing
    }

    Ok(metadata)
}
```

### Option 2: MetadataStore via Raft (Architectural Fix)

Make `metadata_store.create_topic()` **always go through Raft** in cluster mode:

```rust
impl RaftMetadataStore {
    async fn create_topic(&self, topic_name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Propose CreateTopic command to Raft (not just partition metadata)
        self.raft.propose(MetadataCommand::CreateTopic {
            topic: topic_name.to_string(),
            config,
        }).await?;

        // Wait for command to be committed and applied
        self.wait_for_topic(topic_name).await
    }
}
```

This ensures ALL metadata changes go through Raft consensus, not just partition assignments.

---

## Recommended Solution

**Hybrid Approach:**

1. **Short-term (v2.2.7)**: Implement Option 1 - Forward to leader
   - Fast fix, minimal code changes
   - Followers return `NOT_LEADER_FOR_PARTITION` error
   - Clients retry with correct broker from metadata
   - Works with existing Kafka client behavior

2. **Long-term (v2.3.0)**: Implement Option 2 - MetadataStore via Raft
   - Architectural fix, all metadata operations consistent
   - Aligns with Kafka KRaft design
   - Requires refactoring metadata store interface

---

## Testing Plan

### Test Case 1: Auto-Create on Follower

```bash
# Start 3-node cluster
./tests/cluster/start.sh

# Connect client to follower (node 2 on port 9093)
echo "test message" | kafka-console-producer \
  --bootstrap-server localhost:9093 \
  --topic test-auto-create

# Expected (after fix):
# - Client receives NOT_LEADER error
# - Client retries with leader
# - Topic created successfully via Raft
# - All nodes have consistent metadata
# - No "No ISR found" errors
```

### Test Case 2: Benchmark on Mixed Nodes

```bash
# Run chronik-bench against all 3 nodes
chronik-bench \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094 \
  --topic bench-test \
  --message-size 256 \
  --concurrency 128 \
  --duration 15s \
  --mode produce

# Expected (after fix):
# - Throughput >1,000 msg/s (not 10 req/s)
# - No leader election failures
# - All nodes show same ISR
# - No metadata inconsistencies
```

---

## Impact Assessment

### Before Fix

- ❌ Followers cannot handle auto-create (99% throughput loss)
- ❌ Continuous leader election failures
- ❌ Cluster metadata diverges
- ❌ Benchmark unusable in cluster mode

### After Fix

- ✅ Followers forward to leader (or return error)
- ✅ Clients retry with correct broker
- ✅ Topic creation goes through Raft consensus
- ✅ All nodes have consistent metadata
- ✅ No spurious leader election failures
- ✅ Full throughput restored

---

## Related Issues

1. [PARTITION_LEADERSHIP_FIX.md](./PARTITION_LEADERSHIP_FIX.md) - Fixed startup initialization (v2.2.1)
2. [CONFCHANGE_FIX_v2.2.7.md](./CONFCHANGE_FIX_v2.2.7.md) - Fixed ConfChange panic (v2.2.7)
3. [DEADLOCK_FIX_v2.2.7.md](./DEADLOCK_FIX_v2.2.7.md) - Fixed Raft message loop deadlock (v2.2.7)

This bug is **orthogonal** to those fixes - it's a separate issue with runtime topic creation.

---

## Priority

**CRITICAL** - Blocks all cluster testing and production use.

Without this fix:
- Benchmark cannot run (clients connect to random broker)
- Production deployments fail (clients connect to load balancers → random broker)
- Only single-node mode works

---

## Code Locations

- Bug location: [produce_handler.rs:2095-2105](../crates/chronik-server/src/produce_handler.rs#L2095-L2105)
- Related check: [produce_handler.rs:2374](../crates/chronik-server/src/produce_handler.rs#L2374)
- Leader election: [leader_election.rs:107-114](../crates/chronik-server/src/leader_election.rs#L107-L114)

---

**Next Step:** Implement Option 1 (forward to leader) in v2.2.7

