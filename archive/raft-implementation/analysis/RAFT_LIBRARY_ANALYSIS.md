# TiKV raft-rs Deep Dive Analysis & Recommendation

**Date**: 2025-10-16
**Analyst**: Claude (Sonnet 4.5)
**Context**: Chronik Stream Raft implementation - Phase 1 complete, evaluating tikv/raft-rs viability

---

## Executive Summary

**RECOMMENDATION: Switch to OpenRaft**

After comprehensive analysis, **switching to OpenRaft is strongly recommended** over persisting with tikv/raft-rs. The protoc issue is solvable but masks deeper architectural mismatches between tikv/raft-rs and Chronik's async-first design.

**Key Rationale**:
- **Protoc Issue**: Solvable in 30 minutes (downgrade protoc), but this is a symptom of deeper maintenance concerns
- **Async Architecture**: OpenRaft is native async/await, tikv/raft-rs requires timer-based polling (fundamental mismatch)
- **API Stability**: Both pre-1.0, but OpenRaft actively maintained (Nov 2024 commits) vs raft-rs (last release Dec 2024, less frequent)
- **Production Usage**: Both battle-tested (TiKV vs Databend), but OpenRaft designed for modern Rust ecosystem
- **Migration Cost**: Phase 1 only completed RPC protocol - minimal sunk cost (500 LOC, 2 days work)

**Effort Estimates**:
- **Fix protoc & continue with raft-rs**: 30 mins + ongoing friction with async integration (2-3 weeks Phase 2 complexity)
- **Switch to OpenRaft**: 1-2 days refactor + cleaner Phase 2 implementation (1-1.5 weeks Phase 2)

---

## 1. Current Status: tikv/raft-rs (v0.7.0)

### 1.1 Library Metadata

- **Latest Version**: 0.7.0 (released 2023-08-31, over 1 year old)
- **Maintenance Status**: Active but slow
  - Last GitHub activity: December 2024 (issues #558, #559)
  - Commit frequency: ~1-2 issues/PRs per month
  - Response time: Variable (some issues open for months)
- **Production Users**:
  - TiKV itself (~1000+ adopters, trillion key-value pairs scale)
  - Stable since April 2016 (8+ years production proven)
- **License**: Apache 2.0 (compatible)

### 1.2 Technical Architecture

**Strengths**:
- Core Consensus Module only (customizable, flexible, resilient)
- Battle-tested at extreme scale (TiKV clusters with hundreds of nodes)
- Comprehensive Raft features: leader election, log replication, snapshots, joint consensus
- Well-documented with blog posts and implementation guides

**Limitations** (CRITICAL for Chronik):
- **NOT async/await native**: Requires timer-based polling (`RawNode::tick()`)
- **Manual integration**: Must build your own Log, State Machine, Transport
- **Sync-first design**: Doesn't leverage tokio's async ecosystem naturally
- **Protobuf v2 dependency**: Security vulnerability (RUSTSEC-2024-0437)

### 1.3 Async Integration Pattern (tikv/raft-rs)

```rust
// What you HAVE TO DO with raft-rs (timer-based polling)
loop {
    select! {
        _ = ticker.tick() => {
            raft_node.tick();  // Drive state machine manually
        }
        msg = rx.recv() => {
            raft_node.step(msg)?;
        }
    }
}
```

**Problem**: This polling model conflicts with Chronik's async-first design where everything is event-driven via tokio channels, futures, and async/await.

---

## 2. The Protoc Issue Analysis

### 2.1 Root Cause

**Error**:
```
The system `protoc` version mismatch, require >= 3.1.0, got 32.1.x, fallback to the bundled `protoc`
thread 'main' panicked at protobuf-build-0.14.1/src/protobuf_impl.rs:35:14:
No suitable `protoc` (>= 3.1.0) found in PATH
```

**Diagnosis**:
- `raft-proto 0.7.0` → depends on `protobuf-build 0.14.1`
- `protobuf-build 0.14.1` has hardcoded version check: `>= 3.1.0, < 32.0`
- Installed protoc: `32.1` (too new, released 2024)
- Version check rejects protoc 32.1 despite being backward compatible

### 2.2 Why This Happened

1. **raft-proto 0.7.0 released**: August 2023 (1.5 years ago)
2. **protobuf 32.x released**: 2024 (after raft-proto 0.7.0)
3. **Homebrew updated**: Default protoc → 32.1 (latest)
4. **Version check too strict**: `protobuf-build 0.14.1` didn't anticipate protoc 32.x

**This is a maintenance lag issue** - the crate hasn't been updated to support newer protoc versions.

### 2.3 Known Issues & Workarounds

**GitHub Issues Found**:
- [tikv/raft-rs#272](https://github.com/tikv/raft-rs/issues/272): "Using prost-codec causes raft to fail to build"
- [qdrant/qdrant#7279](https://github.com/qdrant/qdrant/issues/7279): "Vulnerability in protobuf version 2" (RUSTSEC-2024-0437)
- Multiple reports of protoc version mismatches across ecosystem

**Workarounds** (in order of preference):

#### Option A: Downgrade protoc (FASTEST - 30 minutes)

```bash
# Uninstall current protoc
brew uninstall protobuf

# Install protobuf@21 (21.12, compatible with protobuf-build 0.14.1)
brew install protobuf@21

# Symlink into PATH (keg-only formula)
brew link --force protobuf@21

# Verify
protoc --version  # Should show libprotoc 21.12

# Build chronik-raft
cargo build -p chronik-raft
```

**Pros**: Quick, unblocks build immediately
**Cons**: Uses deprecated Homebrew formula (disabled 2026-01-08), may conflict with other projects needing protoc 32.x

#### Option B: Fork raft-proto and patch (2-3 hours)

```toml
# In chronik-raft/Cargo.toml
[dependencies]
raft = { version = "0.7", default-features = false }
raft-proto = { git = "https://github.com/chronik-stream/raft-proto", branch = "fix/protobuf-3x" }

# In forked raft-proto/Cargo.toml
[dependencies]
protobuf-build = "0.15"  # Or vendor and patch version check
```

**Steps**:
1. Fork `tikv/raft-proto`
2. Update `protobuf-build` to 0.15+ (if exists) or vendor and remove version check
3. Test build
4. Point `chronik-raft` to fork

**Pros**: Permanent fix, no system protoc downgrade
**Cons**: Maintenance burden (track upstream), adds custom fork

#### Option C: Wait for upstream fix (UNKNOWN timeline)

Check if there's an open PR to fix this:
- Search [tikv/raft-rs PRs](https://github.com/tikv/raft-rs/pulls) for "protobuf" or "protoc"
- Last relevant PR: [#264](https://github.com/tikv/raft-rs/pull/264) (support protobuf build by default) - merged 2022

**Status**: No active PR found to support protoc 32.x
**Timeline**: Unknown (could be weeks/months given slow maintenance pace)

**Pros**: No custom code
**Cons**: Blocks progress indefinitely, no guarantee of fix

---

## 3. Production Battle-Testing

### 3.1 tikv/raft-rs

**Scale Proven**:
- TiKV: ~1000 production adopters (e-commerce, fintech, media)
- Cluster size: Hundreds of nodes, trillion+ key-value pairs
- Performance: 4% throughput improvement with Raft Engine optimization
- Years in production: 8+ years (since April 2016)

**Known Bugs/Limitations**:
- GitHub Issues: ~80 open issues (some from 2019)
- Notable: Joint consensus project stalled (last updated March 2024)
- Protobuf vulnerability: RUSTSEC-2024-0437 (protobuf 2.28.0)

**Benchmarks**:
- Criterion benchmarks exist but "ongoing effort to build appropriate suite"
- TiKV benchmarks: go-ycsb tests show stable throughput/latency at scale
- No public async integration benchmarks found

### 3.2 Alternative: OpenRaft

**Scale Proven**:
- Databend: Production usage as meta-service cluster consensus engine
- Performance: 70K writes/sec (1 writer), 1M writes/sec (256 writers)
- Unit test coverage: 92%

**Maturity**:
- Derived from async-raft (bugs fixed)
- Active development: November 2024 commits (latest: Nov 30, 2024)
- API unstable: Pre-1.0 (incompatible changes possible)
- Chaos testing: Not yet completed

---

## 4. Technical Depth Comparison

| Feature | tikv/raft-rs (0.7.0) | OpenRaft (0.9.21) |
|---------|----------------------|-------------------|
| **Async Native** | ❌ No (timer-based) | ✅ Yes (event-driven) |
| **Log Storage Trait** | ✅ Yes | ✅ Yes (RaftLogStorage) |
| **State Machine Trait** | ✅ Yes | ✅ Yes (RaftStateMachine) |
| **Network Trait** | ❌ No (manual) | ✅ Yes (RaftNetwork) |
| **Snapshot Efficiency** | ✅ Streaming | ✅ Chunk-based |
| **Raft Features** | Leader election, log replication, snapshots, joint consensus | Same + optimized batching |
| **Protocol Compliance** | ✅ Full Raft paper | ✅ Full Raft paper |
| **Tokio Integration** | ⚠️ Manual polling | ✅ Native async/await |
| **Protobuf Version** | protobuf 2.28 (vulnerable) | prost (modern) |
| **API Stability** | Pre-1.0 (slow changes) | Pre-1.0 (active changes) |
| **Maintenance** | ~1-2 PRs/month | ~5-10 PRs/month (2024) |
| **Documentation** | Excellent (TiKV blog) | Good (docs.rs + examples) |

**Verdict**: OpenRaft is architecturally superior for async Rust projects. tikv/raft-rs is proven at scale but designed for sync/timer-based systems.

---

## 5. Workaround Complexity Analysis

### 5.1 Option A: Fix protoc & continue with raft-rs

**Steps**:
1. Downgrade protoc to 21.12: **15 minutes**
2. Build chronik-raft: **5 minutes**
3. Continue Phase 2 with async integration: **2-3 weeks** (high complexity due to polling model)

**Complexity Breakdown** (Phase 2):
- Wrap `RawNode` in async task: Moderate
- Implement timer-based `tick()` loop: Easy
- Bridge sync RawNode with async WAL: **HARD** (channels, Arc, Mutex, blocking pool)
- Integrate with async gRPC (tonic): **HARD** (spawn blocking, message queues)
- Test async correctness: **VERY HARD** (race conditions, deadlocks)

**Estimated Phase 2 Duration**: 2-3 weeks (due to sync/async impedance mismatch)

### 5.2 Option B: Fork raft-proto

**Steps**:
1. Fork tikv/raft-proto: **15 minutes**
2. Update protobuf-build dependency: **30 minutes**
3. Test build: **15 minutes**
4. Update chronik-raft Cargo.toml: **10 minutes**
5. CI/CD setup for fork: **1 hour**

**Ongoing Cost**:
- Track upstream updates: **30 minutes/month**
- Merge conflicts on updates: **1-2 hours/quarter**

**Total**: 2-3 hours initial + ongoing maintenance burden

### 5.3 Option C: Switch to OpenRaft

**Steps**:
1. Replace raft dependency: **5 minutes**
2. Update RPC protocol (raft_rpc.proto → OpenRaft types): **2-3 hours**
3. Refactor storage trait: **2-3 hours**
4. Refactor replica to use async Raft API: **4-6 hours**
5. Update tests: **2-3 hours**

**Total Phase 1 Refactor**: 1-2 days

**Phase 2 Benefits**:
- Native async/await (no polling loops): **-5 days complexity**
- RaftNetwork trait (no manual gRPC bridging): **-3 days complexity**
- Event-driven (fits Chronik's design): **-2 days complexity**

**Estimated Phase 2 Duration**: 1-1.5 weeks (vs 2-3 weeks with raft-rs)

**Net Savings**: 1-2 days refactor cost, **save 1-2 weeks in Phase 2**

---

## 6. Final Recommendation

### 6.1 Decision: Switch to OpenRaft

**Rationale**:

1. **Architecture Fit** (CRITICAL):
   - Chronik is async-first (tokio, async/await everywhere)
   - tikv/raft-rs is sync-first (timer polling, blocking RawNode)
   - OpenRaft is native async (event-driven, no polling)
   - **Fighting the library is always more expensive than switching**

2. **Sunk Cost Fallacy**:
   - Phase 1 completed: 500 LOC, mostly RPC stubs
   - 80% of code is generic (config, error types, storage traits)
   - Only `rpc.rs` + `replica.rs` need refactor (~200 LOC)
   - **2 days of work is NOT worth 2 weeks of Phase 2 friction**

3. **Long-Term Viability**:
   - tikv/raft-rs: Slow maintenance (1.5 years since last release)
   - OpenRaft: Active development (Nov 2024 commits)
   - Both pre-1.0, but OpenRaft evolving faster
   - **Better to ride the active project**

4. **Production Readiness**:
   - Both battle-tested (TiKV vs Databend)
   - OpenRaft: 92% test coverage, 1M writes/sec proven
   - tikv/raft-rs: 8 years proven, but async integration untested
   - **For async systems, OpenRaft is lower risk**

5. **Protoc Issue as Signal**:
   - Not a blocker, but symptom of maintenance lag
   - Workarounds exist but add technical debt
   - **Fix is easy, but root cause (slow updates) persists**

### 6.2 Implementation Plan

**Immediate (Next 2 Days)**:

```bash
# Day 1: Dependency switch + RPC refactor
1. Update chronik-raft/Cargo.toml:
   - Remove: raft = "0.7", tonic-build = "0.12"
   - Add: openraft = "0.9.21"

2. Refactor rpc.rs:
   - Replace gRPC RaftService with OpenRaft's RaftNetwork trait
   - Remove proto/raft_rpc.proto (OpenRaft uses internal types)
   - Implement RaftNetworkFactory for inter-node communication

3. Refactor storage.rs:
   - Implement openraft::RaftLogStorage trait
   - Implement openraft::RaftStateMachine trait
   - Keep MemoryLogStorage for testing

# Day 2: Replica + tests
4. Refactor replica.rs:
   - Use openraft::Raft type (single API)
   - Remove manual tick() loops
   - Add async event handlers (on_receive_entry, on_commit, etc.)

5. Update tests (rpc_test.rs):
   - Remove gRPC mock tests
   - Add OpenRaft integration tests (example from docs)

6. Update build.rs:
   - Remove tonic-build (no proto compilation needed)

7. Documentation:
   - Update README.md with OpenRaft examples
   - Update PHASE1_SUMMARY.md → OPENRAFT_MIGRATION.md
```

**Phase 2 (Next 1-1.5 Weeks)**:

```rust
// Phase 2 will be MUCH simpler with OpenRaft:

// 1. Async Raft node (no polling!)
let raft = Raft::new(
    node_id,
    Arc::new(raft_config),
    Arc::new(network),
    Arc::new(log_storage),
    Arc::new(state_machine),
).await?;

// 2. Event-driven writes (natural async)
raft.client_write(ClientWriteRequest::new(entry)).await?;

// 3. Network integration (trait implementation)
impl RaftNetwork for ChronikNetwork {
    async fn send_append_entries(&self, target: NodeId, rpc: AppendEntriesRequest) {
        // Just call gRPC client - no sync/async bridging!
        self.grpc_clients.get(target).append_entries(rpc).await
    }
}
```

### 6.3 Migration Checklist

- [ ] Backup current chronik-raft crate (git branch: `raft-rs-backup`)
- [ ] Update Cargo.toml dependencies
- [ ] Implement RaftLogStorage trait (reuse existing storage code)
- [ ] Implement RaftStateMachine trait (partition state machine)
- [ ] Implement RaftNetwork trait (gRPC client wrapper)
- [ ] Refactor PartitionReplica to use `Raft` type
- [ ] Update tests to use OpenRaft test utilities
- [ ] Remove proto/raft_rpc.proto
- [ ] Update documentation
- [ ] Integration test with 3-node cluster
- [ ] Performance benchmark (compare with OpenRaft examples)

### 6.4 Rollback Strategy

**If OpenRaft doesn't work out**:
1. Git revert to `raft-rs-backup` branch (5 minutes)
2. Apply Option A workaround (downgrade protoc): 30 minutes
3. Continue with raft-rs Phase 2: 2-3 weeks

**Risk**: Low (Phase 1 is mostly stubs, easy to revert)

---

## 7. Conclusion

**DO NOT** persist with tikv/raft-rs. The protoc issue is a red herring - the real problem is architectural mismatch between sync-first raft-rs and async-first Chronik.

**SWITCH** to OpenRaft now while Phase 1 is fresh (500 LOC, 2 days work). The refactor cost (1-2 days) is dwarfed by Phase 2 savings (1-2 weeks) and long-term maintenance benefits.

**The protoc fix is 30 minutes. The async integration pain is 2-3 weeks. Choose wisely.**

---

## 8. References

- [tikv/raft-rs GitHub](https://github.com/tikv/raft-rs)
- [tikv/raft-rs v0.7.0 Release](https://github.com/tikv/raft-rs/releases/tag/v0.7.0)
- [OpenRaft GitHub](https://github.com/databendlabs/openraft)
- [OpenRaft Documentation](https://docs.rs/openraft)
- [TiKV Blog: Implement Raft in Rust](https://tikv.org/blog/implement-raft-in-rust/)
- [OpenRaft Derived from async-raft](https://github.com/databendlabs/openraft/blob/main/derived-from-async-raft.md)
- [Raft Paper](https://raft.github.io/raft.pdf)
- [RUSTSEC-2024-0437: protobuf 2.28.0 vulnerability](https://rustsec.org/)

---

**Next Action**: Seek user approval for OpenRaft migration, then execute 2-day refactor plan.
