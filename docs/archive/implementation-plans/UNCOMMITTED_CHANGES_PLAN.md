# Uncommitted Changes Management Plan

**Date**: 2025-10-24
**Context**: Session investigating Raft stability issues revealed ongoing problems despite fixes
**Status**: Planning phase - awaiting decision on how to proceed

---

## Current Uncommitted Changes Summary

### Files Modified (13 files, +748/-105 lines)
```
CHANGELOG.md                                   | 164 +++++ (v1.3.67 entry)
crates/chronik-raft/src/group_manager.rs       |   1 +
crates/chronik-raft/src/lib.rs                 |   4 +-
crates/chronik-raft/src/raft_meta_log.rs       |  51 ++-- (callbacks)
crates/chronik-raft/src/replica.rs             |  33 +++ (logging)
crates/chronik-raft/src/rpc.rs                 |  40 +++
crates/chronik-server/src/fetch_handler.rs     | 154 +++++ (tiered fetch)
crates/chronik-server/src/integrated_server.rs | 114 ++++ (metadata store getter)
crates/chronik-server/src/main.rs              |  15 +-- (metrics port fix)
crates/chronik-server/src/raft_cluster.rs      |  11 ++ (set_metadata_store call)
crates/chronik-server/src/raft_integration.rs  | 121 ++++ (CRITICAL FIXES)
docs/planning/CLUSTERING_TRACKER.md            | 142 +++++
tests/Cargo.lock                               |   3 +-
```

### Key Changes Made This Session

#### 1. **Metrics Port Conflict Fix** (GOOD - Keep)
**File**: `crates/chronik-server/src/main.rs:529`
```rust
// Changed from kafka_port + 2 to kafka_port + 4000
let derived = cli.kafka_port + 4000;
```
**Status**: ✅ **WORKING** - Fixed Node 3 startup crash
**Reason to Keep**: Critical fix that allows 3-node cluster to start

#### 2. **Message Ordering Fix** (PARTIAL - Keep but insufficient)
**File**: `crates/chronik-server/src/raft_integration.rs:629-644`
```rust
// Removed tokio::spawn(), now awaits sequentially
for msg in messages {
    let to = msg.to;
    if let Err(e) = raft_client.send_message(&topic, partition, to, msg).await {
        error!("Failed to send message to peer {}: {}", to, e);
    }
}
```
**Status**: ⚠️ **HELPS BUT NOT ENOUGH** - Reduced churn from 4000+ terms to 2000+ terms
**Reason to Keep**: 50% improvement, builds foundation for Phase 2 fixes
**Next Step**: Needs Phase 2 (non-blocking state machine) to complete the fix

#### 3. **Metadata Synchronization Fix** (GOOD - Keep)
**Files**:
- `crates/chronik-server/src/raft_integration.rs:238-309` (RwLock wrapper + set_metadata_store)
- `crates/chronik-server/src/integrated_server.rs:1141-1144` (metadata_store getter)
- `crates/chronik-server/src/raft_cluster.rs:624-633` (call set_metadata_store)

**Status**: ✅ **WORKING** - Event handler now uses RaftMetaLog
**Reason to Keep**: Critical architectural fix, correct design
**Issue**: Doesn't help because `__meta` has no stable leader (due to election churn)

#### 4. **Event Handler with Retry** (ADDED but not complete)
**File**: `crates/chronik-server/src/raft_integration.rs:728-810`
- Added event loop to process BecameLeader/BecameFollower events
- Updates partition assignments when leaders change

**Status**: ⚠️ **INCOMPLETE** - Needs retry logic (Phase 4)
**Reason to Keep**: Core infrastructure for metadata sync

#### 5. **CHANGELOG Updates** (DOCUMENTATION)
**File**: `CHANGELOG.md`
- Added v1.3.67 entry documenting message ordering fix
- Comprehensive details of the investigation

**Status**: ✅ **KEEP** - Valuable documentation

---

## Decision Points

### Option A: Keep All Changes, Continue with Comprehensive Fix Plan ✅ RECOMMENDED

**What to do**:
1. **Commit current changes as v1.3.67-wip** (work in progress)
   ```bash
   git add -A
   git commit -m "wip: v1.3.67 partial fixes - metrics port, message ordering, metadata sync

   - Fixed metrics port conflict (kafka_port + 4000)
   - Fixed message ordering (sequential await instead of tokio::spawn)
   - Fixed metadata synchronization (RwLock + set_metadata_store)
   - Added event handler for partition assignment updates

   KNOWN ISSUES (being addressed):
   - Election churn still present (2000+ terms) - needs Phase 1 (config) + Phase 2 (non-blocking)
   - Deserialization errors (13,489) - needs Phase 3 investigation
   - Metadata sync incomplete - needs Phase 4 (retry logic)

   See docs/COMPREHENSIVE_RAFT_FIX_PLAN.md for complete fix plan"
   ```

2. **Create a feature branch for safety**
   ```bash
   # Save current work
   git checkout -b raft-stability-fixes-v1.3.67

   # Continue work on this branch
   # When done, can merge back to main or abandon if needed
   ```

3. **Execute Comprehensive Fix Plan**
   - Phase 1: Fix Raft config (2h)
   - Phase 2: Non-blocking state machine (6h)
   - Phase 3: Fix deserialization (8h)
   - Phase 4: Metadata retry logic (3h)
   - Phase 5: Comprehensive testing (6h)

4. **After all phases succeed**
   ```bash
   # Merge back to main
   git checkout main
   git merge raft-stability-fixes-v1.3.67

   # Tag as v1.3.67 (or v2.0.0-rc1)
   git tag -a v1.3.67 -m "Raft stability fixes for multi-node clustering"
   ```

**Pros**:
- ✅ Preserves all work done (748 lines of valuable code)
- ✅ Current changes are foundation for complete fix
- ✅ Feature branch provides safety net
- ✅ Can abandon branch if fixes fail (no harm to main)

**Cons**:
- ⚠️ Requires 24 hours of focused work
- ⚠️ Risk of discovering unfixable issues in Phase 3

---

### Option B: Stash Changes, Start Fresh with Just Phase 1

**What to do**:
1. **Stash all uncommitted changes**
   ```bash
   git stash push -u -m "v1.3.67 wip - partial fixes before comprehensive plan"
   ```

2. **Implement ONLY Phase 1 (Raft config change)**
   - Single file change: `crates/chronik-server/src/raft_cluster.rs:56-60`
   - Change election_timeout_ms: 500 → 3000
   - Change heartbeat_interval_ms: 100 → 150

3. **Test Phase 1 in isolation**
   - Build, start cluster
   - Verify election churn stops
   - If successful → commit Phase 1
   - If fails → pop stash, proceed to Option A

4. **After Phase 1 success, pop stash and integrate**
   ```bash
   git stash pop
   # Resolve any conflicts (should be minimal)
   # Continue with Phases 2-5
   ```

**Pros**:
- ✅ Clean slate for testing Phase 1
- ✅ Can verify config change alone fixes majority of churn
- ✅ Stash preserves all work (can retrieve anytime)

**Cons**:
- ⚠️ Loses metrics port fix (will need to re-apply)
- ⚠️ Loses message ordering fix (will need to re-apply)
- ⚠️ More work to reintegrate later

---

### Option C: Cherry-Pick Good Fixes, Discard Rest

**What to do**:
1. **Create a clean branch from HEAD**
   ```bash
   git checkout -b raft-config-only
   ```

2. **Manually apply ONLY proven fixes**:
   - Metrics port fix (1 file, 1 line)
   - Raft config change (1 file, 5 lines)

3. **Discard everything else**
   - Message ordering fix (insufficient)
   - Metadata sync (doesn't help without stable leader)
   - Event handler (incomplete)

4. **Test minimal fix**
   - If works → great, much simpler
   - If fails → revert to Option A

**Pros**:
- ✅ Simplest possible fix
- ✅ Minimal code changes
- ✅ Easier to test and verify

**Cons**:
- ❌ Discards 700+ lines of valuable work
- ❌ Message ordering fix DOES help (50% reduction)
- ❌ Metadata sync architecture is correct design
- ❌ Will need to re-implement for Phases 2-5

---

### Option D: Commit as Experimental, Mark Raft Unstable

**What to do**:
1. **Commit all current changes as-is**
   ```bash
   git add -A
   git commit -m "feat: Raft stability improvements (experimental - v1.3.67-alpha)

   Partial fixes for multi-node Raft clustering:
   - Metrics port conflict resolution
   - Sequential message ordering (reduces churn by 50%)
   - Metadata synchronization architecture
   - Event-driven partition assignment

   EXPERIMENTAL: Multi-node Raft still has stability issues.
   Recommended for testing only. Use standalone mode for production.

   See docs/KNOWN_ISSUES_v2.0.0.md for details"
   ```

2. **Update documentation**
   - Mark Raft as "experimental" in README
   - Add warning in startup logs
   - Document standalone mode as "production-ready"

3. **Release v2.0.0 with clear expectations**
   - Standalone mode: ✅ Production-ready
   - Multi-node Raft: ⚠️ Experimental (use at own risk)

4. **Continue fixing in v2.1.0**
   - No pressure for immediate release
   - Can take time to fix properly

**Pros**:
- ✅ Preserves all work
- ✅ No pressure for immediate fix
- ✅ Standalone mode still works perfectly
- ✅ Honest with users about stability

**Cons**:
- ⚠️ v2.0.0 ships with known issues in Raft
- ⚠️ Users might avoid Raft entirely
- ⚠️ Delays the "complete" v2.0.0 vision

---

## Recommendations by Scenario

### If you want v2.0.0 GA with working Raft → **Option A**
- Commit current work as WIP
- Execute full comprehensive plan (24 hours)
- High confidence of success based on analysis

### If you want to validate config fix first → **Option B**
- Stash current work
- Test Phase 1 alone
- If successful, pop stash and continue

### If you want simplest possible fix → **Option C**
- Keep only metrics port + config changes
- Test minimal changes
- Risk: loses valuable architectural improvements

### If you want to ship v2.0.0 now → **Option D**
- Commit as experimental
- Ship with clear warnings
- Fix properly in v2.1.0

---

## My Recommendation: **Option A (Keep All, Feature Branch, Full Fix)**

**Reasoning**:
1. Current changes are **50% of the solution** (message ordering helps significantly)
2. Metadata sync architecture is **correct design** (will be needed anyway)
3. Investigation identified **exact root causes** (config, blocking, deserialization)
4. Comprehensive plan is **high confidence** based on deep analysis
5. Feature branch provides **safety net** (can abandon if needed)

**Timeline**:
- **Immediate**: Commit to feature branch (30 min)
- **Day 1**: Phase 1 + Phase 2 (8 hours)
- **Day 2**: Phase 3 (8 hours)
- **Day 3**: Phase 4 + Phase 5 (8 hours)
- **Result**: Stable, production-ready multi-node Raft for v2.0.0

**What you get**:
- ✅ All current work preserved and built upon
- ✅ Systematic fix of all identified issues
- ✅ Comprehensive test coverage
- ✅ Production-ready v2.0.0 with multi-node Raft

---

## Next Steps (Awaiting Your Decision)

**Please choose an option**:
- **A**: Keep all, feature branch, execute full plan (24h to stable Raft)
- **B**: Stash, test Phase 1 alone, then decide
- **C**: Cherry-pick minimal fixes only
- **D**: Commit as experimental, fix in v2.1.0

**Or suggest alternative approach**

Once you decide, I'll execute the chosen plan with detailed tracking and testing.
