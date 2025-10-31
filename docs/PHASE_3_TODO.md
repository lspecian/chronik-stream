# Phase 3 Integration Work - TODO List

**Status**: Old code cleanup complete ✅  
**Branch**: `feat/v2.5.0-kafka-cluster`  
**Base**: Clean Phase 2 implementation (RaftCluster + MetadataStateMachine)

---

## Current Build Status

**Libraries**: ✅ All compile (chronik-wal, chronik-protocol, chronik-storage, etc.)  
**Binary**: ❌ 25 errors (expected - integration work needed)

### Error Breakdown (25 total)

#### 1. Missing CLI Integration (3 errors)
**Files**: `main.rs:480-481`
- ❌ `run_raft_cluster()` function doesn't exist
- ❌ `RaftClusterConfig` struct doesn't exist
- **Action**: Implement Phase 2 Step 2.4 - Wire RaftCluster to CLI

#### 2. Old `raft_manager` Field References (10 errors)
**Files**: 
- `produce_handler.rs` (6 errors)
- `fetch_handler.rs` (2 errors)
- `integrated_server.rs` (2 errors)

**Problem**: Code tries to access `self.raft_manager` which was part of old v2.4.1 architecture
**Action**: Replace with new Phase 2 API (`raft_cluster`)

#### 3. Old Module References (2 errors)
**Files**: `integrated_server.rs`, `produce_handler.rs`
- ❌ `use crate::raft_integration::*` - module deleted
- **Action**: Remove these imports

#### 4. Old Crate Reference (1 error)
**File**: `fetch_handler.rs:334`
- ❌ `chronik_raft::ReadIndexRequest` - crate deleted
- **Action**: Remove or replace with new API

#### 5. Duplicate Method Definitions (2 errors)
**File**: `raft_cluster.rs`
- ❌ `bootstrap()` defined twice
- ❌ `propose()` defined twice
- **Action**: Remove duplicate definitions

#### 6. Type Inference Issues (7 errors)
**Cause**: Side effects of missing `raft_manager` field
**Action**: Will resolve automatically after fixing items 2-3

---

## Phase 3 Implementation Tasks

### Task 1: Complete Phase 2 Step 2.4 (CLI Integration)
**File**: `crates/chronik-server/src/raft_cluster.rs`

**Add these functions**:
```rust
pub struct RaftClusterConfig {
    pub node_id: u64,
    pub raft_addr: String,
    pub peers: Vec<(u64, String)>,
    pub bootstrap: bool,
    pub kafka_port: u16,
    pub advertised_addr: String,
}

pub async fn run_raft_cluster(config: RaftClusterConfig) -> Result<()> {
    // 1. Bootstrap RaftCluster
    let cluster = RaftCluster::bootstrap(config.node_id, config.peers).await?;
    
    // 2. Create IntegratedKafkaServer with raft_cluster
    let server_config = IntegratedServerConfig {
        kafka_port: config.kafka_port,
        advertised_addr: config.advertised_addr,
        raft_cluster: Some(Arc::new(cluster)),  // NEW
        ..Default::default()
    };
    
    let server = IntegratedKafkaServer::new(server_config).await?;
    
    // 3. Start Raft background task
    // 4. Start Kafka server
    server.run().await
}
```

**Estimated Time**: 2 hours

---

### Task 2: Remove Old raft_manager References
**Files**: `produce_handler.rs`, `fetch_handler.rs`, `integrated_server.rs`

**Find and replace**:
```rust
// OLD (v2.4.1)
if let Some(ref raft_mgr) = self.raft_manager {
    raft_mgr.propose(...).await?;
}

// NEW (v2.5.0 Phase 2)
if let Some(ref cluster) = self.raft_cluster {
    // Metadata operations only
    cluster.assign_partition(topic, partition, replicas)?;
}
```

**Action items**:
1. Remove `raft_manager: Option<Arc<RaftManager>>` fields
2. Add `raft_cluster: Option<Arc<RaftCluster>>` fields (optional)
3. Update all method calls to new API
4. Remove data replication calls (WAL streaming handles this)

**Estimated Time**: 3-4 hours

---

### Task 3: Remove Old Module/Crate References
**Files**: Multiple

**Remove these imports**:
```rust
use crate::raft_integration::*;  // Module deleted
use chronik_raft::*;              // Crate removed from dependencies
```

**Estimated Time**: 30 minutes

---

### Task 4: Fix Duplicate Method Definitions
**File**: `raft_cluster.rs`

**Check for**:
- Multiple `impl RaftCluster` blocks
- Duplicate `bootstrap()` or `propose()` methods
- Remove old implementations, keep Phase 2 version

**Estimated Time**: 15 minutes

---

### Task 5: Integrate ISR Tracker (Phase 3.2)
**File**: `crates/chronik-server/src/wal_replication.rs`

**Add partition-level replication**:
```rust
impl WalReplicationManager {
    pub async fn replicate_partition(
        &self,
        topic: String,
        partition: i32,
        offset: i64,
        data: Vec<u8>,
    ) {
        // Get replicas from RaftCluster metadata
        if let Some(ref cluster) = self.raft_cluster {
            if let Some(replicas) = cluster.get_partition_replicas(&topic, partition) {
                for replica_node in replicas {
                    // Check if in-sync using ISR tracker
                    if self.isr_tracker.is_in_sync(replica_node, &topic, partition) {
                        self.send_to_node(replica_node, topic.clone(), partition, offset, data.clone()).await;
                    }
                }
            }
        }
    }
}
```

**Estimated Time**: 2-3 hours

---

### Task 6: Update ProduceHandler (Phase 3.3)
**File**: `crates/chronik-server/src/produce_handler.rs`

**Add after WAL write**:
```rust
// After: let offset = self.wal_mgr.append(...).await?;

// Trigger partition replication (if followers exist)
if let Some(ref repl_mgr) = self.wal_replication_manager {
    if let Some(ref cluster) = self.raft_cluster {
        // Only replicate if this partition has replicas
        if let Some(replicas) = cluster.get_partition_replicas(topic, partition) {
            if replicas.len() > 1 {
                repl_mgr.replicate_partition(
                    topic.to_string(),
                    partition,
                    offset,
                    serialized_data,
                ).await;
            }
        }
    }
}
```

**Estimated Time**: 1-2 hours

---

## Testing Plan

### After Task 1-4 (CLI + Cleanup)
```bash
# Should compile successfully
cargo build --release --bin chronik-server

# Standalone mode should work
./target/release/chronik-server standalone
```

### After Task 5-6 (Replication)
```bash
# Start 3-node cluster
./target/release/chronik-server --node-id 1 --kafka-port 9092 --advertised-addr localhost \
  raft-cluster --raft-addr 0.0.0.0:9192 --peers "2@localhost:9193,3@localhost:9194" --bootstrap

# Benchmark with 2 followers
./target/release/chronik-bench --bootstrap-servers localhost:9092 \
  --duration 30 --concurrency 128 --message-size 256

# Target: >= 45K msg/s
```

---

## Success Criteria

✅ **Compilation**: Zero errors  
✅ **Standalone**: Works without regression (50K+ msg/s)  
✅ **Cluster Mode**: CLI accepts raft-cluster subcommand  
✅ **Replication**: Messages replicate to followers  
✅ **ISR Tracking**: Follower lag tracked correctly  
✅ **Performance**: >= 45K msg/s with 2 followers  

---

## Estimated Timeline

- Task 1 (CLI): 2 hours
- Task 2 (Remove old refs): 3-4 hours  
- Task 3 (Remove imports): 30 minutes
- Task 4 (Fix duplicates): 15 minutes
- Task 5 (ISR integration): 2-3 hours
- Task 6 (ProduceHandler): 1-2 hours

**Total**: 9-12 hours of focused work

---

## Notes

- Phase 2 implementation (RaftCluster + MetadataStateMachine) is **complete and tested** ✅
- ISR Tracker is **implemented** ✅
- Only integration/wiring work remains
- No architectural changes needed - follow CLEAN_RAFT_IMPLEMENTATION.md plan

**Next Step**: Start with Task 1 (CLI integration) - this will immediately reduce errors from 25 → ~22
