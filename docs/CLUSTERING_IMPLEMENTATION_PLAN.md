# Chronik Clustering Implementation Plan

**Architecture**: Multi-Raft with Shared WAL
**Target Version**: v2.0.0
**Status**: Planning
**Last Updated**: 2025-10-15

## Executive Summary

Transform Chronik from single-node to distributed multi-node cluster with:
- **Replication**: 3x redundancy (configurable)
- **High Availability**: Automatic leader election per partition
- **Strong Consistency**: Raft consensus for both data and metadata
- **Zero Data Loss**: Quorum-based commits with ISR tracking
- **Kafka Compatibility**: Maintain full Kafka protocol compatibility

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                  Chronik Distributed Cluster                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  Node 1                  Node 2                  Node 3          │
│  ┌─────────────┐        ┌─────────────┐        ┌─────────────┐  │
│  │ Partition 0 │        │ Partition 0 │        │ Partition 0 │  │
│  │  (LEADER)   │───────▶│ (FOLLOWER)  │───────▶│ (FOLLOWER)  │  │
│  │             │        │             │        │             │  │
│  │ WAL + Raft  │        │ WAL + Raft  │        │ WAL + Raft  │  │
│  └─────────────┘        └─────────────┘        └─────────────┘  │
│        │                       │                       │         │
│        └───────── Raft Consensus (AppendEntries) ─────┘         │
│                                                                   │
│  Produce Flow:                                                   │
│  1. Producer → Leader's ProduceHandler                           │
│  2. Leader appends to local WAL (uncommitted)                    │
│  3. Leader replicates via Raft AppendEntries (parallel)          │
│  4. Followers append to WAL, ack                                 │
│  5. Leader waits for quorum (min.insync.replicas=2)              │
│  6. Leader commits, responds to producer                         │
│                                                                   │
│  Metadata Store (ChronikMetaLog):                                │
│  - Also replicated via Raft                                      │
│  - Topics, partitions, consumer groups, offsets                  │
│  - Single Raft group for cluster-wide metadata                   │
└─────────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### 1. Multi-Raft Architecture
- **Decision**: One Raft group per partition + one for metadata
- **Rationale**:
  - Isolates failure domains (one partition fails ≠ cluster fails)
  - Natural fit with Chronik's partition model
  - Proven at scale (TiKV, CockroachDB)
- **Trade-off**: Higher memory/CPU vs. single Raft (acceptable for partition count)

### 2. WAL as Raft Log Storage
- **Decision**: `GroupCommitWal` implements `RaftLogStorage` trait
- **Rationale**:
  - WAL is already log-structured and durable (fsync)
  - No duplicate storage (Raft log = WAL)
  - Reuses existing batch commit optimization
- **Implementation**: Raft entries map 1:1 to WAL records

### 3. Raft Library: raft-rs
- **Decision**: Use [`raft-rs`](https://github.com/tikv/raft-rs) (TiKV's Raft)
- **Rationale**:
  - Production-proven (powers TiKV)
  - Feature-complete (snapshots, membership changes, leadership transfer)
  - Well-documented, active maintenance
- **Trade-off**: Learning curve vs. custom implementation

### 4. RPC Layer: gRPC
- **Decision**: gRPC for inter-node Raft communication
- **Rationale**:
  - Industry standard (strong typing, code generation)
  - Built-in connection management, multiplexing
  - Easy to add TLS for security
- **Trade-off**: Overhead vs. custom TCP (acceptable for clarity)

### 5. Snapshot Strategy
- **Decision**: Trigger snapshots based on **WAL log size** (configurable)
- **Default**: Snapshot when partition WAL exceeds 64MB
- **Rationale**:
  - Prevents unbounded log growth
  - Fast follower catch-up (send snapshot vs. replay 10K entries)
  - Balances snapshot overhead vs. recovery time
- **Configuration**:
  ```toml
  [raft.snapshots]
  max_log_size_mb = 64          # Trigger snapshot at 64MB
  min_interval_entries = 10000  # Don't snapshot too frequently
  ```

### 6. Node Discovery
- **Phase 3**: Static configuration (manual peer list)
- **Phase 5**: DNS-based discovery (Kubernetes-ready)
- **Rationale**:
  - Static config is simple for initial deployment
  - DNS SRV records enable dynamic discovery in K8s/cloud
- **Future**: Support etcd/Consul for advanced deployments

### 7. Failure Testing Strategy
- **Tool**: Toxiproxy for network fault injection
- **Scenarios**:
  - Leader failure (election)
  - Network partition (split-brain)
  - Follower lag (ISR removal/re-add)
  - Cascading failures (2/3 nodes down)
- **Future**: Jepsen testing for formal verification

## Implementation Phases

---

## Phase 1: Raft Foundation (v2.0.0-alpha.1)

**Goal**: Single-partition replication with hardcoded 3-node cluster

**Status**: 🔴 Not Started

### Tasks

#### 1.1 Create `chronik-raft` Crate
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Create `crates/chronik-raft/` directory structure
  - [ ] Add `raft-rs` dependency (latest stable)
  - [ ] Add `tonic` (gRPC) and `prost` (protobuf) dependencies
  - [ ] Define `Cargo.toml` with feature flags
- [ ] **Artifacts**:
  - `crates/chronik-raft/Cargo.toml`
  - `crates/chronik-raft/src/lib.rs`
- [ ] **Tests**: Crate compiles and imports `raft-rs`

#### 1.2 Define Raft RPC Protocol (gRPC)
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 1 day
- [ ] **Tasks**:
  - [ ] Define `raft_rpc.proto` (AppendEntries, RequestVote, InstallSnapshot)
  - [ ] Generate Rust code with `tonic-build`
  - [ ] Create `RaftRpcService` trait
- [ ] **Artifacts**:
  - `crates/chronik-raft/proto/raft_rpc.proto`
  - `crates/chronik-raft/src/rpc.rs` (generated)
- [ ] **Tests**: gRPC service compiles, basic ping/pong test

#### 1.3 Implement `RaftLogStorage` Trait on `GroupCommitWal`
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] Define `RaftLogStorage` trait in `chronik-raft`
  - [ ] Implement `append_entries()` (map Raft entries → WAL records)
  - [ ] Implement `get_entry()`, `get_entries(range)`
  - [ ] Implement `first_index()`, `last_index()`
  - [ ] Handle entry deletion (Raft log compaction)
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/storage.rs` (trait)
  - `crates/chronik-wal/src/raft_storage_impl.rs` (impl)
- [ ] **Tests**:
  - [ ] Append 1000 entries, read back sequentially
  - [ ] Test range queries (get entries 100-200)
  - [ ] Test compaction (delete entries < index 500)

#### 1.4 Create `PartitionReplica` (Single Partition)
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 4 days
- [ ] **Tasks**:
  - [ ] Create `PartitionReplica` struct (owns `RawNode<RaftLogStorage>`)
  - [ ] Implement `propose()` (propose Raft entry)
  - [ ] Implement `ready()` handler (process Raft messages, commits)
  - [ ] Integrate with `ProduceHandler` (append → propose → commit)
  - [ ] Hardcode 3-node cluster config for testing
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/partition_replica.rs`
- [ ] **Tests**:
  - [ ] Single-node Raft starts, proposes entry, commits
  - [ ] 3-node Raft elects leader, replicates entry

#### 1.5 End-to-End Single-Partition Test
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Create integration test with 3 Chronik nodes
  - [ ] Produce message to leader
  - [ ] Verify replication to all 3 WALs
  - [ ] Consume from follower (should succeed after commit)
  - [ ] Test leader failure (kill leader, new election, continue producing)
- [ ] **Artifacts**:
  - `tests/integration/raft_single_partition_test.rs`
- [ ] **Success Criteria**:
  - [ ] Message replicates to 3 nodes
  - [ ] Leader election completes in < 2 seconds
  - [ ] No message loss during leader failover

### Phase 1 Deliverables
- ✅ Single partition can replicate across 3 nodes
- ✅ Raft leader election works
- ✅ Produce/consume through Raft-replicated WAL
- ✅ Basic failure recovery (leader dies, new leader elected)

### Phase 1 Risks
- **Risk**: `raft-rs` integration complexity
  - **Mitigation**: Start with minimal example from raft-rs docs
- **Risk**: WAL format changes break existing data
  - **Mitigation**: Add version field to WalRecord, support V2 (non-Raft) + V3 (Raft)

---

## Phase 2: Multi-Partition Raft (v2.0.0-alpha.2)

**Goal**: Support multiple partitions, each with independent Raft group

**Status**: 🔴 Not Started

### Tasks

#### 2.1 Create `RaftGroupManager`
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] Create `RaftGroupManager` to manage multiple `PartitionReplica` instances
  - [ ] Map `(topic, partition)` → `RaftGroup`
  - [ ] Handle dynamic Raft group creation (when partition assigned)
  - [ ] Implement tick loop (drive all Raft groups)
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/group_manager.rs`
- [ ] **Tests**:
  - [ ] Create 10 Raft groups, verify independence
  - [ ] Leader election in group 0 doesn't affect group 1

#### 2.2 Partition Assignment Strategy
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Implement round-robin partition assignment
  - [ ] Create `PartitionAssignmentMap` (partition → [node1, node2, node3])
  - [ ] Persist assignment in metadata store
- [ ] **Artifacts**:
  - `crates/chronik-common/src/partition_assignment.rs`
- [ ] **Tests**:
  - [ ] 3 nodes, 9 partitions → each node leads 3 partitions

#### 2.3 Update `ProduceHandler` for Multi-Partition
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Route produce request to correct Raft group (by partition)
  - [ ] Return `NOT_LEADER_FOR_PARTITION` error if not leader
  - [ ] Update Kafka Metadata response with correct leader node
- [ ] **Artifacts**:
  - `crates/chronik-server/src/produce_handler.rs` (updated)
- [ ] **Tests**:
  - [ ] Produce to partition 0 on Node 1 (leader) → success
  - [ ] Produce to partition 0 on Node 2 (follower) → error with leader hint

#### 2.4 Update `FetchHandler` for Multi-Partition
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Allow fetches from followers (read committed data only)
  - [ ] Or redirect to leader (configurable via `CHRONIK_FETCH_FROM_FOLLOWERS`)
- [ ] **Artifacts**:
  - `crates/chronik-server/src/fetch_handler.rs` (updated)
- [ ] **Tests**:
  - [ ] Consume from follower (committed data) → success
  - [ ] Consume from follower (uncommitted data) → wait or redirect

#### 2.5 End-to-End Multi-Partition Test
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Create topic with 3 partitions, replication factor 3
  - [ ] Produce to all 3 partitions (round-robin by key)
  - [ ] Verify each partition has independent leader
  - [ ] Kill leader of partition 0, verify re-election
  - [ ] Verify partitions 1 and 2 unaffected
- [ ] **Artifacts**:
  - `tests/integration/raft_multi_partition_test.rs`
- [ ] **Success Criteria**:
  - [ ] All 3 partitions replicate independently
  - [ ] Leader failure in one partition doesn't affect others
  - [ ] Kafka clients can produce/consume normally

### Phase 2 Deliverables
- ✅ Multiple partitions each with independent Raft group
- ✅ Partition assignment (which node leads which partition)
- ✅ Produce/fetch routed to correct Raft group
- ✅ Kafka protocol returns correct leader metadata

### Phase 2 Risks
- **Risk**: Memory/CPU overhead with many Raft groups (100+ partitions)
  - **Mitigation**: Profile with 100 partitions, optimize tick loop if needed
- **Risk**: Metadata store not yet replicated (single point of failure)
  - **Mitigation**: Phase 3 will address this

---

## Phase 3: Cluster Membership & Metadata Replication (v2.0.0-beta.1)

**Goal**: Dynamic cluster with replicated metadata store

**Status**: 🔴 Not Started

### Tasks

#### 3.1 Static Node Discovery
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Define `cluster` config in `chronik.toml`
  - [ ] Parse static peer list (node_id, address, port)
  - [ ] Create `ClusterConfig` struct
  - [ ] Implement `--cluster-config` CLI flag
- [ ] **Artifacts**:
  - `crates/chronik-config/src/cluster.rs`
  - Updated `chronik.toml` with `[cluster.peers]`
- [ ] **Tests**:
  - [ ] Parse valid cluster config
  - [ ] Reject invalid config (duplicate node IDs, missing fields)

#### 3.2 Bootstrap Cluster from Config
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] Implement cluster bootstrap (first node starts, waits for quorum)
  - [ ] Handle initial Raft voting (elect first leader)
  - [ ] Create `ClusterCoordinator` (tracks alive nodes)
  - [ ] Implement heartbeat/health checks between nodes
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/cluster_coordinator.rs`
- [ ] **Tests**:
  - [ ] Start 3 nodes sequentially, cluster forms
  - [ ] Start 1 node alone, waits for quorum (timeout)

#### 3.3 Replicate ChronikMetaLog via Raft
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 5 days
- [ ] **Tasks**:
  - [ ] Create single Raft group for metadata (`__meta` partition)
  - [ ] Implement `MetadataRaftStorage` (stores metadata events)
  - [ ] Update `ChronikMetaLog` to propose metadata ops via Raft
  - [ ] Ensure `create_topic`, `update_offset`, etc. go through Raft
  - [ ] Handle metadata reads (query committed state)
- [ ] **Artifacts**:
  - `crates/chronik-common/src/metadata/raft_meta_log.rs`
- [ ] **Tests**:
  - [ ] Create topic on Node 1, visible on Node 2 after commit
  - [ ] Update consumer offset on Node 2, replicated to Node 1
  - [ ] Kill metadata leader, new leader elected, operations continue

#### 3.4 Partition Assignment on Cluster Start
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] Implement initial partition assignment (on cluster bootstrap)
  - [ ] Distribute partitions evenly across nodes (round-robin)
  - [ ] Handle new topic creation (assign partitions to nodes)
  - [ ] Store assignment in replicated metadata
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/partition_assignment.rs`
- [ ] **Tests**:
  - [ ] 3 nodes, create topic with 9 partitions → 3 per node
  - [ ] 5 nodes, create topic with 10 partitions → 2 per node (balanced)

#### 3.5 End-to-End Cluster Test
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] Start 3-node cluster from scratch
  - [ ] Create topic via Kafka protocol (any node)
  - [ ] Produce to all partitions (round-robin)
  - [ ] Consume from all partitions (follower fetching)
  - [ ] Kill one node, verify cluster continues
  - [ ] Restart killed node, verify it rejoins
- [ ] **Artifacts**:
  - `tests/integration/raft_cluster_test.rs`
- [ ] **Success Criteria**:
  - [ ] Topic creation replicates across all nodes
  - [ ] Produce/consume works from any node
  - [ ] Cluster survives 1-node failure
  - [ ] Node can rejoin cluster after restart

### Phase 3 Deliverables
- ✅ 3-node cluster with static configuration
- ✅ Metadata (topics, offsets) replicated via Raft
- ✅ Automatic partition assignment
- ✅ Full Kafka protocol compatibility in clustered mode

### Phase 3 Risks
- **Risk**: Split-brain during network partition
  - **Mitigation**: Raft prevents this by design (quorum required)
- **Risk**: Metadata operations slow due to Raft consensus
  - **Mitigation**: Metadata ops are rare (create topic, commit offset), acceptable latency

---

## Phase 4: Production Features (v2.0.0-rc.1)

**Goal**: ISR tracking, controlled shutdown, metrics, S3 bootstrap

**Status**: 🔴 Not Started

### Tasks

#### 4.1 ISR (In-Sync Replica) Tracking
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 4 days
- [ ] **Tasks**:
  - [ ] Track follower replication lag (match_index vs. commit_index)
  - [ ] Define "in-sync" threshold (`replica.lag.max.ms` equivalent)
  - [ ] Remove followers from ISR if lagging > threshold
  - [ ] Re-add followers to ISR when caught up
  - [ ] Fail produce if ISR size < `min.insync.replicas`
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/isr_tracker.rs`
- [ ] **Tests**:
  - [ ] Slow follower removed from ISR after timeout
  - [ ] Follower rejoins ISR after catching up
  - [ ] Produce fails if ISR drops below min threshold

#### 4.2 Controlled Shutdown (Leadership Transfer)
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] Implement graceful shutdown (SIGTERM handler)
  - [ ] Transfer Raft leadership before shutdown (raft-rs supports this)
  - [ ] Drain in-flight requests (wait for commits)
  - [ ] Close WAL and network connections cleanly
- [ ] **Artifacts**:
  - `crates/chronik-server/src/shutdown.rs`
- [ ] **Tests**:
  - [ ] SIGTERM triggers leadership transfer
  - [ ] New leader elected before old leader exits
  - [ ] No message loss during graceful shutdown

#### 4.3 Bootstrap New Node from S3
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 4 days
- [ ] **Tasks**:
  - [ ] New node joins cluster (add to Raft config)
  - [ ] Leader sends snapshot (includes WAL state + metadata)
  - [ ] New node downloads snapshot from S3 (if available)
  - [ ] New node replays WAL from snapshot point
  - [ ] New node becomes follower, starts serving
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/bootstrap.rs`
- [ ] **Tests**:
  - [ ] Add 4th node to 3-node cluster
  - [ ] New node downloads snapshot, catches up
  - [ ] New node becomes follower, can serve fetches

#### 4.4 Replication Metrics
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 2 days
- [ ] **Tasks**:
  - [ ] Expose Prometheus metrics:
    - `chronik_raft_leader_count{node_id}` - Number of partitions where this node is leader
    - `chronik_raft_follower_lag{partition, follower_id}` - Replication lag in entries
    - `chronik_raft_isr_size{partition}` - Current ISR size
    - `chronik_raft_election_count` - Number of elections (detect instability)
    - `chronik_raft_commit_latency_ms` - Time from propose to commit (p50, p99)
  - [ ] Add to `chronik-monitoring` crate
- [ ] **Artifacts**:
  - `crates/chronik-monitoring/src/raft_metrics.rs`
- [ ] **Tests**:
  - [ ] Scrape `/metrics`, verify Raft metrics present
  - [ ] Leader election increments `election_count`

#### 4.5 End-to-End Production Test
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] 3-node cluster, create topic with 6 partitions, replication factor 3
  - [ ] Produce 1M messages with kafka-python
  - [ ] Consume with consumer group (3 consumers)
  - [ ] Kill leader of partition 0 mid-produce
  - [ ] Verify zero message loss (consume count == produce count)
  - [ ] Verify ISR tracking in metrics
  - [ ] Gracefully shutdown one node (SIGTERM)
- [ ] **Artifacts**:
  - `tests/integration/raft_production_test.rs`
- [ ] **Success Criteria**:
  - [ ] 1M messages produced and consumed (100% accuracy)
  - [ ] Leader failover completes in < 3 seconds
  - [ ] No duplicate messages
  - [ ] Metrics show healthy replication lag (< 100ms)

### Phase 4 Deliverables
- ✅ ISR tracking with automatic shrink/expand
- ✅ Graceful shutdown with leadership transfer
- ✅ New nodes can bootstrap from S3 snapshots
- ✅ Production-ready replication metrics
- ✅ Zero message loss under failure scenarios

### Phase 4 Risks
- **Risk**: Snapshot size too large for fast bootstrap
  - **Mitigation**: Compress snapshots (gzip), stream from S3
- **Risk**: ISR thrashing (follower repeatedly removed/added)
  - **Mitigation**: Hysteresis (longer grace period for re-add than remove)

---

## Phase 5: Advanced Features (v2.1.0)

**Goal**: Dynamic rebalancing, DNS discovery, rolling upgrades

**Status**: 🔴 Not Started

### Tasks

#### 5.1 DNS-Based Node Discovery
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 3 days
- [ ] **Tasks**:
  - [ ] Query DNS SRV records for peer list
  - [ ] Support Kubernetes headless service pattern
  - [ ] Fallback to static config if DNS unavailable
  - [ ] Handle DNS changes (nodes added/removed)
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/discovery.rs`
- [ ] **Configuration**:
  ```toml
  [cluster.discovery]
  mode = "dns"  # or "static"
  dns_service = "chronik-headless.default.svc.cluster.local"
  ```
- [ ] **Tests**:
  - [ ] Mock DNS SRV records, verify peer discovery
  - [ ] Node joins via DNS, participates in cluster

#### 5.2 Dynamic Partition Rebalancing
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 5 days
- [ ] **Tasks**:
  - [ ] Detect imbalance (node has 2x partitions vs. average)
  - [ ] Trigger leadership transfer (move leader to underloaded node)
  - [ ] Update partition assignment in metadata
  - [ ] Ensure zero downtime during rebalance
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/rebalancer.rs`
- [ ] **Tests**:
  - [ ] 3 nodes balanced, add 4th node → partitions redistribute
  - [ ] Produce/consume continues during rebalance

#### 5.3 Rolling Upgrade Support
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 4 days
- [ ] **Tasks**:
  - [ ] Version Raft RPC messages (support N and N-1 versions)
  - [ ] Backward-compatible WAL format (V3 → V4 migration)
  - [ ] Add `--version-compat` flag (e.g., `--version-compat=v1.x`)
  - [ ] Document upgrade procedure (rolling restart order)
- [ ] **Artifacts**:
  - `docs/ROLLING_UPGRADE_GUIDE.md`
- [ ] **Tests**:
  - [ ] Start cluster with v2.0, upgrade one node to v2.1
  - [ ] Verify mixed-version cluster operates correctly
  - [ ] Complete upgrade (all nodes v2.1), verify compatibility

#### 5.4 Multi-Datacenter Replication (Stretch)
- [ ] **Status**: Not Started
- [ ] **Assignee**: TBD
- [ ] **Estimate**: 8 days
- [ ] **Tasks**:
  - [ ] Implement cross-datacenter Raft (higher latency tolerance)
  - [ ] Preferential leader election (prefer local datacenter)
  - [ ] Async replication to remote DC (eventual consistency)
  - [ ] Handle network partitions between DCs
- [ ] **Artifacts**:
  - `crates/chronik-raft/src/multi_dc.rs`
- [ ] **Configuration**:
  ```toml
  [cluster.datacenters]
  local = "us-west-2"
  remotes = ["us-east-1", "eu-west-1"]
  replication_mode = "async"  # or "sync" for strong consistency
  ```
- [ ] **Tests**:
  - [ ] 3-DC cluster (2 nodes per DC)
  - [ ] Produce in us-west-2, replicate to us-east-1 (async)
  - [ ] Network partition between DCs, verify local cluster continues

### Phase 5 Deliverables
- ✅ DNS-based discovery (Kubernetes-ready)
- ✅ Automatic partition rebalancing
- ✅ Rolling upgrade support (zero downtime)
- ✅ (Optional) Multi-datacenter replication

### Phase 5 Risks
- **Risk**: Cross-DC latency breaks Raft assumptions
  - **Mitigation**: Tune timeouts for WAN (5-10x LAN values)
- **Risk**: Rolling upgrade breaks protocol compatibility
  - **Mitigation**: Thorough versioning, explicit compatibility matrix

---

## Configuration Reference

### Complete `chronik.toml` Example

```toml
[cluster]
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[cluster.peers]
# Static peer list (Phase 3)
nodes = [
  { id = 1, addr = "10.0.1.10:9092" },
  { id = 2, addr = "10.0.1.11:9092" },
  { id = 3, addr = "10.0.1.12:9092" },
]

[cluster.discovery]
# DNS discovery (Phase 5)
mode = "static"  # or "dns"
dns_service = "chronik-headless.default.svc.cluster.local"

[raft]
# Timing
election_timeout_ms = 1000
heartbeat_interval_ms = 100

# Snapshots (size-based + interval-based)
snapshot_max_log_size_mb = 64      # Trigger at 64MB
snapshot_min_interval_entries = 10000  # But not more than every 10K entries
snapshot_compression = "zstd"      # Compress snapshots

# Network
rpc_timeout_ms = 5000
max_inflight_messages = 256

[raft.advanced]
# ISR tracking
replica_lag_max_entries = 1000     # Remove from ISR if lagging > 1000 entries
replica_lag_max_ms = 10000         # Or > 10 seconds

# Leadership
enable_leadership_transfer = true  # Graceful shutdown
leadership_transfer_timeout_ms = 5000

# Performance
batch_append_entries = true        # Batch Raft messages
max_append_entries_size = 1048576  # 1MB per AppendEntries
```

### Environment Variables

```bash
# Enable clustering
CHRONIK_CLUSTER_ENABLED=true
CHRONIK_NODE_ID=1

# Raft tuning
CHRONIK_RAFT_ELECTION_TIMEOUT_MS=1000
CHRONIK_RAFT_SNAPSHOT_SIZE_MB=64

# Override peer list (comma-separated)
CHRONIK_CLUSTER_PEERS=10.0.1.10:9092,10.0.1.11:9092,10.0.1.12:9092

# DNS discovery
CHRONIK_CLUSTER_DISCOVERY_MODE=dns
CHRONIK_CLUSTER_DNS_SERVICE=chronik-headless.default.svc.cluster.local
```

---

## Testing Strategy

### Unit Tests
- **Location**: Each crate's `tests/` directory
- **Scope**: Individual components (RaftLogStorage, ISRTracker, etc.)
- **Run**: `cargo test --lib --bins --workspace`

### Integration Tests
- **Location**: `tests/integration/raft_*.rs`
- **Scope**: Multi-node scenarios (3-node cluster, leader election, etc.)
- **Run**: `cargo test --test raft_cluster_test`

### Fault Injection Tests (Toxiproxy)
- **Tool**: [Toxiproxy](https://github.com/Shopify/toxiproxy)
- **Setup**:
  ```bash
  # Start Toxiproxy
  docker run -d --name toxiproxy -p 8474:8474 -p 9090-9092:9090-9092 shopify/toxiproxy

  # Configure proxy for Chronik nodes
  toxiproxy-cli create chronik1 -l 0.0.0.0:9090 -u 10.0.1.10:9092
  toxiproxy-cli create chronik2 -l 0.0.0.0:9091 -u 10.0.1.11:9092
  toxiproxy-cli create chronik3 -l 0.0.0.0:9092 -u 10.0.1.12:9092
  ```
- **Scenarios**:
  1. **Network Partition**:
     ```bash
     # Isolate Node 1 from Node 2/3
     toxiproxy-cli toxic add chronik1 -t timeout -a timeout=0
     ```
     - **Expected**: Node 2 or 3 becomes leader, cluster continues

  2. **High Latency**:
     ```bash
     # Add 500ms latency to Node 3
     toxiproxy-cli toxic add chronik3 -t latency -a latency=500
     ```
     - **Expected**: Node 3 removed from ISR after lag threshold

  3. **Packet Loss**:
     ```bash
     # 10% packet loss on Node 2
     toxiproxy-cli toxic add chronik2 -t loss_down -a loss=0.1
     ```
     - **Expected**: Occasional Raft message retries, no election

  4. **Cascading Failure**:
     ```bash
     # Kill Node 1, then Node 2 after 5 seconds
     kill -9 <chronik1_pid>
     sleep 5
     kill -9 <chronik2_pid>
     ```
     - **Expected**: Cluster unavailable (quorum lost), no data corruption

- **Tests**:
  - [ ] `tests/integration/raft_fault_injection_test.rs`
  - [ ] Test each scenario above
  - [ ] Verify cluster recovers when toxics removed

### Performance Benchmarks
- **Location**: `crates/chronik-benchmarks/benches/raft_bench.rs`
- **Metrics**:
  - Replication latency (propose → commit)
  - Throughput (messages/sec with 3x replication)
  - Leader election time (under various loads)
- **Run**: `cargo bench --bench raft_bench`

### Jepsen Testing (Future)
- **Goal**: Formal verification of correctness
- **Scope**: Linearizability, no data loss, no duplicate writes
- **Timeline**: Phase 5 or later

---

## Metrics and Monitoring

### Key Metrics (Prometheus)

```
# Raft Health
chronik_raft_leader_count{node_id="1"} 3
chronik_raft_follower_count{node_id="1"} 6
chronik_raft_isr_size{topic="orders", partition="0"} 3

# Replication Lag
chronik_raft_follower_lag_entries{partition="0", follower="2"} 15
chronik_raft_follower_lag_ms{partition="0", follower="2"} 45

# Elections
chronik_raft_election_count{partition="0"} 2
chronik_raft_election_latency_ms{partition="0", quantile="0.99"} 1200

# Commit Latency
chronik_raft_commit_latency_ms{quantile="0.5"} 8
chronik_raft_commit_latency_ms{quantile="0.99"} 35

# Snapshots
chronik_raft_snapshot_count{partition="0"} 5
chronik_raft_snapshot_size_bytes{partition="0"} 67108864  # 64MB
chronik_raft_snapshot_apply_latency_ms{quantile="0.99"} 2500

# Network
chronik_raft_rpc_latency_ms{rpc="AppendEntries", quantile="0.99"} 12
chronik_raft_rpc_errors_total{rpc="AppendEntries", error="timeout"} 3
```

### Grafana Dashboard
- **Location**: `docs/grafana/chronik_raft_dashboard.json`
- **Panels**:
  - Cluster topology (leaders per node)
  - Replication lag (per partition)
  - Election timeline (historical)
  - Commit latency (p50, p95, p99)
  - ISR size over time

---

## Rollout Plan

### Pre-Production Checklist
- [ ] All Phase 4 tests passing
- [ ] Fault injection tests passing (Toxiproxy scenarios)
- [ ] Performance benchmarks meet targets:
  - [ ] Commit latency < 50ms p99
  - [ ] Leader election < 3 seconds
  - [ ] Replication lag < 100ms under normal load
- [ ] Documentation complete:
  - [ ] Deployment guide
  - [ ] Configuration reference
  - [ ] Troubleshooting guide
  - [ ] Migration guide (v1.x → v2.0)

### Staged Rollout
1. **Internal Testing** (1 week):
   - Deploy to internal staging environment
   - Run load tests (1M messages/day)
   - Monitor for memory leaks, crashes

2. **Alpha Release** (v2.0.0-alpha.1, 2 weeks):
   - Release to early adopters (opt-in)
   - Provide migration scripts
   - Gather feedback on stability

3. **Beta Release** (v2.0.0-beta.1, 4 weeks):
   - Release to broader audience
   - Production-ready features (ISR, metrics)
   - Bug fixes from alpha feedback

4. **RC Release** (v2.0.0-rc.1, 2 weeks):
   - Feature freeze, bug fixes only
   - Final performance tuning
   - Documentation review

5. **GA Release** (v2.0.0, TBD):
   - General availability
   - Full production support
   - Announce on Kafka community channels

---

## Risk Assessment

### High Risks
1. **Raft Complexity**
   - **Impact**: High (correctness, data loss)
   - **Mitigation**: Extensive testing, use battle-tested raft-rs, Jepsen testing
   - **Owner**: Lead engineer

2. **Performance Regression**
   - **Impact**: Medium (slower than single-node)
   - **Mitigation**: Benchmarks, profiling, configurable replication (single-node mode)
   - **Owner**: Performance team

3. **Breaking Changes**
   - **Impact**: Medium (existing deployments break)
   - **Mitigation**: Migration guide, backward-compatible WAL, v1.x maintenance branch
   - **Owner**: Product owner

### Medium Risks
4. **Memory Overhead (Many Raft Groups)**
   - **Impact**: Medium (OOM with 1000+ partitions)
   - **Mitigation**: Profile with 1000 partitions, lazy Raft group creation
   - **Owner**: Performance team

5. **Network Partitions**
   - **Impact**: Medium (split-brain, data loss)
   - **Mitigation**: Raft prevents split-brain by design, Toxiproxy testing
   - **Owner**: Test engineer

### Low Risks
6. **Documentation Gaps**
   - **Impact**: Low (adoption slower)
   - **Mitigation**: Comprehensive docs, tutorials, example deployments
   - **Owner**: Documentation lead

---

## Success Criteria

### Phase 1 Success
- [ ] Single partition replicates across 3 nodes
- [ ] Leader election completes in < 2 seconds
- [ ] Zero message loss during leader failover
- [ ] Kafka clients work without modification

### Phase 3 Success
- [ ] 3-node cluster forms automatically from config
- [ ] Topic creation replicates to all nodes
- [ ] Produce/consume works from any node
- [ ] Cluster survives 1-node failure

### Phase 4 Success (Production-Ready)
- [ ] ISR tracking works (remove/re-add followers)
- [ ] Graceful shutdown with leadership transfer
- [ ] 1M messages produced/consumed with zero loss
- [ ] Replication lag < 100ms p99

### v2.0.0 GA Success
- [ ] 10+ production deployments (internal or beta users)
- [ ] 99.9% uptime in production (30 days)
- [ ] No critical bugs in issue tracker
- [ ] Performance ≥ 80% of single-node throughput

---

## Parallelization Strategy

**YES! Development can be highly parallelized.**

### Parallel Work Streams

```
Timeline (Sequential): 21 weeks
Timeline (Parallelized): 11-13 weeks (42% reduction!)

Critical Path: Phase 1 → Phase 2 → Phase 3 → Integration
Parallel Paths: Documentation, Testing Infra, Advanced Features
```

### Team Structure (Recommended)

**Stream A: Core Raft (Critical Path)** - 2 engineers
- Phase 1: Raft foundation
- Phase 2: Multi-partition
- Phase 3: Cluster membership
- **Blocks**: Everything else depends on this

**Stream B: Infrastructure & Testing** - 1 engineer (parallel)
- Toxiproxy setup
- Integration test framework
- Benchmarking harness
- Metrics/monitoring scaffolding
- **Can start**: Day 1 (independent)

**Stream C: Production Features** - 1 engineer (starts Week 4)
- ISR tracking (needs Phase 1 complete)
- Metrics implementation
- S3 bootstrap logic
- **Can start**: After Phase 1 APIs stable

**Stream D: Advanced Features** - 1 engineer (starts Week 6)
- DNS discovery
- Rebalancing algorithms
- Rolling upgrade strategy
- **Can start**: After Phase 2 architecture clear

**Stream E: Documentation** - 1 technical writer (parallel)
- User guides
- Configuration reference
- Troubleshooting docs
- Migration guides
- **Can start**: Day 1 (based on design doc)

### Dependency Graph

```
Week 1-3:
  [Stream A] Phase 1 (CRITICAL PATH)
    ├─ 1.1 Create chronik-raft crate
    ├─ 1.2 Define Raft RPC protocol
    ├─ 1.3 RaftLogStorage trait
    ├─ 1.4 PartitionReplica
    └─ 1.5 E2E single-partition test

  [Stream B] Testing Infrastructure (PARALLEL)
    ├─ Setup Toxiproxy
    ├─ Create test harness
    └─ Benchmark framework

  [Stream E] Documentation (PARALLEL)
    ├─ Deployment guide draft
    └─ Configuration reference

Week 4-5:
  [Stream A] Phase 2 (CRITICAL PATH)
    ├─ 2.1 RaftGroupManager
    ├─ 2.2 Partition assignment
    └─ 2.3-2.5 Multi-partition routing

  [Stream C] Production Features START (depends on 1.4)
    ├─ 4.1 ISR tracking (can design/implement)
    └─ 4.4 Metrics (can implement)

  [Stream B] Testing (PARALLEL)
    └─ Fault injection tests

Week 6-8:
  [Stream A] Phase 3 (CRITICAL PATH)
    ├─ 3.1-3.2 Cluster bootstrap
    └─ 3.3 Metadata replication

  [Stream C] Production Features (PARALLEL)
    ├─ 4.2 Controlled shutdown
    └─ 4.3 S3 bootstrap

  [Stream D] Advanced Features START
    ├─ 5.1 DNS discovery (can implement)
    └─ 5.2 Rebalancing algorithm design

Week 9-11:
  [Integration & Testing] ALL STREAMS CONVERGE
    ├─ Phase 3 E2E test (Stream A)
    ├─ Phase 4 E2E test (Stream C)
    ├─ Fault injection tests (Stream B)
    └─ Performance benchmarks (Stream B)

Week 12-13:
  [Stabilization & Release]
    ├─ Bug fixes
    ├─ Documentation polish
    └─ GA release
```

### Parallelization Details by Phase

#### Phase 1 (Week 1-3) - Limited Parallelization
**Sequential tasks** (on critical path):
- 1.1 → 1.2 → 1.3 → 1.4 → 1.5 (must go in order)

**Parallel tasks**:
- While 1.1-1.4 happening:
  - Stream B: Set up Toxiproxy, test harness
  - Stream E: Write deployment guide skeleton

**Team**: 2 engineers on Stream A, 1 on Stream B, 1 on Stream E

#### Phase 2 (Week 4-5) - Moderate Parallelization
**Sequential tasks**:
- 2.1 (RaftGroupManager) must complete first
- Then 2.2-2.5 can be partially parallelized:
  - Engineer 1: 2.2 (partition assignment) + 2.4 (FetchHandler)
  - Engineer 2: 2.3 (ProduceHandler) + 2.5 (E2E test)

**Parallel tasks**:
- Stream C can START:
  - 4.1 ISR tracking (API from Phase 1 stable)
  - 4.4 Metrics (independent)
- Stream B: Write fault injection tests (ready for Phase 3)

**Team**: 2 on Stream A, 1 on Stream C, 1 on Stream B

#### Phase 3 (Week 6-8) - High Parallelization
**Parallelizable tasks**:
- Engineer 1: 3.1 + 3.2 (cluster bootstrap)
- Engineer 2: 3.3 (metadata Raft)
- Engineer 3: 3.4 (partition assignment)
- Engineer 4: 4.2 + 4.3 (controlled shutdown + S3 bootstrap)
- Engineer 5: 5.1 (DNS discovery)

**Team**: 2 on Stream A, 1 on Stream C, 1 on Stream D, 1 on Stream B (tests)

#### Phase 4 (Week 9-11) - Integration Phase
**Mostly sequential** (integration work):
- 3.5 E2E cluster test (must finish first)
- Then 4.5 production test
- Then fault injection tests

**But can parallelize**:
- Engineer 1: Integration tests
- Engineer 2: Bug fixes from integration
- Engineer 3: Performance tuning
- Engineer 4: Documentation updates

### Minimum Team Sizes

**Option 1: Small Team (3 engineers)**
- Timeline: 15-17 weeks (vs 21 sequential)
- Assignment:
  - 2 engineers: Core Raft (Stream A)
  - 1 engineer: Rotate between B/C/D streams

**Option 2: Optimal Team (5 engineers)**
- Timeline: 11-13 weeks (vs 21 sequential)
- Assignment:
  - 2 engineers: Core Raft (Stream A - critical path)
  - 1 engineer: Testing/Infrastructure (Stream B)
  - 1 engineer: Production features (Stream C)
  - 1 engineer: Advanced features (Stream D)

**Option 3: Large Team (7+ engineers)**
- Timeline: 10-11 weeks (diminishing returns)
- Risk: Coordination overhead, merge conflicts
- Only worth it if GA deadline is critical

### Recommended Approach: 5-Engineer Team

```
┌─────────────────────────────────────────────────────────────────┐
│ Week 1-3: Phase 1 Foundation                                     │
├─────────────────────────────────────────────────────────────────┤
│ Alice & Bob:   Core Raft implementation (1.1-1.5)               │
│ Charlie:       Toxiproxy + test harness                         │
│ Diana:         Metrics scaffolding + monitoring                  │
│ Eve:           Documentation (deployment guide, config ref)      │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Week 4-5: Phase 2 Multi-Partition                                │
├─────────────────────────────────────────────────────────────────┤
│ Alice:         RaftGroupManager + ProduceHandler (2.1, 2.3)     │
│ Bob:           Partition assignment + FetchHandler (2.2, 2.4)   │
│ Charlie:       Fault injection test suite                       │
│ Diana:         ISR tracking implementation (4.1)                 │
│ Eve:           Troubleshooting guide                             │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Week 6-8: Phase 3 Cluster + Phase 4 Production Features         │
├─────────────────────────────────────────────────────────────────┤
│ Alice:         Cluster bootstrap (3.1, 3.2)                     │
│ Bob:           Metadata Raft replication (3.3)                  │
│ Charlie:       Partition assignment + E2E test (3.4, 3.5)       │
│ Diana:         Controlled shutdown + S3 bootstrap (4.2, 4.3)    │
│ Eve:           DNS discovery implementation (5.1)               │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Week 9-11: Integration, Testing, Stabilization                  │
├─────────────────────────────────────────────────────────────────┤
│ Alice & Bob:   Integration testing (3.5, 4.5)                   │
│ Charlie:       Fault injection testing (all scenarios)          │
│ Diana:         Performance benchmarking + tuning                 │
│ Eve:           Migration guide + final docs                      │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Week 12-13: Release Prep                                         │
├─────────────────────────────────────────────────────────────────┤
│ All:           Bug fixes, final testing, GA release              │
└─────────────────────────────────────────────────────────────────┘
```

### Critical Path Analysis

**Critical Path** (cannot be parallelized):
1. Phase 1.1-1.4 (Raft foundation) - 3 weeks
2. Phase 2.1 (RaftGroupManager) - 3 days
3. Phase 3.3 (Metadata Raft) - 5 days
4. Phase 3.5 (E2E cluster test) - 3 days
5. Integration testing - 2 weeks

**Total Critical Path**: ~7-8 weeks (minimum possible timeline)

**With parallelization overhead**: 11-13 weeks (realistic)

### Communication & Coordination

**Daily standups** (15 min):
- What did you finish?
- What are you working on today?
- Any blockers?

**Weekly integration meetings** (1 hour):
- Stream A (Raft): API review for dependent streams
- Demo working features
- Adjust priorities based on progress

**Slack channels**:
- `#chronik-raft-core` (Stream A)
- `#chronik-raft-testing` (Stream B)
- `#chronik-raft-features` (Stream C/D)

**Shared docs**:
- This implementation plan (update weekly)
- API contracts (Stream A publishes for Stream C/D)
- Test results dashboard

### Risk Management (Parallelization-Specific)

**Risk 1: Stream C/D blocked by Stream A API changes**
- **Mitigation**: Stream A publishes stable API contracts early
- **Fallback**: Stream C/D work on prototypes, refactor when API stable

**Risk 2: Merge conflicts (multiple people editing same files)**
- **Mitigation**: Clear ownership (Alice owns `partition_replica.rs`, Bob owns `group_manager.rs`)
- **Process**: Frequent merges (at least daily)

**Risk 3: Integration reveals architectural issues late**
- **Mitigation**: Weekly integration builds (even if incomplete)
- **Process**: Stream A merges to `main` daily, others merge to feature branches

**Risk 4: Testing infrastructure not ready when needed**
- **Mitigation**: Stream B has highest priority after Stream A
- **Fallback**: Manual testing in Phase 1-2, automated in Phase 3

## Timeline Estimate

### Sequential Timeline (1-2 Engineers)

| Phase | Duration | Milestones |
|-------|----------|------------|
| **Phase 1**: Raft Foundation | 3 weeks | Single-partition replication working |
| **Phase 2**: Multi-Partition | 2 weeks | Multiple Raft groups, partition routing |
| **Phase 3**: Cluster Membership | 3 weeks | 3-node cluster, metadata replication |
| **Phase 4**: Production Features | 3 weeks | ISR, metrics, S3 bootstrap |
| **Phase 5**: Advanced Features | 4 weeks | DNS discovery, rebalancing, upgrades |
| **Testing & Stabilization** | 4 weeks | Fault injection, benchmarks, bug fixes |
| **Documentation & Release** | 2 weeks | Guides, tutorials, GA release |
| **Total** | **21 weeks** (~5 months) | v2.0.0 GA |

### Parallelized Timeline (5 Engineers) - RECOMMENDED

| Phase | Sequential | Parallelized | Savings | Notes |
|-------|-----------|--------------|---------|-------|
| **Phase 1**: Raft Foundation | 3 weeks | 3 weeks | 0% | Critical path |
| **Phase 2**: Multi-Partition | 2 weeks | 2 weeks | 0% | Critical path |
| **Phase 3**: Cluster Membership | 3 weeks | 3 weeks | 0% | Critical path |
| **Phase 4**: Production Features | 3 weeks | **0 weeks** | 100% | Parallel with Phase 2-3 |
| **Phase 5**: Advanced Features | 4 weeks | **0 weeks** | 100% | Parallel with Phase 3 |
| **Testing & Stabilization** | 4 weeks | 2 weeks | 50% | Parallel + integration |
| **Documentation & Release** | 2 weeks | **0 weeks** | 100% | Parallel from Day 1 |
| **Total** | **21 weeks** | **11-13 weeks** | **42% faster** | With 5 engineers |

### Parallelized Timeline Milestones (5 Engineers)

**Key Milestones:**
- **Week 3**: Phase 1 complete (alpha.1)
- **Week 5**: Phase 2 complete + ISR tracking + Metrics (alpha.2)
- **Week 8**: Phase 3 complete + Controlled shutdown + DNS discovery (beta.1)
- **Week 11**: All features + testing complete (rc.1)
- **Week 13**: GA release (v2.0.0)

### Parallelized Timeline (3 Engineers)

**Key Milestones:**
- **Week 3**: Phase 1 complete
- **Week 5**: Phase 2 complete
- **Week 8**: Phase 3 complete
- **Week 11**: Phase 4 complete (partial Phase 5)
- **Week 15**: Testing complete
- **Week 17**: GA release (v2.0.0)

---

## Progress Tracking

**Last Updated**: 2025-10-15

### Overall Status
- **Phase 1**: 🔴 Not Started (0/5 tasks)
- **Phase 2**: 🔴 Not Started (0/5 tasks)
- **Phase 3**: 🔴 Not Started (0/5 tasks)
- **Phase 4**: 🔴 Not Started (0/5 tasks)
- **Phase 5**: 🔴 Not Started (0/4 tasks)
- **Overall Progress**: 0% (0/24 major tasks)

### Next Actions
1. [ ] Review this plan with team, get approval
2. [ ] Set up development environment (raft-rs, tonic, toxiproxy)
3. [ ] Start Phase 1.1: Create `chronik-raft` crate

---

## References

### External Resources
- [raft-rs Documentation](https://github.com/tikv/raft-rs)
- [Raft Paper (Diego Ongaro)](https://raft.github.io/raft.pdf)
- [TiKV Architecture](https://tikv.org/deep-dive/introduction/) (multi-Raft example)
- [Kafka Replication Design](https://kafka.apache.org/documentation/#replication)
- [Toxiproxy Guide](https://github.com/Shopify/toxiproxy)

### Internal Documents
- [CLAUDE.md](../CLAUDE.md) - Project overview
- [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) - S3 backup/restore
- [WAL_ARCHITECTURE.md](WAL_ARCHITECTURE.md) - WAL internals
- [LAYERED_STORAGE.md](LAYERED_STORAGE.md) - Storage tiers

---

## Notes

- This plan prioritizes **correctness over speed** - Raft is complex, test thoroughly
- **No shortcuts**: Proper implementation, full testing, complete documentation
- **Fix forward**: If bugs found, fix in next version (never revert)
- **Test with real clients**: Every phase must pass Kafka client tests (kafka-python, KSQLDB)
- **Profile early**: Don't wait until Phase 5 to discover performance issues

---

**Next Review Date**: 2025-10-22 (weekly until Phase 3, then biweekly)
