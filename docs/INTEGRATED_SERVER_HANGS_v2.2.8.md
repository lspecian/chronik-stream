# IntegratedKafkaServer::new() Hangs in Cluster Mode (v2.2.8)

**Date:** 2025-11-09
**Severity:** CRITICAL - BLOCKING
**Status:** 🐛 ROOT CAUSE IDENTIFIED

---

## Summary

`IntegratedKafkaServer::new()` **never returns** in cluster mode, blocking all subsequent initialization including broker registration. This causes the cluster to be unusable.

---

## Evidence

```bash
# Logs show init starts but NEVER completes
[06:26:59] INFO Starting Chronik in CLUSTER mode (node_id=1)
[06:26:59] INFO Initializing IntegratedKafkaServer...
[06:26:59] INFO Starting internal server initialization with Raft: true
[06:26:59] INFO Multi-node mode: Deferring broker registration until Raft leader election completes
... (WAL recovery, leader election, etc. - all complete)
# ❌ NEVER SEE: "IntegratedKafkaServer initialized successfully"
```

**Expected (from main.rs line 640):**
```rust
let server = IntegratedKafkaServer::new(server_config, Some(raft_cluster.clone())).await?;
info!("IntegratedKafkaServer initialized successfully");  // ❌ NEVER LOGGED
```

**What happens instead:**
- `new()` call starts (line 639)
- Internal initialization completes
- Function NEVER returns
- Code after line 640 NEVER executes
- Broker registration (lines 650-697) NEVER runs

---

## Impact Chain

1. ❌ `IntegratedKafkaServer::new()` hangs
2. ❌ Broker registration never happens (code unreachable)
3. ❌ Kafka clients timeout (no brokers in metadata)
4. ❌ Cluster completely unusable

---

## Root Cause (FOUND!)

**CHICKEN-AND-EGG DEADLOCK** in Raft initialization:

1. `IntegratedKafkaServer::new()` waits for Raft leader election (line 393-416)
2. Leader election requires Raft message loop to be running
3. Raft message loop starts in `main.rs` AFTER `IntegratedKafkaServer::new()` returns (line 580+)
4. **DEADLOCK**: `new()` never returns → message loop never starts → election never completes → `new()` never returns!

**Evidence:**
```bash
# Last log before hang:
[INFO] Multi-node mode: Deferring broker registration until Raft leader election completes
[INFO] Waiting for Raft leader election before proposing metadata...
# ❌ NEVER COMPLETES - hangs forever in 30-second wait loop
```

**Code location:**
- [integrated_server.rs:390-416](../crates/chronik-server/src/integrated_server.rs#L390-L416) - Blocking wait loop
- [main.rs:580+](../crates/chronik-server/src/main.rs#L580) - Raft message loop start (unreachable!)

**The Fix (v2.2.8):**
- REMOVE Raft leader wait from `IntegratedKafkaServer::new()`
- MOVE partition metadata initialization to `main.rs` AFTER Raft message loop starts
- Allows `new()` to complete → message loop starts → election happens → metadata initializes

---

## How to Verify

```bash
# Add debug logging to integrated_server.rs::new()
info!("Step 1: Creating metadata store");
let metadata_store = ...;
info!("Step 2: Creating storage service");
let storage = ...;
info!("Step 3: Creating produce handler");
let produce_handler = ...;
info!("FINAL: Returning server instance");
Ok(Self { ... })
```

Run cluster, check which step never completes.

---

## Workaround

**NONE** - This blocks all cluster functionality.

---

## Priority

**CRITICAL P0** - Nothing works until this is fixed.

Must fix BEFORE:
- Broker registration fix
- Auto-create topic fix
- Benchmarking
- ANY cluster testing

