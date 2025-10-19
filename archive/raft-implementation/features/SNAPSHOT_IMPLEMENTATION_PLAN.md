# Snapshot Support Implementation Plan

## Current Status

### ✅ Already Implemented
1. **SnapshotManager** (`crates/chronik-raft/src/snapshot.rs`):
   - Complete snapshot creation, compression, upload to object store
   - Snapshot restoration and verification
   - Background snapshot loop
   - Cleanup of old snapshots
   - Full test coverage

2. **StateMachine Trait** (`crates/chronik-raft/src/state_machine.rs`):
   - `snapshot(last_index, last_term)` - Create snapshot
   - `restore(snapshot_data)` - Restore from snapshot
   - `last_applied()` - Track progress
   - MemoryStateMachine with full implementation

3. **MetadataStateMachine** (`crates/chronik-common/src/metadata/raft_state_machine.rs`):
   - ✅ `snapshot()` - Serializes MetadataState to bincode
   - ✅ `restore()` - Deserializes and replaces state
   - ✅ Full implementation ready to use

### ❌ Missing Integration (The Gap)

The missing piece is wiring up tikv/raft's snapshot mechanism in `PartitionReplica::ready()`:

1. **Snapshot Installation** (receiving snapshots):
   - Check if `ready.snapshot()` is non-empty
   - Call `state_machine.restore(snapshot_data)`
   - Report success/failure to Raft via `report_snapshot()`

2. **Snapshot Creation Triggering** (sending snapshots):
   - Detect when Raft needs a snapshot (log too long)
   - Call `state_machine.snapshot(last_index, last_term)`
   - Store snapshot in Raft's storage
   - Let Raft send it to lagging followers

3. **Configuration**:
   - Set `snapshot_threshold` in RaftConfig (currently 10,000)
   - Enable automatic snapshot creation

## Implementation Steps

### Step 1: Wire Up Snapshot Reception in ready()

Location: `crates/chronik-raft/src/replica.rs::ready()`

Add after extracting the Ready struct:

```rust
// Check for snapshot to install
let snapshot_to_install = if !ready.snapshot().is_empty() {
    info!(
        "Received snapshot for {}-{}: index={}, term={}",
        self.topic,
        self.partition,
        ready.snapshot().get_metadata().get_index(),
        ready.snapshot().get_metadata().get_term()
    );
    Some(ready.snapshot().clone())
} else {
    None
};
```

Then after persisting entries, install the snapshot:

```rust
// Install snapshot if present (BEFORE applying committed entries)
if let Some(snapshot) = snapshot_to_install {
    match self.install_snapshot(snapshot).await {
        Ok(_) => {
            info!("Successfully installed snapshot for {}-{}", self.topic, self.partition);
        }
        Err(e) => {
            error!("Failed to install snapshot for {}-{}: {}", self.topic, self.partition, e);
            // Report failure to Raft
            let mut node = self.raw_node.write();
            node.report_snapshot(self.config.node_id, SnapshotStatus::Failure);
        }
    }
}
```

### Step 2: Implement install_snapshot() Method

Add new method to `PartitionReplica`:

```rust
/// Install a snapshot received from the leader
async fn install_snapshot(&self, snapshot: raft::prelude::Snapshot) -> Result<()> {
    let metadata = snapshot.get_metadata();
    let last_index = metadata.get_index();
    let last_term = metadata.get_term();

    info!(
        "Installing snapshot for {}-{}: index={}, term={}",
        self.topic, self.partition, last_index, last_term
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
        sm.restore(&snapshot_data).await?;
    }

    // Update applied index
    self.applied_index.store(last_index, Ordering::SeqCst);

    // Report success to Raft
    let mut node = self.raw_node.write();
    node.report_snapshot(self.config.node_id, SnapshotStatus::Finish);

    info!(
        "Snapshot installed successfully for {}-{} at index {}",
        self.topic, self.partition, last_index
    );

    Ok(())
}
```

### Step 3: Implement Snapshot Creation Triggering

Add method to trigger snapshot creation when needed:

```rust
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
    let applied = self.applied_index.load(Ordering::SeqCst);
    let term = self.term();

    info!(
        "Creating snapshot for {}-{} at index={}, term={}",
        self.topic, self.partition, applied, term
    );

    // Create snapshot from state machine
    let snapshot_data = if let Some(state_machine) = &self.state_machine {
        let sm = state_machine.read().await;
        sm.snapshot(applied, term).await?
    } else {
        return Err(RaftError::Config("No state machine configured".to_string()));
    };

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
    let storage = self.raw_node.read().mut_store();
    storage.wl().apply_snapshot(raft_snapshot)?;

    info!(
        "Snapshot created and stored for {}-{} at index {}",
        self.topic, self.partition, snapshot_data.last_index
    );

    Ok(())
}
```

### Step 4: Add Periodic Snapshot Creation

In the main Raft processing loop (where tick() is called), add:

```rust
// Periodic snapshot creation (every 30 seconds)
if last_snapshot_check.elapsed() > Duration::from_secs(30) {
    if replica.should_create_snapshot() {
        if let Err(e) = replica.create_snapshot().await {
            warn!("Failed to create snapshot: {}", e);
        }
    }
    last_snapshot_check = Instant::now();
}
```

### Step 5: Update RaftConfig

Ensure `snapshot_threshold` is set appropriately:

```rust
pub struct RaftConfig {
    // ... other fields ...

    /// Create snapshot after N entries (default: 10,000)
    /// Lower for testing (e.g., 100), higher for production
    pub snapshot_threshold: u64,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            // ... other defaults ...
            snapshot_threshold: 10_000,  // Create snapshot every 10K entries
        }
    }
}
```

## Testing Plan

### Test 1: Snapshot Creation
```rust
#[tokio::test]
async fn test_snapshot_creation() {
    // Create replica with low threshold
    let config = RaftConfig {
        snapshot_threshold: 100,
        ..Default::default()
    };

    // Apply 150 entries
    // ...

    // Trigger snapshot
    assert!(replica.should_create_snapshot());
    replica.create_snapshot().await.unwrap();

    // Verify snapshot exists in storage
    let storage = replica.raw_node.read().store();
    let snapshot = storage.snapshot(0, 0).unwrap();
    assert!(snapshot.get_metadata().get_index() >= 100);
}
```

### Test 2: Snapshot Installation
```rust
#[tokio::test]
async fn test_snapshot_installation() {
    // Create replica 1 with data
    // Create snapshot
    // Create replica 2 (empty)
    // Send snapshot from 1 to 2
    // Verify 2 has same state as 1
}
```

### Test 3: Node Rejoin (The Critical Test)
```rust
#[tokio::test]
async fn test_node_rejoin_with_snapshot() {
    // Start 3-node cluster
    // Kill node 3
    // Apply 1000 entries to cluster (nodes 1 and 2)
    // Create snapshot on node 1
    // Restart node 3
    // Node 3 should receive snapshot and catch up
    // Verify node 3 has all 1000 entries
}
```

## Expected Outcome

After this implementation:

1. **Automatic Snapshots**: Replicas will automatically create snapshots every 10K entries
2. **Lagging Node Recovery**: When a follower is too far behind, leader sends snapshot instead of individual entries
3. **Node Rejoin**: Nodes that rejoin after downtime will receive snapshot and catch up
4. **No More Panic**: The "to_commit X is out of range [last_index 0]" panic will be fixed

## Files to Modify

1. `crates/chronik-raft/src/replica.rs`:
   - Add `install_snapshot()` method
   - Add `create_snapshot()` method
   - Add `should_create_snapshot()` method
   - Wire up snapshot handling in `ready()`

2. `crates/chronik-raft/src/lib.rs`:
   - Export snapshot-related types if needed

3. `crates/chronik-server/src/raft_integration.rs`:
   - Add periodic snapshot creation to processing loop

## Timeline

- **Step 1-2** (Snapshot Reception): 2-3 hours
- **Step 3-4** (Snapshot Creation): 2-3 hours
- **Step 5** (Configuration): 30 minutes
- **Testing**: 2-3 hours
- **Total**: ~1 day of focused work

## Success Criteria

✅ Phase 3 test passes without node rejoin panic
✅ Nodes can rejoin cluster after missing >100 commits
✅ Snapshots are created automatically
✅ Snapshot file size is reasonable (<1MB for typical metadata)
✅ No performance degradation during snapshot creation
