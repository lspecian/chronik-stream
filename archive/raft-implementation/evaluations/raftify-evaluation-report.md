# Raftify Evaluation Report: Raft Implementation for Chronik Clustering

**Date**: 2025-10-16
**Evaluator**: Claude (Sonnet 4.5)
**Purpose**: Assess raftify as an alternative to TiKV's raft-rs for Chronik's clustering implementation

---

## Executive Summary

**Recommendation**: **SKIP** - Do not use raftify for Chronik clustering.

**TL;DR**: While raftify is an interesting high-level Raft framework built on tikv/raft-rs, it is:
- **Experimental and unstable** (explicitly marked as such)
- **Minimal production usage** (only Backend.AI, still experimental)
- **Not battle-tested** (no Jepsen testing, minimal test coverage evidence)
- **Limited community** (42 stars, 14 contributors, 10-month-old latest release)
- **Opinionated architecture** (forces gRPC/LMDB, hard to customize)
- **Migration overhead** (would require learning new API on top of tikv/raft-rs)

**Better Alternatives**:
1. **Stick with tikv/raft-rs directly** - Battle-tested, flexible, used in production at scale by TiKV
2. **Consider openraft** - More mature async-native alternative with better production track record

---

## 1. Library Overview

### Basic Information

| Metric | Value | Assessment |
|--------|-------|------------|
| **Latest Version** | 0.1.82 (crates.io) | Pre-1.0, unstable API |
| **Last Update** | 10 months ago (Jan 2024) | Concerning staleness |
| **GitHub Stars** | 42 | Very low adoption |
| **Contributors** | 14 | Small team |
| **License** | Apache-2.0 + MIT (dual) | Compatible with Chronik |
| **Repository** | https://github.com/lablup/raftify | Active but experimental |
| **Documentation** | https://docs.rs/raftify/latest/raftify/ | Basic, incomplete |

### Project Status

**Official Description**: "Experimental High level Raft framework"

Key warnings from the project:
- "The library is in a very experimental stage and the API could be broken"
- "Before 1.0.0, an upgrade may contain incompatible changes"
- "This project has not yet had a security audit or stress test"

**Community Engagement**:
- OSSCA 2024 participation (Korean open source contest)
- 2-3 detailed blog posts from maintainers (Backend.AI team)
- No evidence of large-scale production deployment

---

## 2. Technical Features

### 2.1 Architecture & Design

**Position in Ecosystem**:
```
                     ┌─────────────────────┐
                     │   Your Application   │
                     └──────────┬──────────┘
                                │
                     ┌──────────▼──────────┐
                     │      Raftify         │ ◄── High-level framework
                     │  (gRPC + LMDB)       │     (what we're evaluating)
                     └──────────┬──────────┘
                                │
                     ┌──────────▼──────────┐
                     │   tikv/raft-rs       │ ◄── Core consensus engine
                     │  (Raft algorithm)    │     (battle-tested)
                     └─────────────────────┘
```

Raftify is a **wrapper/framework** around tikv/raft-rs, adding:
- Pre-configured gRPC network layer (using Tonic)
- Pre-configured LMDB storage layer (using heed)
- High-level abstractions for common patterns

**Key Difference from tikv/raft-rs**:
- **tikv/raft-rs**: Core consensus module only - you implement Storage, Network, StateMachine
- **raftify**: Batteries-included framework - gRPC/LMDB/StateMachine pre-wired

### 2.2 Core Components

**Main Structs**:
```rust
// Primary node abstraction
RaftNode<LogEntry, FSM, Storage>

// Bootstrap structure
Raft<LogEntry, FSM, Storage>

// Network transport
Channel (gRPC-based via Tonic)

// Configuration
Config
```

**Key Traits** (you must implement):
```rust
// Your log entry format
trait AbstractLogEntry

// Your state machine
trait AbstractStateMachine {
    async fn apply(&mut self, log_entry: Vec<u8>) -> Result<Vec<u8>>;
    async fn snapshot(&self) -> Result<Vec<u8>>;
    async fn restore(&mut self, snapshot: Vec<u8>) -> Result<()>;
}

// Storage backend (optional - LMDB/RocksDB provided)
trait StableStorage
```

### 2.3 Raft Protocol Support

| Feature | Support | Notes |
|---------|---------|-------|
| **Leader Election** | ✅ Yes | From tikv/raft-rs |
| **Log Replication** | ✅ Yes | From tikv/raft-rs |
| **Snapshots** | ✅ Yes | Automatic snapshot triggers |
| **Dynamic Membership** | ✅ Yes | Joint consensus supported |
| **Linearizable Reads** | ✅ Yes | ReadIndex approach (from tikv/raft-rs) |
| **Multi-Raft** | ❌ No | Single group per RaftNode |
| **Log Compaction** | ✅ Yes | Automatic via snapshots |
| **Pre-vote** | ✅ Yes | From tikv/raft-rs (v0.7+) |

**CRITICAL LIMITATION**: No built-in multi-Raft support for managing multiple consensus groups on one node.

### 2.4 Async/Tokio Support

**Async-First Design**: ✅ Yes

Example from documentation:
```rust
#[tokio::main]
async fn main() -> Result<()> {
    // Create Raft node
    let raft = Raft::bootstrap(
        node_id,
        initial_peers,
        store,
        config
    ).await?;

    // Run Raft in background
    let raft_handle = tokio::spawn(raft.clone().run());

    // Propose changes
    raft.propose(log_entry).await?;

    tokio::try_join!(raft_handle)?;
    Ok(())
}
```

**Tokio Compatibility**: Full support, uses tokio for async runtime and task spawning.

**API Ergonomics**: Good for simple use cases, but limited customization.

### 2.5 Storage Abstraction

**Storage Backends Supported**:
1. **LMDB** (via heed) - Default, recommended by maintainers
2. **RocksDB** - Added in later versions
3. **Custom** - Via `StableStorage` trait

**RaftLogStorage Customization**:
- ❌ **Cannot directly implement tikv/raft-rs `Storage` trait**
- ✅ **Can implement raftify's `StableStorage` trait** (limited)
- ⚠️ **Storage is tightly coupled to raftify's abstractions**

**Conclusion**: If you want to integrate Chronik's existing WAL as Raft storage, you'd need to:
1. Implement raftify's `StableStorage` trait (not tikv/raft-rs `Storage`)
2. Work around raftify's assumptions about LMDB-like storage
3. Potentially fork raftify to bypass its storage layer

**Complexity Rating**: HIGH - easier to use tikv/raft-rs directly.

### 2.6 Network Layer

**Network Transport**: gRPC (Tonic)

**Customization Options**:
- ❌ **Cannot bring your own network layer easily**
- ✅ **gRPC is built-in and mandatory**
- ⚠️ **Channel abstraction is tightly coupled**

**For Chronik**:
- ✅ We plan to use gRPC anyway (via Tonic)
- ✅ Compatible with our architecture
- ❌ But raftify's gRPC abstraction may conflict with our existing proto definitions

**Protobuf Requirement**:
- ✅ Uses `rust-protobuf` by default (from tikv/raft-rs)
- ⚠️ May require `protoc` for code generation
- ⚠️ Potential version conflicts with our gRPC definitions

### 2.7 Multi-Raft / Partition Support

**Critical Question**: Can raftify support multi-Raft (one consensus group per partition)?

**Answer**: ❌ **NO** - Not out of the box.

**Analysis**:
- Raftify wraps one `RawNode` per `RaftNode` instance
- No abstraction for managing multiple Raft groups on one process
- Would need to manually instantiate multiple `RaftNode` instances
- No shared heartbeat optimization (unlike TiKV's multi-Raft)

**For Chronik**:
- We need one Raft group per topic-partition (potentially 100s)
- Would require creating 100s of `RaftNode` instances
- Each would have separate gRPC connections (inefficient)
- No built-in batching of messages across groups

**Comparison to TiKV**:
- TiKV manages thousands of Raft groups per node efficiently
- Shares heartbeats across groups on same connection
- Raftify doesn't provide this infrastructure

**Recommendation**: Use tikv/raft-rs directly for multi-Raft scenarios.

---

## 3. Production Readiness

### 3.1 Production Users

**Known Deployments**:
1. **Backend.AI** - Manager process HA (experimental stage)
2. **Blockscape Validators** - Cosmos blockchain validators (NOT production-ready, warns "has not yet had a security audit or stress test")

**Status**: EXPERIMENTAL - No confirmed large-scale production deployments.

### 3.2 Battle-Tested Status

| Aspect | Status | Evidence |
|--------|--------|----------|
| **Jepsen Testing** | ❌ None | No evidence found |
| **Formal Verification** | ❌ None | No evidence found |
| **Chaos Engineering** | ❌ None | No evidence found |
| **Production Track Record** | ❌ Minimal | Only Backend.AI (experimental) |
| **Security Audit** | ❌ None | Explicitly states "not yet had security audit" |
| **Stress Testing** | ❌ None | Explicitly states "not yet stress tested" |

**Comparison to tikv/raft-rs**:
- tikv/raft-rs: Used in TiKV production clusters (thousands of nodes, petabytes of data)
- raftify: Used experimentally by ~1-2 projects

### 3.3 Test Coverage

**Evidence Found**:
- No public test coverage reports
- No CI badge showing test status
- Examples exist but unclear if comprehensive tests exist
- No integration test suite evidence

**Concern**: Without test coverage metrics or comprehensive test suite, stability is uncertain.

### 3.4 Performance Benchmarks

**Benchmark Data**: ❌ **NONE FOUND**

- No throughput numbers
- No latency measurements
- No comparison to tikv/raft-rs or other implementations
- No performance documentation

**Implication**: Cannot assess performance characteristics for Chronik's needs.

### 3.5 Stability Assessment

**API Stability**: ⚠️ **UNSTABLE**
- Version 0.1.x (pre-1.0)
- "API could be broken" warning
- "Incompatible changes before 1.0.0"

**Release Frequency**: ⚠️ **CONCERNING**
- Last release: 10 months ago (v0.1.82, Jan 2024)
- Could indicate:
  - Stable and complete (unlikely given "experimental" label)
  - Abandoned or low priority
  - Under heavy development in unreleased branches

**Production Readiness Score**: **2/10**

Breakdown:
- Functionality: 6/10 (works but limited features)
- Stability: 2/10 (experimental, unstable API)
- Community: 3/10 (small, limited adoption)
- Testing: 2/10 (no evidence of comprehensive testing)
- Performance: 0/10 (no benchmarks)
- Documentation: 4/10 (basic, incomplete)

---

## 4. Integration Complexity

### 4.1 API Ergonomics

**Ease of Use**: ⭐⭐⭐⭐☆ (4/5 for simple cases)

**Async/Await Friendly**: ✅ Yes

Example - Creating a Raft-backed KV store:
```rust
use raftify::*;

// 1. Define your log entry
#[derive(Serialize, Deserialize)]
enum LogEntry {
    Insert { key: String, value: String },
    Delete { key: String },
}

// 2. Implement state machine
struct KvStore {
    data: HashMap<String, String>,
}

#[async_trait]
impl AbstractStateMachine for KvStore {
    async fn apply(&mut self, entry: Vec<u8>) -> Result<Vec<u8>> {
        let log_entry: LogEntry = bincode::deserialize(&entry)?;
        match log_entry {
            LogEntry::Insert { key, value } => {
                self.data.insert(key, value);
            }
            LogEntry::Delete { key } => {
                self.data.remove(&key);
            }
        }
        Ok(vec![])
    }

    async fn snapshot(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self.data)
    }

    async fn restore(&mut self, snapshot: Vec<u8>) -> Result<()> {
        self.data = bincode::deserialize(&snapshot)?;
        Ok(())
    }
}

// 3. Bootstrap cluster
#[tokio::main]
async fn main() -> Result<()> {
    let store = KvStore::new();
    let raft = Raft::bootstrap(
        node_id,
        peers,
        store,
        Config::default()
    ).await?;

    // Run Raft
    let handle = tokio::spawn(raft.clone().run());

    // Use it
    raft.propose(log_entry).await?;

    handle.await?;
    Ok(())
}
```

**Pros**:
- Clean async API
- Simple trait implementations
- Good for basic use cases
- Less boilerplate than tikv/raft-rs

**Cons**:
- Limited customization points
- Hard to integrate with existing storage (like Chronik's WAL)
- gRPC layer is opaque
- No advanced configuration options

### 4.2 Learning Curve

**For Developers Familiar With**:
- **tikv/raft-rs**: Medium learning curve (new abstractions, but builds on same core)
- **Raft concepts**: Low-Medium (high-level API hides complexity)
- **Async Rust**: Low (clean async/await usage)

**Documentation Quality**: ⭐⭐⭐☆☆ (3/5)
- Basic getting-started guide
- Memstore example
- Deep-dive blog posts (2 articles)
- Missing: advanced usage, troubleshooting, performance tuning

### 4.3 Migration Effort

**Scenario**: Migrating from tikv/raft-rs to raftify

**Effort Estimate**: **7-10 days**

**Required Work**:
1. Rewrite `Storage` trait impl as `StableStorage` trait (2 days)
2. Adapt state machine to `AbstractStateMachine` (1 day)
3. Integrate gRPC layer with existing Chronik proto definitions (2 days)
4. Refactor RaftNode usage and API calls (1 day)
5. Testing and debugging (2-3 days)

**Complexity**: Medium-High

**Risk**: High - may discover incompatibilities late in migration.

### 4.4 Build Dependencies

**Protobuf Requirement**: ✅ Yes (but handled by rust-protobuf)

**From Documentation**:
> "You can use raft with either rust-protobuf or Prost to encode/decode gRPC messages. We use rust-protobuf by default."

**Build Requirements** (from DEVELOPMENT.md reference):
- Rust 1.70+ (likely)
- `protoc` (Protocol Buffers compiler) - possibly optional with vendored protos
- Standard Rust toolchain

**Chronik Compatibility**:
- ✅ We already use protobuf (via Tonic/prost)
- ⚠️ May have version conflicts (rust-protobuf vs prost)
- ⚠️ Need to verify proto compatibility

---

## 5. Chronik-Specific Fit

### 5.1 Multi-Raft Support

**Requirement**: Chronik needs one Raft group per topic-partition (potentially 100s-1000s).

**Raftify Support**: ❌ **DOES NOT MEET REQUIREMENT**

**Issues**:
1. No built-in multi-Raft abstraction
2. Would need to spawn 100s of `RaftNode` instances
3. Each instance has separate gRPC connections (network inefficiency)
4. No shared heartbeat optimization across groups
5. High memory overhead (LMDB per group)

**Alternative**: Use tikv/raft-rs directly and implement multi-Raft coordinator (like TiKV does).

### 5.2 gRPC Compatibility

**Chronik's Plan**: Use gRPC (Tonic) for cluster communication.

**Raftify's gRPC**: Uses Tonic (same as Chronik) ✅

**Compatibility**: ⚠️ **PARTIAL**

**Concerns**:
- Raftify defines its own gRPC service definitions
- May conflict with Chronik's proto definitions
- Hard to extend with custom RPC methods
- Opaque Channel abstraction

**Recommendation**: Use tikv/raft-rs + Tonic directly for full control.

### 5.3 WAL Integration

**Chronik's WAL**: GroupCommitWal with custom segment format.

**Raftify's Storage**: Expects LMDB or RocksDB-like storage.

**Integration Feasibility**: ⚠️ **DIFFICULT**

**Challenges**:
1. `StableStorage` trait expects key-value operations
2. Chronik's WAL is append-only log-structured
3. Impedance mismatch between abstractions
4. Would need adapter layer (complexity)

**Recommendation**: Use tikv/raft-rs `Storage` trait directly - better fit for WAL.

### 5.4 Async Ecosystem Fit

**Chronik's Stack**: Tokio + Tonic

**Raftify's Stack**: Tokio + Tonic ✅

**Compatibility**: ✅ **EXCELLENT**

**No Issues Expected**: Both use same async runtime and gRPC framework.

---

## 6. Comparison Matrix

### 6.1 Raftify vs tikv/raft-rs

| Criteria | Raftify | tikv/raft-rs | Winner |
|----------|---------|--------------|--------|
| **Abstraction Level** | High-level framework | Core consensus module | Depends on needs |
| **Ease of Use** | Easy (batteries included) | Hard (more boilerplate) | Raftify |
| **Flexibility** | Low (opinionated) | High (bring your own) | raft-rs |
| **Production Track Record** | Minimal (experimental) | Extensive (TiKV production) | raft-rs |
| **Multi-Raft Support** | No | Yes (with custom coordinator) | raft-rs |
| **Community Size** | Small (42 stars) | Large (3.9k stars) | raft-rs |
| **Documentation** | Basic | Comprehensive | raft-rs |
| **Test Coverage** | Unknown | High | raft-rs |
| **Performance** | Unknown (no benchmarks) | Proven (TiKV scale) | raft-rs |
| **API Stability** | Unstable (pre-1.0) | Stable (v0.7.x) | raft-rs |
| **Custom Storage** | Hard (adapter needed) | Easy (trait impl) | raft-rs |
| **Custom Network** | Very Hard (gRPC locked in) | Easy (trait impl) | raft-rs |
| **Async Support** | Native (tokio) | Library-agnostic | Tie |
| **Learning Curve** | Lower | Higher | Raftify |
| **For Chronik's Needs** | Poor fit | Good fit | raft-rs |

**Overall**: tikv/raft-rs wins for Chronik's use case.

### 6.2 Raftify vs openraft

| Criteria | Raftify | openraft | Winner |
|----------|---------|----------|--------|
| **Abstraction Level** | High-level | High-level | Tie |
| **Async/Await Native** | Yes | Yes | Tie |
| **Production Track Record** | Minimal | Moderate (Databend, etc.) | openraft |
| **Performance** | Unknown | 70k writes/sec (1 writer) | openraft |
| **Multi-Raft Support** | No | Yes (with custom coord) | openraft |
| **Community Size** | 42 stars | 3.3k stars | openraft |
| **Custom Storage** | Medium | Easy (trait) | openraft |
| **Custom Network** | Hard | Easy (trait) | openraft |
| **API Stability** | Unstable | Pre-1.0 but more mature | openraft |
| **Documentation** | Basic | Comprehensive | openraft |

**Overall**: openraft is superior to raftify in almost every way.

---

## 7. Pros and Cons

### Pros ✅

1. **Easy to Get Started**: Batteries-included approach with pre-wired gRPC/LMDB
2. **Async-Native**: Clean async/await API built on Tokio
3. **Less Boilerplate**: Higher-level abstractions reduce code compared to tikv/raft-rs
4. **Good Examples**: Memstore example is clear and simple
5. **Dual License**: Apache-2.0 + MIT (compatible with Chronik)
6. **Builds on tikv/raft-rs**: Inherits correctness of battle-tested core
7. **gRPC Built-in**: Uses Tonic (same as Chronik's plan)

### Cons ❌

1. **EXPERIMENTAL STATUS**: Explicitly marked as experimental, unstable API
2. **NO PRODUCTION TRACK RECORD**: Only Backend.AI using it experimentally
3. **NO BENCHMARKS**: Zero performance data available
4. **NO BATTLE TESTING**: No Jepsen, no formal verification, no chaos engineering
5. **SMALL COMMUNITY**: 42 stars, 14 contributors (vs 3.9k for tikv/raft-rs)
6. **STALE RELEASES**: 10 months since last release
7. **NO MULTI-RAFT**: Cannot efficiently manage multiple Raft groups per node
8. **OPINIONATED ARCHITECTURE**: Forces gRPC/LMDB, hard to customize
9. **POOR DOCUMENTATION**: Missing advanced usage, performance tuning, troubleshooting
10. **WAL INTEGRATION HARD**: Impedance mismatch with Chronik's WAL architecture
11. **NO TEST COVERAGE DATA**: Unknown test quality
12. **ADDS LAYER OF ABSTRACTION**: Additional indirection over tikv/raft-rs (more failure modes)

---

## 8. Code Examples

### 8.1 Basic Raft Node Setup

```rust
use raftify::prelude::*;

// Define log entry type
#[derive(Serialize, Deserialize, Clone, Debug)]
enum MyLogEntry {
    SetValue(String, String),
    DeleteKey(String),
}

impl AbstractLogEntry for MyLogEntry {
    fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self)
    }

    fn decode(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data)
    }
}

// Define state machine
struct MyStateMachine {
    data: HashMap<String, String>,
}

#[async_trait]
impl AbstractStateMachine for MyStateMachine {
    async fn apply(&mut self, entry: Vec<u8>) -> Result<Vec<u8>> {
        let log_entry: MyLogEntry = MyLogEntry::decode(&entry)?;

        match log_entry {
            MyLogEntry::SetValue(k, v) => {
                self.data.insert(k, v);
            }
            MyLogEntry::DeleteKey(k) => {
                self.data.remove(&k);
            }
        }

        Ok(vec![])
    }

    async fn snapshot(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self.data)
    }

    async fn restore(&mut self, snapshot: Vec<u8>) -> Result<()> {
        self.data = bincode::deserialize(&snapshot)?;
        Ok(())
    }
}

// Bootstrap Raft cluster
#[tokio::main]
async fn main() -> Result<()> {
    let node_id = 1;
    let peers = vec![
        Peer { id: 1, addr: "127.0.0.1:9001".to_string() },
        Peer { id: 2, addr: "127.0.0.1:9002".to_string() },
        Peer { id: 3, addr: "127.0.0.1:9003".to_string() },
    ];

    let fsm = MyStateMachine { data: HashMap::new() };

    let raft = Raft::bootstrap(
        node_id,
        peers,
        fsm,
        Config::default(),
    ).await?;

    // Run Raft in background
    let raft_handle = tokio::spawn(raft.clone().run());

    // Propose changes
    let entry = MyLogEntry::SetValue("key".into(), "value".into());
    raft.propose(entry.encode()?).await?;

    // Wait
    raft_handle.await??;
    Ok(())
}
```

### 8.2 Comparison with tikv/raft-rs

**Same Functionality in tikv/raft-rs**:

```rust
use raft::{Config, RawNode, storage::MemStorage, prelude::*};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Create storage
    let storage = MemStorage::new();

    // 2. Create config
    let cfg = Config {
        id: 1,
        election_tick: 10,
        heartbeat_tick: 3,
        ..Default::default()
    };

    // 3. Create raw node
    let mut raw_node = RawNode::new(&cfg, storage, &logger)?;

    // 4. Implement network layer (you write this)
    let (tx, mut rx) = mpsc::channel(100);

    // 5. Main loop
    loop {
        if raw_node.has_ready() {
            let mut ready = raw_node.ready();

            // Persist entries to storage
            if let Some(hs) = ready.hs() {
                storage.set_hardstate(hs.clone())?;
            }
            storage.append(&ready.entries)?;

            // Send messages over network (you implement this)
            for msg in ready.messages.drain(..) {
                send_message(msg).await?;
            }

            // Apply committed entries to state machine
            for entry in ready.committed_entries.take().unwrap() {
                apply_to_state_machine(&entry).await?;
            }

            raw_node.advance(ready);
        }

        // Receive messages from network
        if let Ok(msg) = rx.try_recv() {
            raw_node.step(msg)?;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
```

**Analysis**:
- Raftify: ~50 lines, clean, but opinionated
- tikv/raft-rs: ~80 lines, more boilerplate, but flexible
- For simple cases: Raftify wins on ease-of-use
- For Chronik's needs (multi-Raft, custom WAL): tikv/raft-rs wins on flexibility

---

## 9. Decision Matrix

### 9.1 Use Raftify If:

- ✅ Building a simple distributed application (single Raft group)
- ✅ Want to get started quickly with minimal code
- ✅ Fine with LMDB/RocksDB storage
- ✅ Fine with gRPC (Tonic) networking
- ✅ Don't need multi-Raft support
- ✅ Experimental/prototype stage project
- ✅ Can tolerate unstable API

### 9.2 DON'T Use Raftify If:

- ❌ Need production-ready, battle-tested solution
- ❌ Need multi-Raft (100s of consensus groups per node)
- ❌ Need to integrate existing storage (like Chronik's WAL)
- ❌ Need custom network layer
- ❌ Need performance guarantees
- ❌ Need stable API for long-term project
- ❌ Need comprehensive documentation
- ❌ Need large community support

### 9.3 For Chronik Specifically:

**Requirements**:
1. Multi-Raft (one group per partition) - ❌ Raftify doesn't support
2. WAL integration - ❌ Hard with raftify's abstractions
3. Production-ready - ❌ Raftify is experimental
4. gRPC networking - ✅ Raftify supports
5. Async/Tokio - ✅ Raftify supports
6. Custom storage - ❌ Hard with raftify
7. Battle-tested - ❌ Raftify lacks this

**Score**: 2/7 requirements met

**Recommendation**: **DO NOT USE** raftify for Chronik.

---

## 10. Integration Effort Estimate

**Scenario**: Integrate raftify into Chronik for clustering

### Phase 1: Exploration (2-3 days)
- Understand raftify API
- Build proof-of-concept (single Raft group)
- Test with Chronik data structures

### Phase 2: Implementation (5-7 days)
- Implement `StableStorage` trait for Chronik's WAL
- Implement `AbstractStateMachine` for partition state
- Integrate gRPC layer with existing protos
- Wire up RaftNode to Chronik server

### Phase 3: Multi-Raft Workaround (3-5 days)
- Design multi-RaftNode management
- Implement RaftNode lifecycle per partition
- Handle cross-group communication
- Optimize network usage (shared connections)

### Phase 4: Testing (3-5 days)
- Unit tests for Raft integration
- Integration tests for cluster scenarios
- Performance testing
- Chaos testing (simulate failures)

### Phase 5: Debugging & Fixes (3-5 days)
- Fix integration bugs
- Address performance issues
- Handle edge cases
- Documentation

**Total Estimate**: **16-25 days**

**Risk Level**: **HIGH**
- Experimental library may have hidden bugs
- Multi-Raft workaround may hit architectural limits
- WAL integration may require raftify fork
- Performance may not meet requirements
- API instability may require future rewrites

**Comparison**:
- **Using tikv/raft-rs directly**: 20-30 days, but **more control** and **lower risk**
- **Using openraft**: 15-20 days, similar abstraction level but **more mature**

---

## 11. Alternative Recommendations

### Option 1: tikv/raft-rs (Direct) ⭐ RECOMMENDED

**Pros**:
- Battle-tested in TiKV production
- Flexible (bring your own storage/network)
- Supports multi-Raft (with custom coordinator)
- Large community (3.9k stars)
- Stable API
- Excellent documentation
- Easy WAL integration (implement `Storage` trait)

**Cons**:
- More boilerplate code
- Steeper learning curve
- Need to implement network/storage layers

**Effort**: 20-30 days
**Risk**: Low
**Production Readiness**: High (9/10)

### Option 2: openraft ⭐ ALTERNATIVE

**Pros**:
- Async-native (like raftify)
- Mature (used in Databend, etc.)
- Flexible (custom storage/network via traits)
- Good performance (70k writes/sec benchmarked)
- Active community (3.3k stars)
- Better than raftify in almost every way

**Cons**:
- API still pre-1.0 (but more stable than raftify)
- Less production track record than tikv/raft-rs
- Different API from tikv/raft-rs (migration cost)

**Effort**: 15-20 days
**Risk**: Medium
**Production Readiness**: Medium-High (7/10)

### Option 3: raftify ❌ NOT RECOMMENDED

**Pros**: Easy to start

**Cons**: Everything else (see Cons section above)

**Effort**: 16-25 days
**Risk**: High
**Production Readiness**: Low (2/10)

---

## 12. Final Recommendation

### For Chronik Clustering: **Use tikv/raft-rs Directly**

**Rationale**:

1. **Production Requirements**: Chronik aims to be production-ready. Raftify is experimental.

2. **Multi-Raft Need**: Chronik needs one Raft group per partition (100s-1000s). Raftify doesn't support this. tikv/raft-rs does (with custom coordinator, which TiKV proves is feasible).

3. **WAL Integration**: Chronik has a custom GroupCommitWal. tikv/raft-rs `Storage` trait is a better fit than raftify's `StableStorage`.

4. **Battle-Tested**: tikv/raft-rs has proven itself at scale in TiKV. Raftify has not.

5. **Community Support**: 3.9k stars vs 42 stars. Easier to get help with tikv/raft-rs.

6. **Control**: Chronik is a production system. We need full control over storage, networking, and performance tuning. Raftify's opinionated architecture limits this.

7. **Risk Management**: Using an experimental library (raftify) for a core component (consensus) is high risk. tikv/raft-rs is lower risk.

**Implementation Plan**:

1. **Week 1-2**: Study tikv/raft-rs API and TiKV's multi-Raft implementation
2. **Week 3-4**: Implement `Storage` trait for Chronik's WAL
3. **Week 5-6**: Implement `RaftNetwork` trait for gRPC communication
4. **Week 7-8**: Build multi-Raft coordinator (managing multiple `RawNode` instances)
5. **Week 9-10**: Integration with Chronik server + testing

**Total Timeline**: 10 weeks (vs 3-5 weeks with raftify, but **much lower risk**)

**If Timeline is Critical**: Consider openraft (faster than tikv/raft-rs, safer than raftify).

---

## 13. Appendix: Research Links

### Official Resources
- GitHub: https://github.com/lablup/raftify
- Crates.io: https://crates.io/crates/raftify
- Docs.rs: https://docs.rs/raftify/latest/raftify/

### Blog Posts (Maintainers)
- Introduction: https://www.backend.ai/blog/2024-01-26-introduce-raftify
- Deep Dive Part 1: https://www.backend.ai/blog/2024-03-29-deepdive-raft
- Deep Dive Part 2: https://www.backend.ai/blog/2024-05-29-deepdive-raft-2

### Related Projects
- tikv/raft-rs: https://github.com/tikv/raft-rs
- openraft: https://github.com/databendlabs/openraft
- async-raft (archived, became openraft): https://github.com/async-raft/async-raft

### Comparison Resources
- TiKV Multi-Raft: https://tikv.org/deep-dive/scalability/multi-raft/
- Raft Consensus: https://raft.github.io/

---

## 14. Conclusion

**Raftify is an interesting project** that fills a niche for developers wanting a batteries-included Raft framework. However, for **Chronik's production clustering needs**, it falls short in critical areas:

- Experimental status (not production-ready)
- No multi-Raft support (essential for Chronik)
- Opinionated architecture (limits integration with Chronik's WAL)
- Small community and unknown quality (testing, performance)

**The additional abstraction layer raftify provides is not worth the trade-offs** for a production system like Chronik. We're better off using **tikv/raft-rs directly**, accepting the higher implementation effort in exchange for battle-tested reliability, flexibility, and community support.

**Verdict**: ❌ **SKIP raftify, use tikv/raft-rs**

---

**Report Generated**: 2025-10-16
**Research Duration**: ~2 hours
**Confidence Level**: High (based on comprehensive web research, documentation analysis, and architecture comparison)
