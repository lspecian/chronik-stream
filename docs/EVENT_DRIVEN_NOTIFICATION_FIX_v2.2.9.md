# Event-Driven Notification Fix (v2.2.9)

## Summary

**Problem**: Polling-based retry loop in `RaftMetadataStore` causes 50-200ms latency per topic creation, leading to 85% throughput regression (9K → 1.4K msg/s) with concurrent topic auto-creation.

**Solution**: Replace polling with event-driven notifications using `tokio::sync::Notify`. Raft state machine notifies waiting threads instantly when entries are applied.

**Expected Performance**: 10-40x faster topic creation (50-200ms → 1-5ms)

---

## Implementation Status

### ✅ Phase 1: RaftMetadataStore Notification Infrastructure (COMPLETE)

**File**: [crates/chronik-server/src/raft_metadata_store.rs](../crates/chronik-server/src/raft_metadata_store.rs)

**Changes Made**:
1. Added notification fields to struct:
```rust
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
    pending_topics: Arc<DashMap<String, Arc<Notify>>>,
    pending_brokers: Arc<DashMap<i32, Arc<Notify>>>,
}
```

2. Added notification methods:
```rust
pub fn notify_topic_created(&self, topic_name: &str) {
    if let Some((_, notify)) = self.pending_topics.remove(topic_name) {
        notify.notify_waiters(); // Wake up ALL waiting threads
    }
}

pub fn notify_broker_registered(&self, broker_id: i32) {
    if let Some((_, notify)) = self.pending_brokers.remove(&broker_id) {
        notify.notify_waiters();
    }
}
```

3. Replaced `create_topic()` polling loop with event-driven wait:
```rust
async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
    // Register notification channel BEFORE proposing
    let notify = Arc::new(Notify::new());
    self.pending_topics.insert(name.to_string(), Arc::clone(&notify));

    // Propose to Raft
    self.raft.propose(MetadataCommand::CreateTopic { ... }).await?;

    // Wait for notification (1-5ms) instead of polling (50ms intervals)
    match tokio::time::timeout(Duration::from_millis(2000), notify.notified()).await {
        Ok(_) => {
            // ✅ Instant wake-up when Raft applies entry!
            let state = self.state();
            state.topics.get(name).cloned().ok_or(...)
        }
        Err(_) => {
            // Timeout fallback (handles edge cases)
            let state = self.state();
            state.topics.get(name).cloned().ok_or(...)
        }
    }
}
```

**Benefits**:
- ✅ No more 50ms sleep loops
- ✅ Threads wake up INSTANTLY when Raft applies entry
- ✅ Minimal latency (1-5ms instead of 50-200ms)
- ✅ 2-second timeout for safety

### ⏳ Phase 2: RaftCluster Notification Calls (IN PROGRESS)

**File**: [crates/chronik-server/src/raft_cluster.rs](../crates/chronik-server/src/raft_cluster.rs)

**TODO**: Find where Raft applies MetadataCommand entries and add notification calls:

```rust
// In RaftCluster::apply_metadata_command() or similar
fn apply_metadata_command(&self, command: MetadataCommand, metadata_store: &RaftMetadataStore) {
    match command {
        MetadataCommand::CreateTopic { name, ... } => {
            // Apply to state machine
            let topic = TopicMetadata { ... };
            state.topics.insert(name.clone(), topic);

            // ✅ NEW: Notify waiting threads
            metadata_store.notify_topic_created(&name);
        }
        MetadataCommand::RegisterBroker { broker_id, ... } => {
            // Apply to state machine
            state.brokers.insert(broker_id, ...);

            // ✅ NEW: Notify waiting threads
            metadata_store.notify_broker_registered(broker_id);
        }
        // ... other commands
    }
}
```

**Challenge**: RaftCluster needs reference to RaftMetadataStore to call notification methods.

**Options**:
1. **Pass metadata_store reference** to apply method (need to check if already available)
2. **Share notification maps** - RaftCluster gets `Arc<DashMap>` references from RaftMetadataStore
3. **Callback pattern** - RaftCluster calls a registered callback when entries are applied

### ⏳ Phase 3: Testing (PENDING)

**Tests Needed**:
1. Single topic creation - verify < 10ms latency
2. 128 concurrent topic creations - verify no serialization bottleneck
3. Timeout edge case - Raft never commits, verify timeout works
4. Cluster mode - verify notifications work across nodes
5. Performance benchmark - verify 9K+ msg/s with concurrent auto-creation

---

## Next Steps

1. **Find metadata apply location** - Search RaftCluster for where MetadataCommand is applied to state machine
2. **Wire up notifications** - Add `metadata_store.notify_topic_created()` calls
3. **Build and test** - Verify compilation and basic functionality
4. **Performance test** - Run benchmark with 128 concurrent clients
5. **Verify improvement** - Should see 1.4K → 9K+ msg/s recovery

---

**Status**: Phase 1 complete, Phase 2 in progress
**Date**: 2025-11-10
**Version**: v2.2.9 (event-driven notifications)
