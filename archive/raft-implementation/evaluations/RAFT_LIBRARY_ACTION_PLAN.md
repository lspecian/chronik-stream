# Raft Library Decision - Action Plan

**Decision**: Continue with tikv/raft-rs using `prost-codec` feature
**Date**: 2025-10-16
**Status**: Ready to implement

---

## Immediate Actions (This Week)

### 1. Update Cargo.toml for prost-codec

**File**: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-raft/Cargo.toml`

**Change**:
```toml
# Before:
raft = { workspace = true }

# After:
raft = { version = "0.7", default-features = false, features = ["prost-codec"] }
```

**Why**: Eliminates protoc dependency by using pure Rust Prost instead of rust-protobuf.

### 2. Verify Build Without protoc

**Test**:
```bash
# Optional: Temporarily remove protoc to verify
# brew uninstall protobuf  # macOS
# apt remove protobuf-compiler  # Linux

# Should build successfully
cargo build -p chronik-raft

# Verify tests pass
cargo test -p chronik-raft

# If needed, reinstall protoc for other tools
# brew install protobuf
```

**Expected**: Build succeeds without protoc installed.

### 3. Update Documentation

**Files to Update**:

#### a. README.md
**File**: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-raft/README.md`

**Add Section**:
```markdown
## Dependencies

- `raft` (0.7): TiKV's Raft implementation with `prost-codec` feature
  - **Note**: Uses Prost (pure Rust) instead of rust-protobuf
  - **No protoc required**: Build works on systems without Protocol Buffer compiler
- `tonic` (0.12): gRPC framework
- `prost` (0.13): Protobuf serialization
- Standard Chronik dependencies (serde, tokio, async-trait, etc.)

### Build Requirements

**Simplified** (no protoc needed):
1. Rust 1.75+ (workspace edition 2021)
2. Standard Cargo build: `cargo build -p chronik-raft`

**Previous Requirement Removed**: Protocol Buffers Compiler (`protoc`) is no longer needed thanks to `prost-codec` feature.
```

#### b. PHASE1_SUMMARY.md
**File**: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-raft/PHASE1_SUMMARY.md`

**Update "Build Requirements" Section**:
```markdown
## Build Requirements

To build this crate, you need:

1. **Rust** 1.75+ (workspace edition 2021)
2. ~~**Protocol Buffers Compiler** (`protoc`)~~ **NO LONGER REQUIRED**
   - **Update (2025-10-16)**: Using `prost-codec` feature eliminates protoc dependency
   - Build works with pure Rust toolchain only

Build command:
```bash
cargo build --package chronik-raft
```

**Note**: Phase 1 originally documented protoc as required. This has been resolved by using the `prost-codec` feature in raft v0.7, which uses pure Rust code generation instead of the C++ protoc compiler.
```

#### c. CHANGELOG.md
**File**: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/CHANGELOG.md`

**Add Entry**:
```markdown
## [Unreleased]

### Changed
- **chronik-raft**: Use `prost-codec` feature for raft v0.7 to eliminate protoc dependency
  - Build no longer requires Protocol Buffer compiler installation
  - Pure Rust toolchain sufficient for all builds
  - Improves developer experience and CI/CD simplicity

### Documentation
- Add comprehensive Raft library comparison matrix (RAFT_LIBRARY_COMPARISON.md)
- Document decision to continue with tikv/raft-rs vs alternatives (openraft, raftify)
- Update chronik-raft README with simplified build requirements
```

### 4. Verify CI/CD Pipeline

**File**: `.github/workflows/ci.yml` (if exists)

**Check**:
- Ensure CI doesn't install protoc (should work without it)
- Remove any `apt-get install protobuf-compiler` or similar commands
- Verify build succeeds in clean environment

**If protoc is installed in CI**: Remove it (no longer needed for chronik-raft).

---

## Phase 2: WAL Integration (Weeks 2-3)

**Goal**: Implement production-ready Raft with WAL-backed storage

### Tasks

#### 1. Implement WalRaftStorage (3-4 days)

**Create**: `crates/chronik-raft/src/storage/wal_storage.rs`

**Requirements**:
- Implement `RaftLogStorage` trait using `GroupCommitWal`
- Map `RaftEntry` to `WalRecord` format
- Use dedicated topic `__raft_internal` for Raft logs
- Support recovery from WAL on startup

**Key Methods**:
```rust
impl RaftLogStorage for WalRaftStorage {
    async fn append_entries(&self, entries: Vec<RaftEntry>) -> Result<()> {
        // Write to GroupCommitWal with term/index metadata
    }

    async fn get_entry(&self, index: u64) -> Result<Option<RaftEntry>> {
        // Read from WAL by index
    }

    async fn truncate_suffix(&self, from_index: u64) -> Result<()> {
        // Remove uncommitted entries after crash
    }

    async fn truncate_prefix(&self, to_index: u64) -> Result<()> {
        // Compact old entries (after snapshot)
    }
}
```

#### 2. Complete PartitionReplica (2-3 days)

**File**: `crates/chronik-raft/src/replica.rs`

**Requirements**:
- Create `RawNode` from raft-rs library
- Implement `propose()` for new log entries
- Handle `ready()` callback for state changes
- Apply committed entries to state machine
- Manage leader election timeouts

**State Machine Integration**:
```rust
pub struct PartitionStateMachine {
    topic: String,
    partition: i32,
    storage: Arc<dyn SegmentWriter>,
}

impl StateMachine for PartitionStateMachine {
    fn apply(&mut self, entry: RaftEntry) -> Result<()> {
        // Deserialize entry.data as CanonicalRecord
        // Write to segment storage
        // Update high watermark
    }
}
```

#### 3. Wire Up gRPC Service (2 days)

**File**: `crates/chronik-raft/src/rpc.rs`

**Requirements**:
- Connect gRPC handlers to `PartitionReplica` instances
- Route AppendEntries/RequestVote to correct partition
- Handle snapshot streaming
- Add metrics and tracing

#### 4. Integration Testing (3-4 days)

**Create**: `tests/integration/raft_clustering.rs`

**Test Scenarios**:
- Leader election (3-node cluster)
- Log replication (write to leader, verify followers)
- Leader failover (kill leader, verify re-election)
- WAL recovery (restart follower, verify catch-up)
- Network partition (split-brain prevention)
- Snapshot installation (add lagging follower)

**Test Infrastructure**:
```rust
#[tokio::test]
async fn test_leader_election() {
    // Start 3 nodes
    let node1 = start_raft_node(1, vec![2, 3]).await;
    let node2 = start_raft_node(2, vec![1, 3]).await;
    let node3 = start_raft_node(3, vec![1, 2]).await;

    // Wait for leader election
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify exactly one leader
    let leaders = count_leaders(vec![node1, node2, node3]).await;
    assert_eq!(leaders, 1);
}
```

---

## Phase 3: Server Integration (Weeks 4-5)

### Tasks

#### 1. Add Cluster Mode to chronik-server (2-3 days)

**File**: `crates/chronik-server/src/main.rs`

**Add Command**:
```rust
#[derive(Parser)]
enum Commands {
    Standalone,
    Cluster {
        #[arg(long)]
        node_id: u64,
        #[arg(long)]
        node_addr: String,
        #[arg(long)]
        seed_nodes: Vec<String>,
    },
    // ... existing commands
}
```

**Startup Flow**:
```rust
Commands::Cluster { node_id, node_addr, seed_nodes } => {
    // 1. Start Raft service on node_addr
    let raft_service = RaftServiceImpl::new();
    tokio::spawn(start_raft_server(node_addr, raft_service));

    // 2. Create RaftCluster manager
    let cluster = RaftCluster::new(node_id, seed_nodes).await?;

    // 3. Start Kafka server with cluster-aware handlers
    let server = IntegratedKafkaServer::new_clustered(cluster).await?;
    server.run().await?;
}
```

#### 2. Cluster-Aware ProduceHandler (2 days)

**File**: `crates/chronik-server/src/produce_handler.rs`

**Add Routing**:
```rust
pub async fn handle_produce_clustered(
    &self,
    request: ProduceRequest,
    cluster: Arc<RaftCluster>,
) -> Result<ProduceResponse> {
    for topic_data in request.topic_data {
        for partition_data in topic_data.partition_data {
            // Find partition leader
            let leader = cluster.get_partition_leader(
                &topic_data.name,
                partition_data.index
            ).await?;

            if leader == cluster.node_id() {
                // Local leader - propose to Raft
                let replica = cluster.get_replica(
                    &topic_data.name,
                    partition_data.index
                ).await?;
                replica.propose(partition_data.records).await?;
            } else {
                // Remote leader - proxy request
                cluster.proxy_to_leader(leader, request).await?;
            }
        }
    }
}
```

#### 3. Metadata Coordination (2-3 days)

**File**: `crates/chronik-common/src/metadata/clustered.rs`

**Create ClusteredMetadataStore**:
```rust
pub struct ClusteredMetadataStore {
    local_metadata: Arc<ChronikMetaLog>,
    raft_cluster: Arc<RaftCluster>,
}

impl MetadataStore for ClusteredMetadataStore {
    async fn create_topic(&self, topic: &str, config: TopicConfig) -> Result<()> {
        // 1. Propose metadata change to Raft
        let entry = MetadataEntry::CreateTopic { topic, config };
        self.raft_cluster.propose_metadata(entry).await?;

        // 2. Wait for commit
        self.raft_cluster.wait_for_commit().await?;

        // 3. Local metadata automatically updated via state machine
        Ok(())
    }
}
```

#### 4. Health Checks and Monitoring (1-2 days)

**File**: `crates/chronik-server/src/health.rs`

**Add Cluster Endpoints**:
```rust
GET /admin/cluster/members      // List all nodes
GET /admin/cluster/partitions   // List partition leaders
GET /admin/cluster/health       // Cluster health status
POST /admin/cluster/transfer-leader/{topic}/{partition}/{to_node}
```

**Prometheus Metrics**:
```rust
raft_leader_elections_total{node_id}
raft_log_replication_lag_seconds{topic, partition}
raft_proposals_total{node_id, topic, partition}
raft_snapshot_transfers_total{node_id}
```

---

## Timeline Summary

| Phase | Duration | Status |
|-------|----------|--------|
| Immediate Actions | Week 1 | **Ready to start** |
| Phase 2: WAL Integration | Weeks 2-3 | Planned |
| Phase 3: Server Integration | Weeks 4-5 | Planned |
| **Total** | **5 weeks** | On track |

---

## Success Criteria

### Week 1 (Immediate)
- ✅ `cargo build -p chronik-raft` succeeds without protoc
- ✅ Documentation updated (README, CHANGELOG, PHASE1_SUMMARY)
- ✅ Decision documented (RAFT_LIBRARY_COMPARISON.md)

### Week 3 (Phase 2)
- ✅ `WalRaftStorage` implemented and tested
- ✅ `PartitionReplica` fully functional
- ✅ Integration tests pass (leader election, replication, recovery)
- ✅ No memory leaks or panics in 24h stress test

### Week 5 (Phase 3)
- ✅ 3-node cluster successfully bootstrapped
- ✅ ProduceRequest routed to partition leaders
- ✅ Metadata changes replicated across cluster
- ✅ Health endpoints return correct status
- ✅ Prometheus metrics exported

---

## Risk Mitigation

### Risk: WAL async conversion required
**Likelihood**: LOW (WAL already has sync API, Raft is sync)
**Impact**: MEDIUM (would delay Phase 2)
**Mitigation**: Keep `WalRaftStorage` sync, use `spawn_blocking` if needed

### Risk: raft-rs API complexity
**Likelihood**: MEDIUM (lower-level API requires more code)
**Impact**: LOW (TiKV examples available)
**Mitigation**: Reference TiKV source code, community support

### Risk: Integration bugs
**Likelihood**: MEDIUM (complex distributed system)
**Impact**: MEDIUM (would delay Phase 3)
**Mitigation**: Comprehensive integration testing, gradual rollout

---

## Next Steps

1. **Review this action plan** with team
2. **Approve decision** to continue with tikv/raft-rs
3. **Assign Week 1 tasks** to implement prost-codec change
4. **Schedule Phase 2 kickoff** for Week 2
5. **Update project roadmap** with 5-week timeline

---

## References

- **Decision Matrix**: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/RAFT_LIBRARY_COMPARISON.md`
- **Phase 1 Summary**: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-raft/PHASE1_SUMMARY.md`
- **Raft Docs**: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/docs/raft/README.md`
- **tikv/raft-rs**: https://github.com/tikv/raft-rs

---

**Status**: Ready for implementation
**Next Review**: After Week 1 completion
**Owner**: Development Team
