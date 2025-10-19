# OpenRaft Evaluation for Chronik Clustering

**Date**: October 16, 2025
**Evaluator**: Claude (Anthropic)
**Purpose**: Assess OpenRaft as an alternative to TiKV's raft-rs for Chronik's clustering implementation

---

## Executive Summary

**Recommendation**: **SKIP OpenRaft for now**

While OpenRaft is a well-designed, modern async Raft implementation with excellent API ergonomics and strong production use in Databend, it is **not production-ready** for Chronik due to:

1. **API Instability** - Pre-1.0.0 with breaking changes expected
2. **Incomplete Testing** - No chaos testing or Jepsen validation (yet)
3. **Single-Raft Design** - No built-in multi-raft support (Chronik needs one Raft group per partition)
4. **Migration Risk** - Significant architectural differences from raft-rs require complete rewrite

**Alternative Recommendation**: Stick with **TiKV raft-rs** or wait for OpenRaft 1.0.0+ with chaos testing.

---

## 1. Library Overview

### Latest Version
- **Version**: 0.9.19 (released June 10, 2025)
- **Main Branch**: 0.10 (alpha)
- **Crates.io**: https://crates.io/crates/openraft
- **GitHub**: https://github.com/databendlabs/openraft

### Community Metrics
| Metric | Value | Assessment |
|--------|-------|------------|
| GitHub Stars | 1,700 | Moderate (vs TiKV raft-rs: 3.1k) |
| Forks | 182 | Moderate |
| Contributors | 62 | Good |
| Last Commit | Active (2025) | Excellent |
| Release Frequency | Regular (monthly) | Excellent |

### License
- **Dual-licensed**: MIT OR Apache-2.0
- **Compatibility**: ✅ Fully compatible with Chronik (Apache 2.0)

### Documentation Quality
| Aspect | Rating | Notes |
|--------|--------|-------|
| API Docs (docs.rs) | ⭐⭐⭐⭐⭐ | Comprehensive, well-structured |
| Guide | ⭐⭐⭐⭐ | Good getting started, architecture docs |
| Examples | ⭐⭐⭐⭐⭐ | Excellent (raft-kv-memstore, raft-kv-rocksdb) |
| FAQ | ⭐⭐⭐⭐ | Thorough, addresses common questions |
| Migration Guides | ⭐⭐⭐⭐ | 0.6→0.7, 0.7→0.8, 0.8→0.9 guides available |

---

## 2. Technical Features

### Raft Protocol Compliance
- **Raft Paper Compliance**: ✅ Full (joint consensus, log compaction, snapshots)
- **Extensions**: Dynamic membership, linearizable reads, log streaming
- **Version**: Based on Raft thesis (Diego Ongaro, 2014)

### Async Support (Tokio)
```rust
// OpenRaft is fully async/await native
use openraft::Raft;

let raft = Raft::new(node_id, config, network, storage).await?;

// All operations are async
raft.client_write(request).await?;
let metrics = raft.metrics().borrow().clone();
```

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent**
- Fully reactive, event-driven (no tick-based loops like raft-rs)
- Native tokio integration
- No blocking operations in core logic

### Storage Abstraction

OpenRaft provides three core traits:

```rust
#[async_trait]
pub trait RaftLogReader<C: RaftTypeConfig>: Send + Sync + 'static {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<C>>, StorageError<C>>;

    async fn read_vote(&mut self) -> Result<Option<Vote<C::NodeId>>, StorageError<C>>;
}

#[async_trait]
pub trait RaftLogStorage<C: RaftTypeConfig>: RaftLogReader<C> {
    async fn save_vote(&mut self, vote: &Vote<C::NodeId>) -> Result<(), StorageError<C>>;

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<C>)
        -> Result<(), StorageError<C>>
    where I: IntoIterator<Item = Entry<C>> + Send;

    async fn truncate(&mut self, log_id: LogId<C::NodeId>) -> Result<(), StorageError<C>>;
    async fn purge(&mut self, log_id: LogId<C::NodeId>) -> Result<(), StorageError<C>>;
}

#[async_trait]
pub trait RaftStateMachine<C: RaftTypeConfig>: Send + Sync + 'static {
    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Response<C>>, StorageError<C>>
    where I: IntoIterator<Item = Entry<C>> + Send;

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder;
    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Self::Snapshot>, StorageError<C>>;
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<C::NodeId, C::Node>,
        snapshot: Box<Self::Snapshot>,
    ) -> Result<(), StorageError<C>>;
}
```

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent**
- Clean, well-designed trait separation
- Easy to implement custom storage (RocksDB, SQLite, etc.)
- Built-in examples: `openraft-memstore`, `openraft-rocksstore`, `openraft-sledstore`

**Chronik Integration Path**:
```rust
// Hypothetical Chronik WAL integration
struct ChronikWalStorage {
    wal: Arc<WalManager>,
    // ... metadata, state machine
}

#[async_trait]
impl RaftLogStorage<TypeConfig> for ChronikWalStorage {
    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>)
        -> Result<(), StorageError<TypeConfig>> {
        for entry in entries {
            // Write to Chronik WAL
            self.wal.append_record(entry.into()).await?;
        }
        callback.log_io_completed(Ok(()));
        Ok(())
    }
    // ... other methods
}
```

### Network Layer

OpenRaft uses a "bring-your-own-network" design:

```rust
#[async_trait]
pub trait RaftNetwork<C: RaftTypeConfig>: Send + Sync + 'static {
    async fn send_append_entries(
        &mut self,
        target: C::NodeId,
        rpc: AppendEntriesRequest<C>,
    ) -> Result<AppendEntriesResponse<C>, RPCError<C::NodeId, C::Node>>;

    async fn send_install_snapshot(
        &mut self,
        target: C::NodeId,
        rpc: InstallSnapshotRequest<C>,
    ) -> Result<InstallSnapshotResponse<C>, RPCError<C::NodeId, C::Node>>;

    async fn send_vote(
        &mut self,
        target: C::NodeId,
        rpc: VoteRequest<C>,
    ) -> Result<VoteResponse<C>, RPCError<C::NodeId, C::Node>>;
}

pub trait RaftNetworkFactory<C: RaftTypeConfig>: Send + Sync + 'static {
    type Network: RaftNetwork<C>;

    async fn new_client(&mut self, target: C::NodeId, node: &C::Node) -> Self::Network;
}
```

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent**
- No forced dependencies (HTTP, gRPC, WebSocket - your choice)
- Perfect for Chronik's gRPC/Tonic stack
- Example: Databend uses Tonic gRPC (see `databend/src/meta`)

**Chronik Integration Example** (with Tonic):
```rust
use tonic::transport::Channel;

struct ChronikRaftNetwork {
    client: RaftServiceClient<Channel>,
}

#[async_trait]
impl RaftNetwork<TypeConfig> for ChronikRaftNetwork {
    async fn send_append_entries(
        &mut self,
        target: NodeId,
        rpc: AppendEntriesRequest<TypeConfig>,
    ) -> Result<AppendEntriesResponse<TypeConfig>, RPCError> {
        let request = tonic::Request::new(rpc.into());
        let response = self.client.append_entries(request).await?;
        Ok(response.into_inner().into())
    }
    // ... other methods
}
```

### Snapshot Support
- **Manual Trigger**: `Raft::trigger_snapshot().await`
- **Automatic Compaction**: Configurable via `SnapshotPolicy`
- **Streaming**: ✅ Leader→Follower snapshot streaming
- **Incremental**: ❌ No (full snapshots only)

**Code Example**:
```rust
// Configure snapshot policy
let config = Config {
    snapshot_policy: SnapshotPolicy::LogsSinceLast(10_000),
    // Snapshot every 10k log entries
    ..Default::default()
};

// Manual snapshot trigger
raft.trigger_snapshot().await?;
```

**Assessment**: ⭐⭐⭐⭐ **Good** (but lacks incremental snapshots)

### Dynamic Membership Changes
- **Algorithm**: Joint consensus (Raft paper §6)
- **Single-step**: ❌ NOT supported (intentionally - buggy algorithm)
- **API**:
  ```rust
  // Add learner (non-voting)
  raft.add_learner(node_id, node, blocking).await?;

  // Change membership (joint consensus)
  let membership = Membership::new(vec![btreeset![1, 2, 3]], None);
  raft.change_membership(membership, retain).await?;
  ```

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent** (safe, well-designed)

### Linearizable Reads
```rust
// Ensure leadership and wait for applied state
raft.ensure_linearizable().await?;

// Now safe to read from state machine
let value = state_machine.get(key).await?;
```

**Implementation**:
- Sends heartbeats to quorum
- Waits for state machine to apply up to ReadIndex
- Blocks until linearizability guaranteed

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent** (correct, easy to use)

### Performance Characteristics

#### Benchmark Results (OpenRaft Framework - NOT Real-World)
| Clients | Throughput | Latency (avg) | Config |
|---------|-----------|---------------|--------|
| 1 | 70,000 ops/s | 14.3 μs | Single writer |
| 64 | 730,000 ops/s | 1.37 μs | Parallel writes |
| 256 | 1,014,000 ops/s | 0.98 μs | Max throughput |

**Test Conditions**:
- 3-node cluster on single server
- In-memory store (no disk I/O)
- No network (local channels)
- ⚠️ **NOT a real-world benchmark**

**Real-World Production** (Databend user report):
- ~17,000 inserts/s with on-disk SQLite state machine + RocksDB log store
- 40ms blocking issues reported under heavy write load (GitHub discussion #1170)

**Assessment**: ⭐⭐⭐ **Moderate**
- Framework benchmarks look great (1M ops/s)
- Real-world performance unknown for Chronik's workload
- Concerns about write latency under load

---

## 3. Production Readiness

### Known Production Users
| Organization | Use Case | Scale |
|--------------|----------|-------|
| **Databend** | Meta-service cluster (consensus engine) | Production (cloud data warehouse) |
| **CnosDB** | Time-series database clustering | Production |
| **RobustMQ** | Message queue consensus | Unknown |
| **Helyim** | SeaweedFS in Rust | Unknown |
| **Microsoft** | (Mentioned but no details) | Unknown |
| **Huobi** | (Mentioned but no details) | Unknown |

**Assessment**: ⭐⭐⭐ **Moderate**
- Proven in Databend production
- Limited public battle stories
- No large-scale (>1000 node) deployments documented

### Battle-Tested Status
| Aspect | Status | Evidence |
|--------|--------|----------|
| Unit Tests | ✅ 92% coverage | High |
| Integration Tests | ✅ Available | Examples in tests/ |
| Chaos Tests | ❌ **NOT COMPLETE** | OpenRaft docs admit this |
| Jepsen Testing | ❌ **NONE** | No public Jepsen results |
| Formal Verification | ❌ **NONE** | No TLA+ spec |
| Production Time | ~2 years | Databend since ~2022 |

**Critical Gap**: ⚠️ **NO CHAOS TESTING OR JEPSEN VALIDATION**

From OpenRaft docs:
> "The chaos test has not yet been completed, and further testing is needed to ensure the application's robustness and reliability."

**Comparison to TiKV raft-rs**:
- TiKV raft-rs: ✅ Jepsen tested, 5+ years in production, 100+ nodes at scale
- OpenRaft: ❌ No chaos testing, 2 years production, smaller deployments

**Assessment**: ⭐⭐⭐ **Moderate** (not ready for mission-critical systems)

### Test Coverage
- **Unit Tests**: 92% (excellent)
- **Protocol Tests**: ✅ Extensive (log replication, leader election, etc.)
- **Cluster Tests**: ✅ Available in examples/
- **Fault Injection**: ❌ Incomplete
- **Fuzz Testing**: ❌ None documented

### API Stability
**CRITICAL ISSUE**: ⚠️ **API NOT STABLE (Pre-1.0.0)**

From OpenRaft docs:
> "Openraft API is not stable yet. Before 1.0.0, an upgrade may contain incompatible changes."

**Version History**:
- 0.6.x → 0.7.x: Breaking changes
- 0.7.x → 0.8.x: Breaking changes
- 0.8.x → 0.9.x: Breaking changes (made `Raft::new()` async)
- 0.9.x → 0.10.x: In progress (alpha)

**Commit Prefixes** (from changelog):
- `DataChange:` - On-disk data format changes (requires migration)
- `Change:` - Incompatible API changes
- `Feature:` - Compatible new features
- `Fix:` - Bug fixes

**Assessment**: ⭐⭐ **Poor** (too risky for production Chronik)

---

## 4. Integration Complexity

### API Ergonomics (Async/Await Friendly)
```rust
// Clean, intuitive API
let raft = Raft::new(node_id, config, network, storage).await?;

// Write data
let response = raft.client_write(WriteRequest { key, value }).await?;

// Linearizable read
raft.ensure_linearizable().await?;
let data = state_machine.get(key).await?;

// Metrics
let metrics = raft.metrics().borrow().clone();
println!("Leader: {:?}, Term: {}", metrics.current_leader, metrics.current_term);
```

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent** (idiomatic Rust, async-first)

### Learning Curve
| Aspect | Difficulty | Notes |
|--------|-----------|-------|
| Basic Usage | Easy | Excellent docs, examples |
| Custom Storage | Moderate | Trait impl requires understanding log/state machine split |
| Custom Network | Moderate | Well-documented, but RPC design is on you |
| Debugging | Hard | Raft debugging is inherently complex |
| Migration from raft-rs | **Very Hard** | Complete architectural rewrite |

**Assessment**: ⭐⭐⭐⭐ **Good** (for new projects; hard for migrations)

### Example Code Quality
**raft-kv-memstore** (in-memory KV store):
- ✅ Production-quality structure
- ✅ Actix-web server example
- ✅ Client library
- ✅ Cluster management APIs
- ✅ Well-commented

**raft-kv-rocksdb** (persistent KV store):
- ✅ RocksDB integration
- ✅ On-disk snapshots
- ✅ Recovery testing

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent**

### Migration Effort from TiKV raft-rs

**Architectural Differences**:
| Aspect | TiKV raft-rs | OpenRaft |
|--------|--------------|----------|
| **Execution Model** | Tick-based (sync) | Event-driven (async) |
| **API Style** | Callback-based | Async/await |
| **Storage Trait** | `Storage` (single trait) | `RaftLogStorage` + `RaftStateMachine` (split) |
| **Network** | BYO (protobuf-based) | BYO (trait-based) |
| **Node Creation** | `RawNode::new()` | `Raft::new().await` |
| **Ticking** | Manual `tick()` calls | Automatic (event-driven) |

**Migration Steps**:
1. **Rewrite storage layer** (raft-rs `Storage` → OpenRaft `RaftLogStorage` + `RaftStateMachine`)
2. **Rewrite network layer** (implement `RaftNetwork` trait for Tonic gRPC)
3. **Remove tick() loop** (OpenRaft is event-driven)
4. **Convert to async/await** (all operations are async)
5. **Rewrite state management** (different state machine model)
6. **Add snapshot handling** (different API)
7. **Migrate membership changes** (different API - joint consensus)
8. **Testing** (comprehensive re-testing required)

**Estimated Effort**: **3-4 weeks** (full-time, experienced Rust developer)

**Assessment**: ⭐⭐ **High Complexity** (almost a complete rewrite)

### Build Dependencies
```toml
[dependencies]
openraft = { version = "0.9", features = ["serde"] }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"

# Optional (for examples)
tonic = "0.12"  # gRPC
rocksdb = "0.22"  # Persistent storage
serde = { version = "1", features = ["derive"] }
```

**No protoc required** (unlike TiKV raft-rs which uses protobuf)

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent** (minimal, pure Rust)

---

## 5. Chronik-Specific Fit

### Multi-Raft Support (One Group Per Partition)

**CRITICAL GAP**: ❌ **OpenRaft has NO built-in multi-raft support**

OpenRaft is a **single Raft group** library. To support multiple partitions:

**Option 1: Manual Multi-Raft** (like TiKV)
```rust
struct MultiRaftManager {
    raft_groups: HashMap<PartitionId, Arc<Raft<TypeConfig>>>,
    // Manage lifecycle, routing, etc.
}

impl MultiRaftManager {
    async fn route_request(&self, partition: PartitionId, request: Request) {
        let raft = self.raft_groups.get(&partition).ok_or(...)?;
        raft.client_write(request).await?;
    }

    async fn add_partition(&mut self, partition: PartitionId) {
        let raft = Raft::new(node_id, config, network, storage).await?;
        self.raft_groups.insert(partition, Arc::new(raft));
    }
}
```

**Complexity**:
- ⚠️ Manual routing per partition
- ⚠️ Separate storage per Raft group
- ⚠️ Network multiplexing (distinguish partition in RPCs)
- ⚠️ Resource management (file handles, memory per group)
- ⚠️ Snapshot coordination across groups

**Option 2: Wait for Multi-Raft Support** (future work)
- OpenRaft roadmap doesn't mention multi-raft
- Would need custom implementation

**TiKV Approach** (for reference):
- TiKV built a custom multi-raft layer on top of raft-rs
- Raft Engine: log-structured storage for multiple Raft groups
- Took years to stabilize

**Assessment**: ⭐⭐ **Poor** (major implementation effort required)

### gRPC Compatibility (Tonic)

**Excellent Fit** - OpenRaft is protocol-agnostic:

```rust
// Chronik Tonic gRPC service
use tonic::{Request, Response, Status};
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse};

#[tonic::async_trait]
impl RaftService for RaftServiceImpl {
    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequestProto>,
    ) -> Result<Response<AppendEntriesResponseProto>, Status> {
        let req: AppendEntriesRequest<TypeConfig> = request.into_inner().try_into()?;

        let network = self.network_factory.new_client(target_node).await;
        let resp = network.send_append_entries(target, req).await?;

        Ok(Response::new(resp.into()))
    }
}
```

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent** (natural fit)

### WAL Integration Feasibility

**Good News**: OpenRaft's storage traits are flexible:

```rust
struct ChronikRaftLogStorage {
    wal_manager: Arc<WalManager>,
    partition_id: i32,
}

#[async_trait]
impl RaftLogStorage<TypeConfig> for ChronikRaftLogStorage {
    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>)
        -> Result<(), StorageError<TypeConfig>>
    where I: IntoIterator<Item = Entry<TypeConfig>> + Send {
        for entry in entries {
            // Convert OpenRaft Entry to Chronik WalRecord
            let wal_record = WalRecord::new_v2(
                entry.log_id.index,
                self.partition_id,
                entry.payload.into(),
            );

            // Append to Chronik WAL
            self.wal_manager.append_record(wal_record).await?;
        }

        // Fsync for durability
        self.wal_manager.flush().await?;

        // Notify OpenRaft completion
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<TypeConfig>> {
        // Truncate Chronik WAL
        self.wal_manager.truncate_after(log_id.index).await?;
        Ok(())
    }

    // ... other methods
}
```

**Challenges**:
- OpenRaft log format vs Chronik's `CanonicalRecord` format
- Double-WAL overhead (Raft log + Kafka message WAL)?
- Snapshot coordination with S3 tiered storage

**Assessment**: ⭐⭐⭐⭐ **Good** (doable, but requires careful design)

### Async Ecosystem Fit (Tokio, Tonic)

**Perfect Match**:
```rust
// Chronik is already tokio + tonic
#[tokio::main]
async fn main() {
    // OpenRaft fits naturally
    let raft = Raft::new(node_id, config, network, storage).await?;

    // Tonic gRPC server
    let raft_service = RaftServiceImpl::new(raft.clone());
    Server::builder()
        .add_service(RaftServiceServer::new(raft_service))
        .serve(addr)
        .await?;
}
```

**Assessment**: ⭐⭐⭐⭐⭐ **Excellent**

---

## 6. Pros and Cons

### Pros
| Pro | Impact | Notes |
|-----|--------|-------|
| ✅ **Async/Await Native** | High | Perfect fit for Chronik's async stack |
| ✅ **Clean API** | High | Intuitive, idiomatic Rust |
| ✅ **Excellent Documentation** | High | Docs, examples, guides all top-tier |
| ✅ **Flexible Storage** | High | Easy to integrate with Chronik WAL |
| ✅ **BYO Network** | High | Natural fit for Tonic gRPC |
| ✅ **Event-Driven** | Medium | Better performance than tick-based |
| ✅ **Linearizable Reads** | Medium | Easy API for strong consistency |
| ✅ **Dynamic Membership** | Medium | Safe joint consensus algorithm |
| ✅ **Apache 2.0** | Low | License compatible |
| ✅ **Active Development** | Medium | Regular releases, responsive maintainers |

### Cons
| Con | Impact | Severity |
|-----|--------|----------|
| ❌ **API Unstable (Pre-1.0)** | **CRITICAL** | 🔴 BLOCKER |
| ❌ **No Chaos Testing** | **CRITICAL** | 🔴 BLOCKER |
| ❌ **No Jepsen Validation** | **CRITICAL** | 🔴 BLOCKER |
| ❌ **No Multi-Raft Support** | **HIGH** | 🟠 Major work required |
| ❌ **Limited Production Proof** | HIGH | 🟠 Risk unknown |
| ❌ **Migration Complexity** | HIGH | 🟠 3-4 weeks effort |
| ⚠️ **No Formal Verification** | Medium | 🟡 Less confidence |
| ⚠️ **Unknown Chronik Fit** | Medium | 🟡 Kafka semantics unclear |
| ⚠️ **Double WAL Overhead?** | Medium | 🟡 Design challenge |
| ⚠️ **Smaller Community** | Low | 🟡 Less support |

---

## 7. Production Readiness Score

| Category | Score | Weight | Notes |
|----------|-------|--------|-------|
| **Correctness** | 6/10 | 30% | No chaos testing, no Jepsen |
| **Stability** | 4/10 | 25% | Pre-1.0, breaking changes |
| **Performance** | 7/10 | 15% | Good benchmarks, but unproven at scale |
| **Documentation** | 9/10 | 10% | Excellent |
| **Production Use** | 6/10 | 10% | Databend only major user |
| **Community** | 6/10 | 5% | Moderate stars, active |
| **Maintainability** | 8/10 | 5% | Clean code, good tests |

**Weighted Score**: **6.1 / 10**

**Production Readiness**: ⚠️ **NOT READY for mission-critical systems**

**Assessment**:
- ✅ Good for **experimental** or **low-stakes** projects
- ⚠️ Risky for **Chronik production** (financial data, zero-loss guarantee)
- 🔴 Wait for **1.0.0 + chaos testing** before production use

---

## 8. Integration Effort Estimate

### Phase 1: Proof of Concept (1 week)
- [ ] Implement `RaftLogStorage` trait with in-memory store
- [ ] Implement `RaftStateMachine` trait for simple KV
- [ ] Implement `RaftNetwork` trait with Tonic gRPC
- [ ] Single Raft group (1 partition)
- [ ] Basic produce/fetch through Raft

**Deliverable**: Single-partition Raft cluster with Kafka compatibility

### Phase 2: WAL Integration (1 week)
- [ ] Integrate `RaftLogStorage` with Chronik WAL
- [ ] Handle log compaction + snapshots
- [ ] Test crash recovery
- [ ] Benchmark write throughput

**Deliverable**: Durable Raft with Chronik WAL backend

### Phase 3: Multi-Raft (2 weeks)
- [ ] Design multi-raft manager (partition routing)
- [ ] Implement per-partition Raft groups
- [ ] Multiplexed gRPC network layer
- [ ] Resource management (file handles, memory)
- [ ] Snapshot coordination across groups

**Deliverable**: Multi-partition Raft (Kafka-like)

### Phase 4: Production Hardening (2 weeks)
- [ ] Chaos testing (Jepsen-style)
- [ ] Performance benchmarking
- [ ] Monitoring/metrics
- [ ] Failure mode testing
- [ ] Load testing

**Total Estimate**: **6-8 weeks** (experienced Rust dev, full-time)

**Risk**: 🔴 **High** (API instability, unknown failure modes)

---

## 9. Code Examples

### Example 1: Basic Raft Setup

```rust
use openraft::{Config, Raft};
use std::sync::Arc;

// Define your type configuration
openraft::declare_raft_types!(
    pub TypeConfig:
        D = Request,           // Client request type
        R = Response,          // Client response type
        NodeId = u64,          // Node ID type
        Node = NodeInfo,       // Node metadata
        Entry = Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
);

#[tokio::main]
async fn main() -> Result<()> {
    // Configure Raft
    let config = Config {
        heartbeat_interval: 500,
        election_timeout_min: 1500,
        election_timeout_max: 3000,
        snapshot_policy: SnapshotPolicy::LogsSinceLast(10_000),
        ..Default::default()
    };

    // Create storage and network
    let storage = Arc::new(MemStorage::new());
    let network = Arc::new(NetworkFactory::new());

    // Initialize Raft
    let raft = Raft::new(
        node_id,
        Arc::new(config),
        network,
        storage.clone(),
    ).await?;

    // Write data (linearizable)
    let request = Request::Set { key: "foo".into(), value: "bar".into() };
    let response = raft.client_write(request).await?;
    println!("Write response: {:?}", response);

    // Read data (linearizable)
    raft.ensure_linearizable().await?;
    let value = storage.get("foo").await?;
    println!("Read value: {:?}", value);

    Ok(())
}
```

### Example 2: Implementing RaftLogStorage with Chronik WAL

```rust
use openraft::storage::{RaftLogStorage, RaftLogReader, LogState};
use chronik_wal::WalManager;

pub struct ChronikWalStorage {
    wal: Arc<WalManager>,
    partition_id: i32,
    state: Arc<RwLock<StorageState>>,
}

#[async_trait]
impl RaftLogReader<TypeConfig> for ChronikWalStorage {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<TypeConfig>> {
        let start = match range.start_bound() {
            Bound::Included(&x) => x,
            Bound::Excluded(&x) => x + 1,
            Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            Bound::Included(&x) => x + 1,
            Bound::Excluded(&x) => x,
            Bound::Unbounded => u64::MAX,
        };

        // Read from Chronik WAL
        let records = self.wal.read_range(start, end).await?;

        // Convert WalRecord to Raft Entry
        let entries = records
            .into_iter()
            .map(|record| Entry::from_wal_record(record))
            .collect();

        Ok(entries)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<TypeConfig>> {
        let state = self.state.read().await;
        Ok(state.vote.clone())
    }
}

#[async_trait]
impl RaftLogStorage<TypeConfig> for ChronikWalStorage {
    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<TypeConfig>> {
        let mut state = self.state.write().await;
        state.vote = Some(vote.clone());

        // Persist vote to WAL metadata
        self.wal.save_metadata("vote", bincode::serialize(vote)?).await?;
        Ok(())
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>)
        -> Result<(), StorageError<TypeConfig>>
    where I: IntoIterator<Item = Entry<TypeConfig>> + Send {
        for entry in entries {
            let wal_record = WalRecord::new_v2(
                entry.log_id.index,
                self.partition_id,
                entry.payload.into_bytes(),
            );

            self.wal.append_record(wal_record).await?;
        }

        // Fsync for durability
        self.wal.flush().await?;

        // Notify OpenRaft
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<TypeConfig>> {
        self.wal.truncate_after(log_id.index).await?;
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<TypeConfig>> {
        self.wal.purge_before(log_id.index).await?;
        Ok(())
    }
}
```

### Example 3: Tonic gRPC Network Layer

```rust
use tonic::{transport::Channel, Request, Response, Status};
use openraft::network::{RaftNetwork, RaftNetworkFactory};

pub struct TonicRaftNetwork {
    target: u64,
    client: RaftServiceClient<Channel>,
}

#[async_trait]
impl RaftNetwork<TypeConfig> for TonicRaftNetwork {
    async fn send_append_entries(
        &mut self,
        target: u64,
        rpc: AppendEntriesRequest<TypeConfig>,
    ) -> Result<AppendEntriesResponse<TypeConfig>, RPCError<u64, NodeInfo>> {
        let request = Request::new(rpc.into());

        let response = self.client
            .append_entries(request)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        Ok(response.into_inner().try_into()?)
    }

    async fn send_install_snapshot(
        &mut self,
        target: u64,
        rpc: InstallSnapshotRequest<TypeConfig>,
    ) -> Result<InstallSnapshotResponse<TypeConfig>, RPCError<u64, NodeInfo>> {
        let request = Request::new(rpc.into());

        let response = self.client
            .install_snapshot(request)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        Ok(response.into_inner().try_into()?)
    }

    async fn send_vote(
        &mut self,
        target: u64,
        rpc: VoteRequest<TypeConfig>,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<u64, NodeInfo>> {
        let request = Request::new(rpc.into());

        let response = self.client
            .vote(request)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;

        Ok(response.into_inner().try_into()?)
    }
}

pub struct TonicNetworkFactory {
    nodes: Arc<RwLock<HashMap<u64, NodeInfo>>>,
}

#[async_trait]
impl RaftNetworkFactory<TypeConfig> for TonicNetworkFactory {
    type Network = TonicRaftNetwork;

    async fn new_client(&mut self, target: u64, node: &NodeInfo) -> Self::Network {
        let channel = Channel::from_shared(node.addr.clone())
            .unwrap()
            .connect()
            .await
            .unwrap();

        TonicRaftNetwork {
            target,
            client: RaftServiceClient::new(channel),
        }
    }
}
```

### Example 4: Multi-Raft Manager (for Chronik Partitions)

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct MultiRaftManager {
    /// Map: PartitionId -> Raft instance
    raft_groups: Arc<RwLock<HashMap<i32, Arc<Raft<TypeConfig>>>>>,

    /// Shared network factory
    network_factory: Arc<TonicNetworkFactory>,

    /// Shared config
    config: Arc<Config>,
}

impl MultiRaftManager {
    pub async fn new(config: Config, network_factory: TonicNetworkFactory) -> Self {
        Self {
            raft_groups: Arc::new(RwLock::new(HashMap::new())),
            network_factory: Arc::new(network_factory),
            config: Arc::new(config),
        }
    }

    /// Create a new Raft group for a partition
    pub async fn add_partition(&self, partition_id: i32, node_id: u64) -> Result<()> {
        let storage = Arc::new(ChronikWalStorage::new(partition_id));

        let raft = Raft::new(
            node_id,
            self.config.clone(),
            self.network_factory.clone(),
            storage,
        ).await?;

        let mut groups = self.raft_groups.write().await;
        groups.insert(partition_id, Arc::new(raft));

        Ok(())
    }

    /// Route a write to the appropriate partition
    pub async fn write(&self, partition_id: i32, request: Request) -> Result<Response> {
        let groups = self.raft_groups.read().await;

        let raft = groups
            .get(&partition_id)
            .ok_or_else(|| anyhow!("Partition {} not found", partition_id))?;

        let response = raft.client_write(request).await?;
        Ok(response)
    }

    /// Linearizable read from partition
    pub async fn read(&self, partition_id: i32, key: &str) -> Result<Option<String>> {
        let groups = self.raft_groups.read().await;

        let raft = groups
            .get(&partition_id)
            .ok_or_else(|| anyhow!("Partition {} not found", partition_id))?;

        // Ensure linearizability
        raft.ensure_linearizable().await?;

        // Read from state machine
        let storage = raft.storage();
        storage.get(key).await
    }

    /// Get metrics for all partitions
    pub async fn metrics(&self) -> HashMap<i32, RaftMetrics<u64, NodeInfo>> {
        let groups = self.raft_groups.read().await;

        groups
            .iter()
            .map(|(&partition_id, raft)| {
                let metrics = raft.metrics().borrow().clone();
                (partition_id, metrics)
            })
            .collect()
    }
}
```

---

## 10. Final Recommendation

### Decision: **SKIP OpenRaft for Chronik v1.4**

**Reasoning**:

1. **API Instability** (🔴 BLOCKER)
   - Pre-1.0.0 with breaking changes
   - Risk of migration burden across versions
   - Not acceptable for production Chronik

2. **No Chaos Testing** (🔴 BLOCKER)
   - OpenRaft admits chaos testing is incomplete
   - No Jepsen validation
   - Chronik requires zero-loss guarantee
   - **Cannot trust correctness under failure**

3. **No Multi-Raft Support** (🟠 MAJOR)
   - Chronik needs one Raft group per partition
   - Would require building multi-raft layer ourselves
   - TiKV took years to stabilize their multi-raft implementation
   - Too much risk and effort

4. **Limited Production Proof** (🟠 MAJOR)
   - Only Databend as major user
   - No public large-scale deployments
   - Unknown failure modes at scale

### Alternative Path: **Stick with TiKV raft-rs**

**Rationale**:
- ✅ Battle-tested (5+ years in production)
- ✅ Jepsen validated
- ✅ Proven at scale (TiKV, TiDB)
- ✅ API stable (mature project)
- ✅ Multi-raft patterns documented (Raft Engine, Multi-Raft)
- ⚠️ Sync/tick-based (less ergonomic than OpenRaft)
- ⚠️ Requires manual async wrappers

**Chronik Integration with raft-rs**:
```rust
// Wrap raft-rs in async task
let (tx, rx) = mpsc::channel();

tokio::spawn(async move {
    loop {
        select! {
            msg = rx.recv() => {
                // Handle raft messages
                raw_node.step(msg)?;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                // Tick
                raw_node.tick();
            }
        }

        // Process ready
        let ready = raw_node.ready();
        // ... handle ready state
        raw_node.advance(ready);
    }
});
```

### When to Reconsider OpenRaft

**Criteria for Future Adoption**:
1. ✅ OpenRaft 1.0.0+ released (API stable)
2. ✅ Chaos testing completed
3. ✅ Jepsen validation passed
4. ✅ 2+ major production users at scale
5. ✅ Multi-raft patterns documented/supported
6. ✅ 12+ months of 1.0 stability

**Timeline**: Likely **2026 or later**

---

## 11. References

### Official Resources
- **Crates.io**: https://crates.io/crates/openraft
- **GitHub**: https://github.com/databendlabs/openraft
- **Docs**: https://docs.rs/openraft/latest/openraft/
- **Guide**: https://databendlabs.github.io/openraft/
- **Examples**: https://github.com/databendlabs/openraft/tree/main/examples

### Production Users
- **Databend**: https://github.com/databendlabs/databend
  - Meta-service: `databend/src/meta/raft-store/`
- **CnosDB**: https://github.com/cnosdb/cnosdb
- **RobustMQ**: https://github.com/robustmq/robustmq

### Comparison Resources
- **TiKV raft-rs**: https://github.com/tikv/raft-rs
- **Async-Raft** (OpenRaft predecessor): https://github.com/async-raft/async-raft
- **Raft Paper**: https://raft.github.io/raft.pdf

### Related Reading
- **TiKV Multi-Raft**: https://tikv.org/deep-dive/scalability/multi-raft/
- **Raft Engine**: https://www.infoq.com/articles/raft-engine-tikv-database/
- **Building Distributed Systems on Raft**: https://tikv.org/blog/building-distributed-storage-system-on-raft/

---

**Report Compiled**: October 16, 2025
**Evaluator**: Claude (Anthropic)
**Next Steps**: Present to Chronik team for decision
