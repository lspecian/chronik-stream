# Snapshot Support - Ready to Implement

## Status: Analysis Complete ✅

All infrastructure for Raft snapshot support **already exists** in the codebase:

### ✅ Existing Infrastructure
1. **SnapshotManager** - Complete implementation with tests (`crates/chronik-raft/src/snapshot.rs`)
2. **StateMachine trait** - Defines snapshot() and restore() (`crates/chronik-raft/src/state_machine.rs`)
3. **MetadataStateMachine** - Fully implements snapshot/restore (`crates/chronik-common/src/metadata/raft_state_machine.rs`)
4. **tikv/raft integration** - Ready struct has snapshot field

### ❌ Missing: Wiring in PartitionReplica

The ONLY missing piece is connecting tikv/raft's snapshot mechanism to our state machine in `PartitionReplica::ready()`.

## Implementation Required

### File: `crates/chronik-raft/src/replica.rs`

**Location 1: After line 566 in ready() method**

Add snapshot checking:

```rust
// Check for snapshot to install (BEFORE persisting anything)
let snapshot_to_install = if !ready.snapshot().is_empty() {
    let snap_meta = ready.snapshot().get_metadata();
    info!(
        "Received snapshot for {}-{}: index={}, term={}",
        self.topic,
        self.partition,
        snap_meta.get_index(),
        snap_meta.get_term()
    );
    Some(ready.snapshot().clone())
} else {
    None
};
```

**Location 2: After line 687 (after dropping node lock, before persisting entries)**

Add snapshot installation:

```rust
// Step 1.5: Install snapshot if present (BEFORE applying committed entries)
if let Some(snapshot) = snapshot_to_install {
    match self.install_snapshot(&snapshot).await {
        Ok(_) => {
            info!("Successfully installed snapshot for {}-{}", self.topic, self.partition);
        }
        Err(e) => {
            error!("Failed to install snapshot for {}-{}: {}", self.topic, self.partition, e);
            // Report failure to Raft
            let mut node = self.raw_node.write();
            use raft::SnapshotStatus;
            node.report_snapshot(self.config.node_id, SnapshotStatus::Failure);
        }
    }
}
```

**Location 3: Add new method to PartitionReplica impl block**

```rust
/// Install a snapshot received from the leader
async fn install_snapshot(&self, snapshot: &raft::prelude::Snapshot) -> Result<()> {
    use crate::state_machine::SnapshotData;
    use raft::SnapshotStatus;

    let metadata = snapshot.get_metadata();
    let last_index = metadata.get_index();
    let last_term = metadata.get_term();

    info!(
        "Installing snapshot for {}-{}: index={}, term={}, size={} bytes",
        self.topic, self.partition, last_index, last_term, snapshot.get_data().len()
    );

    // Convert tikv/raft Snapshot to our SnapshotData
    let snapshot_data = SnapshotData {
        last_index,
        last_term,
        conf_state: metadata.get_conf_state().get_voters().to_vec(),
        data: snapshot.get_data().to_vec(),
    };

    // Restore state machine from snapshot
    if let Some(state_machine) = &self.state_machine {
        let mut sm = state_machine.write().await;
        sm.restore(&snapshot_data).await.map_err(|e| {
            RaftError::StorageError(format!("Failed to restore snapshot: {}", e))
        })?;

        info!(
            "State machine restored from snapshot at index {}, last_applied={}",
            last_index,
            sm.last_applied()
        );
    }

    // Update applied index
    self.applied_index.store(last_index, Ordering::SeqCst);

    // Report success to Raft
    {
        let mut node = self.raw_node.write();
        node.report_snapshot(self.config.node_id, SnapshotStatus::Finish);
    }

    info!(
        "Snapshot installed successfully for {}-{} at index {}",
        self.topic, self.partition, last_index
    );

    Ok(())
}

/// Check if snapshot should be created based on log size
pub fn should_create_snapshot(&self) -> bool {
    let node = self.raw_node.read();
    let last_index = node.raft.raft_log.last_index();
    let applied = self.applied_index.load(Ordering::SeqCst);

    // Create snapshot if unapplied entries exceed threshold
    let unapplied = last_index.saturating_sub(applied);
    unapplied >= self.config.snapshot_threshold
}

/// Create and store a snapshot
pub async fn create_snapshot(&self) -> Result<()> {
    use crate::state_machine::SnapshotData;

    let applied = self.applied_index.load(Ordering::SeqCst);
    let term = self.term();

    info!(
        "Creating snapshot for {}-{} at index={}, term={}",
        self.topic, self.partition, applied, term
    );

    // Create snapshot from state machine
    let snapshot_data = if let Some(state_machine) = &self.state_machine {
        let sm = state_machine.read().await;
        sm.snapshot(applied, term).await.map_err(|e| {
            RaftError::StorageError(format!("Failed to create snapshot: {}", e))
        })?
    } else {
        return Err(RaftError::Config("No state machine configured".to_string()));
    };

    info!(
        "Created snapshot: index={}, term={}, size={} bytes",
        snapshot_data.last_index,
        snapshot_data.last_term,
        snapshot_data.data.len()
    );

    // Convert to tikv/raft Snapshot format
    let mut raft_snapshot = raft::prelude::Snapshot::default();
    let mut metadata = raft::prelude::SnapshotMetadata::default();
    metadata.set_index(snapshot_data.last_index);
    metadata.set_term(snapshot_data.last_term);

    // Set conf_state (cluster configuration)
    let mut conf_state = raft::prelude::ConfState::default();
    conf_state.set_voters(snapshot_data.conf_state.clone());
    metadata.set_conf_state(conf_state);

    raft_snapshot.set_metadata(metadata);
    raft_snapshot.set_data(snapshot_data.data);

    // Store snapshot in Raft storage (MemStorage)
    {
        let storage = self.raw_node.read().mut_store();
        storage.wl().apply_snapshot(raft_snapshot).map_err(|e| {
            RaftError::StorageError(format!("Failed to apply snapshot to storage: {}", e))
        })?;
    }

    info!(
        "Snapshot created and stored for {}-{} at index {}",
        self.topic, self.partition, snapshot_data.last_index
    );

    Ok(())
}
```

## Build & Test Commands

```bash
# Build with raft feature
cargo build --release --bin chronik-server --features raft

# Run Phase 3 test (should now pass without panic)
python3 test_raft_phase3_cluster_lifecycle.py

# Expected: Node rejoin no longer panics with "to_commit X is out of range"
```

## Estimated Time

- **Code changes**: 1-2 hours (straightforward additions)
- **Build & test**: 30 minutes
- **Verification**: 1 hour
- **Total**: 2-4 hours

## Success Criteria

✅ Phase 3 test passes completely without node rejoin panic
✅ Nodes can rejoin after missing >100 commits
✅ Snapshot is sent automatically when follower lags
✅ State machine restored correctly from snapshot
✅ No "out of range" errors in logs

## Why This Will Work

1. **MetadataStateMachine.snapshot()** already serializes full metadata state
2. **MetadataStateMachine.restore()** already deserializes and replaces state
3. **tikv/raft** handles all the snapshot sending/receiving logic
4. We just need to wire up the `ready.snapshot()` → `install_snapshot()` path

The heavy lifting is already done. This is literally just wiring existing pieces together.

## Next Action

When you're ready to implement:

1. Open `crates/chronik-raft/src/replica.rs`
2. Add the 3 code blocks above at the specified locations
3. Add imports at top of file:
   ```rust
   use raft::{prelude::Snapshot, SnapshotStatus};
   ```
4. Build and test

That's it. Snapshot support will be complete.
