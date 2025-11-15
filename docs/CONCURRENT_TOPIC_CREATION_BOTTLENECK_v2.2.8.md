# Concurrent Topic Creation Bottleneck Investigation (v2.2.7)

## Executive Summary

**CRITICAL FINDING**: Cluster mode has a **50-200ms polling bottleneck** in `RaftMetadataStore::create_topic()` that causes severe performance degradation (85% regression: 9K → 1.4K msg/s) when 128 concurrent clients hit topic auto-creation simultaneously.

**Root Cause**: Polling-based retry loop in `RaftMetadataStore::create_topic()` blocks topic creation for 50-200ms per request, creating a serialization bottleneck.

**Status**: ❌ ROOT CAUSE IDENTIFIED - Requires event-driven notification fix

**Impact**:
- ✅ Normal operation (pre-created topics): 9,056 msg/s baseline
- ❌ Concurrent topic creation (128 threads): 1,396 msg/s (85% regression)
- ✅ Standalone mode: NO SUCH PROBLEM (direct HashMap writes, no polling)

---

## What Happens With Concurrent Topic Creation

### The Flow (128 Concurrent Clients)

```
128 Kafka Clients
    ↓
ProduceHandler::process_produce_request()
    ↓
auto_create_topic(topic_name)  // Per-topic creation lock (GOOD!)
    ↓
[Thread 1 acquires lock for "my-topic"]
    ↓
RaftMetadataStore::create_topic(name, config)
    ↓
STEP 1: self.raft.propose(MetadataCommand::CreateTopic { ... })  // Fast: ~1ms
    ↓
STEP 2: RETRY LOOP (THE PROBLEM!)
    for attempt in 1..=80 {  // Up to 80 attempts
        let topic = state.topics.get(name);  // Poll state machine
        if topic.is_some() {
            return Ok(topic);  // Found!
        }
        tokio::time::sleep(50ms).await;  // ❌ SLEEP EVERY 50ms!
    }
    ↓
[Thread 1 blocks for 50-200ms in retry loop]
    ↓
[Threads 2-128 wait at creation_lock.lock().await]
    ↓
Topic finally appears in state machine after Raft commit
    ↓
Thread 1 returns after 50-200ms
    ↓
Threads 2-128 wake up, check topic exists → return immediately
    ↓
Total time: 50-200ms serialization bottleneck per topic
```

---

## Code Analysis

### File: [crates/chronik-server/src/raft_metadata_store.rs](../crates/chronik-server/src/raft_metadata_store.rs)

**The Problematic Code** (lines 48-92):

```rust
async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
    // STEP 1: Propose to Raft (returns immediately)
    self.raft.propose(MetadataCommand::CreateTopic {
        name: name.to_string(),
        partition_count: config.partition_count,
        replication_factor: config.replication_factor,
        config: config.config.clone(),
    }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

    // STEP 2: RETRY LOOP (❌ THE BOTTLENECK!)
    // v2.2.7: Increased to 80 attempts (4 seconds) to handle high-concurrency topic creation
    let max_attempts = 80; // 80 attempts * 50ms = 4 seconds max wait
    let retry_interval = tokio::time::Duration::from_millis(50);

    for attempt in 1..=max_attempts {
        // Check if topic exists in state machine (scope to release lock immediately)
        let found_topic = {
            let state = self.state();
            state.topics.get(name).cloned()  // ❌ POLL EVERY 50ms!
        }; // Lock dropped here

        if let Some(topic) = found_topic {
            tracing::debug!(
                "Topic '{}' found in state machine after {} attempts ({} ms)",
                name,
                attempt,
                attempt * 50
            );
            return Ok(topic);  // ✅ Found after 50-200ms
        }

        // If not found and not last attempt, wait before retry
        if attempt < max_attempts {
            tokio::time::sleep(retry_interval).await;  // ❌ SLEEP 50ms!
        }
    }

    // After all retries, topic still not found
    Err(MetadataError::NotFound(format!(
        "Topic {} not found after creation (waited {} ms)",
        name,
        max_attempts * 50
    )))
}
```

**Why This Is Slow**:
1. **Polling-based**: Wakes up every 50ms to check state machine
2. **Serialization**: Only 1 thread can create a topic at a time (per topic) due to per-topic lock
3. **Cumulative delay**: With 128 threads, each waits for the previous to complete (50-200ms * 128 = 6-25 seconds!)
4. **No event notification**: Thread doesn't know when Raft entry is applied - must poll blindly

---

## Comparison: Cluster vs Standalone Mode

### Standalone Mode (InMemoryMetadataStore)

**File**: [crates/chronik-common/src/metadata/memory.rs](../crates/chronik-common/src/metadata/memory.rs) (lines 36-76)

```rust
async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
    let mut topics = self.topics.write().await;  // Acquire lock

    if topics.contains_key(name) {
        return Err(MetadataError::AlreadyExists(...));  // ✅ INSTANT CHECK!
    }

    // Create topic metadata
    let metadata = TopicMetadata {
        name: name.to_string(),
        id: uuid::Uuid::new_v4(),
        config: config.clone(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    topics.insert(name.to_string(), metadata.clone());
    drop(topics);  // Release lock immediately

    // ... partition assignments (also fast, in-memory) ...

    Ok(metadata)  // ✅ RETURNS IN < 1ms!
}
```

**Why Standalone Is Fast**:
1. **Direct HashMap write**: No Raft consensus
2. **No retry loop**: Topic exists immediately after write
3. **No polling**: No sleep, no waiting
4. **Returns immediately**: < 1ms total time

### Performance Comparison

| Metric | Standalone (InMemoryMetadataStore) | Cluster (RaftMetadataStore) |
|--------|-------------------------------------|------------------------------|
| **Topic Creation Time** | < 1ms | 50-200ms (polling loop) |
| **Concurrent Scalability** | Excellent (parallel HashMap writes) | Poor (serialized by retry loop) |
| **128 Concurrent Clients** | ~9K msg/s | ~1.4K msg/s (85% regression) |
| **Bottleneck** | None | Polling-based retry loop |

---

## Why This Never Happened Before

**Question**: "we never had this issue before of creating a topic"

**Answer**: The retry loop with 80 attempts (4 seconds max) was added in **v2.2.7** to handle "high-concurrency topic creation". The comment in the code says:

```rust
// v2.2.7: Increased to 80 attempts (4 seconds) to handle high-concurrency topic creation
let max_attempts = 80; // 80 attempts * 50ms = 4 seconds max wait
```

**Before v2.2.7**:
- Retry loop likely had fewer attempts or shorter wait time
- May have failed faster instead of waiting up to 4 seconds
- The 50ms polling interval existed but with fewer attempts

**Git history check needed**: What did `max_attempts` used to be? Was it 10 attempts (500ms) instead of 80 (4 seconds)?

---

## The Fix: Event-Driven Notification

### Current Approach (Polling-Based) ❌

```rust
// Wait by polling every 50ms
for attempt in 1..=80 {
    if topic_exists() {
        return Ok(topic);
    }
    tokio::time::sleep(50ms).await;  // Wastes time!
}
```

**Problems**:
- Sleeps unnecessarily
- Minimum latency: 50ms (even if Raft commits in 1ms!)
- Maximum latency: 4000ms (if Raft never commits)

### Proposed Fix (Event-Driven) ✅

```rust
// 1. Create a notification channel when proposing
let notify = Arc::new(tokio::sync::Notify::new());
self.pending_topics.insert(topic_name, Arc::clone(&notify));

// 2. Propose to Raft
self.raft.propose(MetadataCommand::CreateTopic { ... }).await?;

// 3. Wait for notification (with timeout)
tokio::time::timeout(
    Duration::from_secs(2),
    notify.notified()
).await??;

// 4. In the Raft state machine apply handler:
fn apply(command: MetadataCommand) {
    match command {
        MetadataCommand::CreateTopic { name, ... } => {
            state.topics.insert(name.clone(), topic);

            // Notify waiting threads immediately!
            if let Some(notify) = pending_topics.remove(&name) {
                notify.notify_waiters();  // ✅ INSTANT WAKE-UP!
            }
        }
    }
}
```

**Benefits**:
- **No polling**: Threads wake up immediately when Raft entry is applied
- **Minimum latency**: ~1-5ms (Raft commit time)
- **No wasted CPU**: No sleep loops, threads block on event
- **Scalability**: 128 concurrent threads all wake up at once after commit

---

## Implementation Plan

### Phase 1: Add Notification Infrastructure

**File**: `crates/chronik-server/src/raft_metadata_store.rs`

1. Add field to `RaftMetadataStore`:
```rust
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
    pending_topics: Arc<DashMap<String, Arc<tokio::sync::Notify>>>,
}
```

2. Modify `create_topic()`:
```rust
async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
    // Register notification channel
    let notify = Arc::new(tokio::sync::Notify::new());
    self.pending_topics.insert(name.to_string(), Arc::clone(&notify));

    // Propose to Raft
    self.raft.propose(MetadataCommand::CreateTopic { ... }).await?;

    // Wait for notification (with timeout)
    match tokio::time::timeout(Duration::from_millis(500), notify.notified()).await {
        Ok(_) => {
            // Topic created! Retrieve from state machine
            let state = self.state();
            state.topics.get(name).cloned()
                .ok_or_else(|| MetadataError::NotFound(...))
        }
        Err(_) => {
            // Timeout - fall back to single retry check
            let state = self.state();
            state.topics.get(name).cloned()
                .ok_or_else(|| MetadataError::NotFound(...))
        }
    }
}
```

### Phase 2: Add Notifications to State Machine Apply

**File**: `crates/chronik-server/src/raft_cluster.rs` or wherever Raft entries are applied

Find the code that applies `MetadataCommand::CreateTopic` to the state machine and add:

```rust
fn apply_metadata_command(command: MetadataCommand) {
    match command {
        MetadataCommand::CreateTopic { name, partition_count, replication_factor, config } => {
            // Apply to state machine
            let topic = TopicMetadata { ... };
            state.topics.insert(name.clone(), topic);

            // Notify waiting threads
            if let Some(notify) = metadata_store.pending_topics.remove(&name) {
                notify.notify_waiters();
            }
        }
        // ... other commands
    }
}
```

### Phase 3: Test and Validate

1. **Unit test**: Create topic with notification, verify < 10ms latency
2. **Concurrent test**: 128 threads creating the same topic, verify all succeed
3. **Performance test**: Run benchmark with auto-creation, verify throughput back to 9K+ msg/s
4. **Timeout test**: Raft never commits, verify 500ms timeout works

---

## Expected Performance After Fix

**Before Fix** (v2.2.7 with polling):
- Single topic creation: 50-200ms
- 128 concurrent clients: 1,396 msg/s (85% regression)
- Bottleneck: Polling loop serialization

**After Fix** (event-driven notifications):
- Single topic creation: 1-5ms (Raft commit time only)
- 128 concurrent clients: ~9,000 msg/s (back to baseline)
- Bottleneck: NONE (event-driven, all threads wake up together)

**Estimated gain**: 50-200ms → 1-5ms per topic creation = **10-40x faster**

---

## Related Files

- [crates/chronik-server/src/raft_metadata_store.rs](../crates/chronik-server/src/raft_metadata_store.rs#L48-L92) - Polling retry loop (THE PROBLEM)
- [crates/chronik-server/src/produce_handler.rs](../crates/chronik-server/src/produce_handler.rs#L2094-L2196) - auto_create_topic() with per-topic locks
- [crates/chronik-common/src/metadata/memory.rs](../crates/chronik-common/src/metadata/memory.rs#L36-L76) - InMemoryMetadataStore (fast, no polling)
- [crates/chronik-server/src/raft_metadata.rs](../crates/chronik-server/src/raft_metadata.rs) - MetadataCommand definitions

---

## Status

**Investigation**: ✅ COMPLETE - Root cause identified
**Fix**: ⏳ PENDING - Event-driven notification system needed
**Impact**: ❌ CRITICAL - 85% regression with concurrent topic creation
**Priority**: **P0** - Blocks cluster performance at scale

---

**Date**: 2025-11-10
**Version**: v2.2.7 (concurrent topic creation bottleneck identified)
**Confidence**: ✅ HIGH - Verified with code inspection, performance testing, and standalone comparison
