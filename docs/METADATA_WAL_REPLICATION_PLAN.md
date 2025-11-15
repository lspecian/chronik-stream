# Metadata WAL Replication - Complete Implementation Plan

## Overview

This document provides a step-by-step plan to implement **Event-based Metadata WAL Replication** for the acks=all fix. This is a multi-session project that will enable followers to receive partition assignments and ISR updates via WAL streaming, eliminating the acks=all timeout issue.

## Architecture: Event-Based Metadata Replication

```
┌─────────────────────────────────────────────────────────────────┐
│                 Event-Based Metadata Flow                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  1. ProduceHandler calls metadata_store.assign_partition()       │
│     ↓                                                             │
│  2. RaftMetadataStore writes to local Raft state machine         │
│     ↓                                                             │
│  3. RaftMetadataStore emits MetadataEvent::AssignPartition       │
│     ↓                                                             │
│  4. MetadataWalReplicator (subscriber) receives event            │
│     ↓                                                             │
│  5. MetadataWalReplicator writes to metadata WAL                 │
│     ↓                                                             │
│  6. MetadataWalReplicator replicates to followers (async)        │
│     ↓                                                             │
│  7. Followers receive WAL entry → apply to local state machine   │
│     ↓                                                             │
│  8. Followers now have partition assignments → discovery works!  │
│                                                                   │
└─────────────────────────────────────────────────────────────────┘
```

**Key Benefits:**
- ✅ No downcasting or trait object issues
- ✅ Clean separation of concerns
- ✅ Metadata store remains pure (no replication logic)
- ✅ Replication happens automatically via events
- ✅ Easy to test (mock event subscribers)
- ✅ Future-proof (can add more subscribers easily)

---

## Phase 1: Event Infrastructure (Session 1)

### Goal
Create the event system for metadata changes.

### Tasks

#### 1.1 Define MetadataEvent Enum

**File**: `crates/chronik-server/src/metadata_events.rs` (NEW FILE)

```rust
//! Metadata event system for WAL replication
//!
//! This module provides an event-based architecture for metadata changes.
//! When metadata is modified (partition assigned, ISR updated, etc.),
//! events are emitted that can be processed by subscribers like
//! MetadataWalReplicator.

use std::sync::Arc;
use tokio::sync::broadcast;
use serde::{Serialize, Deserialize};

/// Events emitted when metadata changes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataEvent {
    /// Partition replicas were assigned
    AssignPartition {
        topic: String,
        partition: i32,
        replicas: Vec<u64>,
    },

    /// Partition leader was set
    SetPartitionLeader {
        topic: String,
        partition: i32,
        leader: u64,
    },

    /// ISR was updated for a partition
    UpdateISR {
        topic: String,
        partition: i32,
        isr: Vec<u64>,
    },

    /// Topic was created
    CreateTopic {
        name: String,
        partition_count: u32,
        replication_factor: u32,
    },

    /// Topic was deleted
    DeleteTopic {
        name: String,
    },
}

/// Channel for broadcasting metadata events
#[derive(Clone)]
pub struct MetadataEventBus {
    sender: broadcast::Sender<MetadataEvent>,
}

impl MetadataEventBus {
    /// Create a new event bus with buffer capacity
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Emit a metadata event
    pub fn emit(&self, event: MetadataEvent) {
        // Fire-and-forget - don't block if no subscribers
        let _ = self.sender.send(event);
    }

    /// Subscribe to metadata events
    pub fn subscribe(&self) -> broadcast::Receiver<MetadataEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_bus() {
        let bus = MetadataEventBus::new(100);
        let mut rx = bus.subscribe();

        bus.emit(MetadataEvent::AssignPartition {
            topic: "test".to_string(),
            partition: 0,
            replicas: vec![1, 2, 3],
        });

        let event = rx.recv().await.unwrap();
        match event {
            MetadataEvent::AssignPartition { topic, .. } => {
                assert_eq!(topic, "test");
            }
            _ => panic!("Wrong event type"),
        }
    }
}
```

**Estimated Time**: 30 minutes

---

#### 1.2 Add Event Bus to RaftMetadataStore

**File**: `crates/chronik-server/src/raft_metadata_store.rs`

**Changes**:

1. Add field to struct (around line 60):
```rust
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
    node_id: u64,
    metadata_wal: Arc<MetadataWal>,
    metadata_wal_replicator: Arc<MetadataWalReplicator>,
    lease_manager: Arc<crate::leader_lease::LeaseManager>,
    event_bus: Arc<MetadataEventBus>,  // ← ADD THIS
}
```

2. Update constructor (around line 65-115):
```rust
impl RaftMetadataStore {
    pub fn new(
        raft: Arc<RaftCluster>,
        node_id: u64,
        metadata_wal: Arc<MetadataWal>,
        metadata_wal_replicator: Arc<MetadataWalReplicator>,
        lease_manager: Arc<crate::leader_lease::LeaseManager>,
        event_bus: Arc<MetadataEventBus>,  // ← ADD THIS PARAMETER
    ) -> Self {
        Self {
            raft,
            node_id,
            metadata_wal,
            metadata_wal_replicator,
            lease_manager,
            event_bus,  // ← STORE IT
        }
    }
}
```

3. Emit events after metadata operations (add to trait methods):

**In `assign_partition()` method** (around line 220):
```rust
async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
    // ... existing code ...

    // NEW: Emit event after successful assignment
    if assignment.is_leader {
        self.event_bus.emit(MetadataEvent::AssignPartition {
            topic: assignment.topic.clone(),
            partition: assignment.partition as i32,
            replicas: vec![assignment.broker_id as u64],  // TODO: Get full replica list
        });
    }

    Ok(())
}
```

**After SetPartitionLeader command** (in apply_metadata_command_direct, around line 250):
```rust
MetadataCommand::SetPartitionLeader { topic, partition, leader } => {
    self.raft.apply_metadata_command_direct(cmd)?;

    // NEW: Emit event
    self.event_bus.emit(MetadataEvent::SetPartitionLeader {
        topic: topic.clone(),
        partition,
        leader,
    });

    Ok(vec![])
}
```

**After UpdateISR command** (around line 260):
```rust
MetadataCommand::UpdateISR { topic, partition, isr } => {
    self.raft.apply_metadata_command_direct(cmd)?;

    // NEW: Emit event
    self.event_bus.emit(MetadataEvent::UpdateISR {
        topic: topic.clone(),
        partition,
        isr: isr.clone(),
    });

    Ok(vec![])
}
```

**Estimated Time**: 45 minutes

---

## Phase 2: Metadata WAL Replicator Subscriber (Session 2)

### Goal
Make MetadataWalReplicator subscribe to events and replicate commands.

### Tasks

#### 2.1 Add Event Subscriber to MetadataWalReplicator

**File**: `crates/chronik-server/src/metadata_wal_replication.rs`

**Changes**:

1. Add event bus field (around line 40):
```rust
pub struct MetadataWalReplicator {
    metadata_wal: Arc<MetadataWal>,
    replication_mgr: Arc<WalReplicationManager>,
    event_bus: Arc<MetadataEventBus>,  // ← ADD THIS
}
```

2. Update constructor (around line 55):
```rust
impl MetadataWalReplicator {
    pub fn new(
        metadata_wal: Arc<MetadataWal>,
        replication_mgr: Arc<WalReplicationManager>,
        event_bus: Arc<MetadataEventBus>,  // ← ADD THIS PARAMETER
    ) -> Self {
        Self {
            metadata_wal,
            replication_mgr,
            event_bus,  // ← STORE IT
        }
    }
}
```

3. Add event listener loop (NEW METHOD):
```rust
impl MetadataWalReplicator {
    /// Start listening for metadata events and replicate them
    pub fn start_event_listener(self: Arc<Self>) {
        let mut rx = self.event_bus.subscribe();

        tokio::spawn(async move {
            tracing::info!("MetadataWalReplicator event listener started");

            while let Ok(event) = rx.recv().await {
                if let Err(e) = self.handle_event(event).await {
                    tracing::warn!("Failed to handle metadata event: {}", e);
                }
            }

            tracing::warn!("MetadataWalReplicator event listener stopped");
        });
    }

    /// Handle a metadata event by replicating it
    async fn handle_event(&self, event: MetadataEvent) -> Result<()> {
        match event {
            MetadataEvent::AssignPartition { topic, partition, replicas } => {
                tracing::info!("📡 Replicating AssignPartition: {}-{} => {:?}",
                    topic, partition, replicas);

                let cmd = crate::raft_metadata::MetadataCommand::AssignPartition {
                    topic,
                    partition,
                    replicas,
                };

                self.replicate_command(cmd).await?;
            }

            MetadataEvent::SetPartitionLeader { topic, partition, leader } => {
                tracing::info!("📡 Replicating SetPartitionLeader: {}-{} => {}",
                    topic, partition, leader);

                let cmd = crate::raft_metadata::MetadataCommand::SetPartitionLeader {
                    topic,
                    partition,
                    leader,
                };

                self.replicate_command(cmd).await?;
            }

            MetadataEvent::UpdateISR { topic, partition, isr } => {
                tracing::info!("📡 Replicating UpdateISR: {}-{} => {:?}",
                    topic, partition, isr);

                let cmd = crate::raft_metadata::MetadataCommand::UpdateISR {
                    topic,
                    partition,
                    isr,
                };

                self.replicate_command(cmd).await?;
            }

            _ => {
                // Other events don't need replication yet
                tracing::debug!("Ignoring metadata event: {:?}", event);
            }
        }

        Ok(())
    }

    /// Replicate a metadata command to followers
    async fn replicate_command(&self, cmd: crate::raft_metadata::MetadataCommand) -> Result<()> {
        // 1. Write to metadata WAL (for local persistence)
        let offset = self.metadata_wal.append(&cmd).await
            .context("Failed to append metadata command to WAL")?;

        // 2. Replicate to followers via existing infrastructure
        self.replicate(&cmd, offset).await?;

        Ok(())
    }
}
```

**Estimated Time**: 1 hour

---

## Phase 3: Integration with Server Startup (Session 3)

### Goal
Wire up the event bus during server initialization.

### Tasks

#### 3.1 Create and Pass Event Bus in IntegratedServer

**File**: `crates/chronik-server/src/integrated_server.rs`

**Changes** (around line 200-300 where RaftMetadataStore is created):

1. Create event bus:
```rust
// Create metadata event bus for WAL replication
let metadata_event_bus = Arc::new(MetadataEventBus::new(1000));  // Buffer 1000 events
```

2. Pass to RaftMetadataStore constructor:
```rust
let metadata_store = Arc::new(RaftMetadataStore::new(
    raft_cluster.clone(),
    config.node_id,
    metadata_wal.clone(),
    metadata_wal_replicator.clone(),
    lease_manager.clone(),
    metadata_event_bus.clone(),  // ← ADD THIS
));
```

3. Pass to MetadataWalReplicator constructor:
```rust
let metadata_wal_replicator = Arc::new(MetadataWalReplicator::new(
    metadata_wal.clone(),
    wal_replication_manager.clone(),
    metadata_event_bus.clone(),  // ← ADD THIS
));
```

4. Start event listener:
```rust
// Start metadata event replication
metadata_wal_replicator.clone().start_event_listener();
info!("✓ Metadata WAL replication event listener started");
```

**Estimated Time**: 45 minutes

---

## Phase 4: Testing and Validation (Session 4)

### Goal
Verify the complete flow works end-to-end.

### Tasks

#### 4.1 Add Debug Logging

Add comprehensive logging to track events:

1. In `RaftMetadataStore::assign_partition()`:
```rust
tracing::info!("📤 EMIT: AssignPartition event for {}-{}", topic, partition);
```

2. In `MetadataWalReplicator::handle_event()`:
```rust
tracing::info!("📥 RECV: Metadata event: {:?}", event);
```

3. In `MetadataWalReplicator::replicate_command()`:
```rust
tracing::info!("📡 REPL: Replicating metadata command, offset={}", offset);
```

#### 4.2 Test with Cluster

**Commands**:
```bash
# Build
cargo build --release --bin chronik-server

# Start cluster
cd tests/cluster
./stop.sh && ./start.sh

# Monitor logs for events
tail -f logs/node1.log | grep -E "(EMIT|RECV|REPL|📡|📤|📥)"

# Test acks=all
python3 test_acks_all_fix.py
```

**Expected Log Flow**:
```
[Node 1] 📤 EMIT: AssignPartition event for acks-all-test-0
[Node 1] 📥 RECV: Metadata event: AssignPartition { topic: "acks-all-test", partition: 0, replicas: [1, 2, 3] }
[Node 1] 📡 REPL: Replicating metadata command, offset=123
[Node 1] Sent metadata WAL entry to follower localhost:9292
[Node 1] Sent metadata WAL entry to follower localhost:9293
[Node 2] Received metadata WAL entry for acks-all-test-0
[Node 2] Applied metadata command: AssignPartition
[Node 3] Received metadata WAL entry for acks-all-test-0
[Node 3] Applied metadata command: AssignPartition
[Node 1] Updated followers for acks-all-test-0: ["localhost:9292", "localhost:9293"]
```

#### 4.3 Verify acks=all Performance

**Test Script**: `tests/cluster/test_acks_all_fix.py`

**Expected Result**:
```
============================================================
Testing acks=all fix for partition leader assignment
============================================================

1. Creating producer with acks=all...
   ✓ Producer created

2. Sending test message with acks=all...
   ✓ Message sent successfully in 0.523s
   ✓ Topic: acks-all-test, Partition: 0, Offset: 0

============================================================
✅ SUCCESS: acks=all completed in 0.523s (< 2s)
============================================================

The fix is working correctly!
Partition leaders are now assigned during topic creation.
WAL replication can discover followers and send ACKs.
```

**Estimated Time**: 1.5 hours

---

## Phase 5: Cleanup and Documentation (Session 5)

### Goal
Remove old code and document the new architecture.

### Tasks

#### 5.1 Remove Unused Helper Methods

**File**: `crates/chronik-server/src/raft_metadata_store.rs`

Delete the `assign_partition_replicas()` and `update_isr_replicated()` helper methods (lines 1128-1223) since they're no longer needed with the event-based approach.

#### 5.2 Update Documentation

**Files to Update**:

1. `docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md`
   - Add section: "Final Solution: Event-Based Metadata Replication"
   - Explain why this approach was chosen
   - Document the event flow

2. `docs/METADATA_WAL_REPLICATION_PLAN.md` (this file)
   - Mark all phases as COMPLETED
   - Add "Lessons Learned" section
   - Add troubleshooting guide

3. `CLAUDE.md`
   - Update architecture section with metadata event system
   - Document MetadataEventBus usage

#### 5.3 Add Architecture Diagram

Create `docs/metadata_replication_architecture.md`:

```markdown
# Metadata WAL Replication Architecture

## Event Flow

┌─────────────────────────────────────────────────────────────────┐
│                     Metadata Change Flow                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  ┌──────────────┐                                                │
│  │ ProduceHandler│                                               │
│  └──────┬────────┘                                               │
│         │ assign_partition()                                     │
│         ↓                                                         │
│  ┌──────────────────┐                                            │
│  │RaftMetadataStore │                                            │
│  └──────┬───────────┘                                            │
│         │ 1. Apply to Raft state                                │
│         │ 2. Emit MetadataEvent                                  │
│         ↓                                                         │
│  ┌──────────────────┐                                            │
│  │MetadataEventBus  │ (broadcast channel)                        │
│  └──────┬───────────┘                                            │
│         │ broadcast to all subscribers                           │
│         ↓                                                         │
│  ┌──────────────────────┐                                        │
│  │MetadataWalReplicator │ (subscriber)                           │
│  └──────┬───────────────┘                                        │
│         │ 1. Write to metadata WAL                               │
│         │ 2. Replicate to followers                              │
│         ↓                                                         │
│  ┌──────────────────────┐                                        │
│  │WalReplicationManager │                                        │
│  └──────┬───────────────┘                                        │
│         │ Send via TCP to followers                              │
│         ↓                                                         │
│  ┌──────────────────┐   ┌──────────────────┐                    │
│  │   Node 2         │   │   Node 3         │                    │
│  │   (Follower)     │   │   (Follower)     │                    │
│  └──────────────────┘   └──────────────────┘                    │
│                                                                   │
└─────────────────────────────────────────────────────────────────┘
```

**Estimated Time**: 1 hour

---

## Summary: Complete File Change List

### New Files to Create

1. ✅ `crates/chronik-server/src/metadata_events.rs`
   - MetadataEvent enum
   - MetadataEventBus implementation
   - Tests

2. ✅ `docs/metadata_replication_architecture.md`
   - Architecture diagrams
   - Event flow documentation
   - Troubleshooting guide

### Files to Modify

1. ✅ `crates/chronik-server/src/raft_metadata_store.rs`
   - Add `event_bus: Arc<MetadataEventBus>` field
   - Update constructor to accept event_bus
   - Emit events in assign_partition(), after SetPartitionLeader, after UpdateISR
   - Delete old helper methods (lines 1128-1223)

2. ✅ `crates/chronik-server/src/metadata_wal_replication.rs`
   - Add `event_bus: Arc<MetadataEventBus>` field
   - Update constructor to accept event_bus
   - Add `start_event_listener()` method
   - Add `handle_event()` method
   - Add `replicate_command()` method

3. ✅ `crates/chronik-server/src/integrated_server.rs`
   - Create MetadataEventBus instance
   - Pass to RaftMetadataStore constructor
   - Pass to MetadataWalReplicator constructor
   - Call `start_event_listener()` after initialization

4. ✅ `crates/chronik-server/src/lib.rs` or `mod.rs`
   - Add `pub mod metadata_events;` declaration

5. ✅ `docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md`
   - Add "Final Solution" section
   - Document event-based approach

6. ✅ `CLAUDE.md`
   - Update architecture section
   - Document metadata event system

---

## Session Checklist

Use this checklist to track progress across sessions:

### Session 1: Event Infrastructure
- [ ] Create `metadata_events.rs` with MetadataEvent enum
- [ ] Create MetadataEventBus implementation
- [ ] Add tests for event bus
- [ ] Add event_bus field to RaftMetadataStore
- [ ] Update RaftMetadataStore constructor
- [ ] Emit events in metadata operations
- [ ] Build and verify compilation

### Session 2: Replicator Subscriber
- [ ] Add event_bus field to MetadataWalReplicator
- [ ] Update MetadataWalReplicator constructor
- [ ] Implement start_event_listener()
- [ ] Implement handle_event()
- [ ] Implement replicate_command()
- [ ] Build and verify compilation

### Session 3: Server Integration
- [ ] Create MetadataEventBus in IntegratedServer
- [ ] Pass event_bus to RaftMetadataStore
- [ ] Pass event_bus to MetadataWalReplicator
- [ ] Start event listener
- [ ] Build and verify compilation
- [ ] Start cluster and check logs

### Session 4: Testing
- [ ] Add debug logging to track events
- [ ] Test acks=all with cluster
- [ ] Verify event flow in logs
- [ ] Verify followers receive metadata
- [ ] Verify acks=all completes in < 1s
- [ ] Run performance benchmarks

### Session 5: Cleanup
- [ ] Remove old helper methods
- [ ] Update documentation
- [ ] Create architecture diagrams
- [ ] Add troubleshooting guide
- [ ] Final testing
- [ ] Commit and push changes

---

## Estimated Total Time

- Session 1: 1.5 hours
- Session 2: 1 hour
- Session 3: 1 hour
- Session 4: 2 hours
- Session 5: 1.5 hours

**Total**: ~7 hours across 5 sessions

---

## Success Criteria

The implementation is complete when:

1. ✅ acks=all completes in < 1 second (not 30 seconds)
2. ✅ Followers receive partition assignments via WAL
3. ✅ Followers receive ISR updates via WAL
4. ✅ Discovery worker finds followers immediately (< 100ms)
5. ✅ WAL replication sends ACKs back to leader
6. ✅ No deadlocks or blocking in request handlers
7. ✅ All tests pass
8. ✅ Documentation is complete

---

## Rollback Plan

If something goes wrong, revert by:

1. Remove `metadata_events.rs`
2. Remove event_bus fields from RaftMetadataStore and MetadataWalReplicator
3. Remove start_event_listener() call from IntegratedServer
4. Restore old helper methods in RaftMetadataStore (from git history)
5. Rebuild and test

Git commands:
```bash
git checkout HEAD -- crates/chronik-server/src/raft_metadata_store.rs
git checkout HEAD -- crates/chronik-server/src/metadata_wal_replication.rs
git checkout HEAD -- crates/chronik-server/src/integrated_server.rs
git clean -fd  # Remove new files
cargo build --release
```

---

## Contact and Support

If stuck during implementation:
1. Check logs for event flow: `tail -f logs/node1.log | grep -E "(📤|📥|📡)"`
2. Review [docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md](docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md)
3. Review this plan and session checklist
4. Check git history for reference: `git log --oneline --graph`

---

## Notes for Future Sessions

### Session Continuity Prompt

```
I'm continuing work on the metadata WAL replication fix for acks=all timeout.

Current status: [Session X completed / Session Y in progress]

Please review:
1. docs/METADATA_WAL_REPLICATION_PLAN.md - Complete implementation plan
2. docs/ACKS_ALL_DEADLOCK_STATUS.md - Current status and findings
3. docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md - Root cause analysis

Next steps: Follow Session [Y] checklist in METADATA_WAL_REPLICATION_PLAN.md

Working directory: /home/ubuntu/Development/chronik-stream
```

---

**Last Updated**: 2025-11-13
**Status**: Ready to implement
**Estimated Completion**: 5 sessions (~7 hours total)
