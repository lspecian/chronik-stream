# Raft Cluster Testing Guide

**Critical lessons learned from debugging Raft cluster issues (v1.3.67 / v2.0.0)**

## Table of Contents
1. [Common Mistakes](#common-mistakes)
2. [Correct Test Setup](#correct-test-setup)
3. [Build Requirements](#build-requirements)
4. [Debugging Techniques](#debugging-techniques)
5. [What to Check When Tests Fail](#what-to-check-when-tests-fail)

---

## Common Mistakes

### ❌ MISTAKE #1: Forgetting to Specify Node IDs

**Problem**: All nodes default to `node_id=1` if not explicitly specified, causing:
- Vote responses sent to wrong destinations (`to=1` instead of `to=2` or `to=3`)
- Leader election never succeeds
- Asymmetric connectivity (node A can send to B, but B's responses go to itself)

**Symptom**:
```
Node 2 cast 78 votes
Node 1 received 0 responses from node 2
```

**Fix**: ALWAYS specify `--node-id N` BEFORE the `raft-cluster` subcommand:

```bash
# ✅ CORRECT
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \              # ← REQUIRED!
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap

# ❌ WRONG - Missing --node-id
./target/release/chronik-server \
  --kafka-port 9092 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap
```

**How to Verify**:
```bash
# Check each node's log for "Node ID:"
grep "Node ID:" ./test_logs/node*.log

# Should see:
# node1.log: Node ID: 1
# node2.log: Node ID: 2
# node3.log: Node ID: 3

# If you see all "Node ID: 1", you forgot --node-id!
```

---

### ❌ MISTAKE #2: Building Without the `raft` Feature

**Problem**: The `raft-cluster` subcommand is gated behind `#[cfg(feature = "raft")]`. Building without this feature results in:

```
error: unrecognized subcommand 'raft-cluster'
  tip: a similar subcommand exists: 'cluster'
```

**Fix**: ALWAYS build with `--features raft`:

```bash
# ✅ CORRECT
cargo build --release --bin chronik-server --features raft

# ❌ WRONG - Missing --features raft
cargo build --release --bin chronik-server
```

**How to Verify**:
```bash
# Check if raft-cluster subcommand exists
./target/release/chronik-server --help | grep raft-cluster

# Should see:
#   raft-cluster  Run as a node in a Raft-replicated cluster
```

---

### ❌ MISTAKE #3: Using Wrong Subcommand

**Problem**: Chronik has multiple cluster-related subcommands:

1. `cluster` - Mock CLI for cluster management (status, add-node, etc.) - **NOT for starting a Raft cluster**
2. `raft-cluster` - Starts a Raft cluster node (CORRECT for testing)
3. `all` mode with `--cluster-config` - Old API, rejects multi-node Raft
4. `standalone` mode - Single-node only, rejects multi-node Raft

**Fix**: Use `raft-cluster` subcommand for multi-node testing.

---

## Correct Test Setup

### Minimal Working 3-Node Cluster Test

See tests/phase5_simple_test.sh or tests/phase5_debug_test.sh for working examples.

Key requirements:
- Build with `--features raft`
- Specify `--node-id N` for each node (BEFORE raft-cluster subcommand)
- Use unique ports for Kafka, Raft gRPC, and metrics
- Wait between node starts (3s minimum)

---

## Testing Checklist

Before running any Raft cluster test:

- [ ] Built with `--features raft`
- [ ] Verified `raft-cluster` subcommand exists
- [ ] Killed all existing chronik processes
- [ ] Cleaned test data directories
- [ ] Each node has unique `--node-id N`
- [ ] Each node has unique `--kafka-port`
- [ ] Each node has unique `--data-dir`
- [ ] Each node has unique `--raft-addr`

**Last Updated**: 2025-10-25 (v2.0.0 Phase 1-4 validation)

---

## FAQ

### Q: Does building with `--features raft` break standalone mode?

**A: No!** Building with `--features raft` only **adds** the `raft-cluster` subcommand. It does NOT change or break existing modes:

✅ **All these modes work identically with or without `--features raft`:**
- `standalone` - Single-node Kafka server
- `ingest` - Ingest node
- `search` - Search node  
- `all` - All components

The only difference is you get an additional subcommand:
- `raft-cluster` - Multi-node Raft cluster (only available with `--features raft`)

**Recommendation**: Always build with `--features raft` to have maximum flexibility. You can still use standalone mode, but also have raft-cluster available when needed.

```bash
# ✅ RECOMMENDED: Build with raft support
cargo build --release --bin chronik-server --features raft

# Use standalone mode (works exactly the same)
./target/release/chronik-server standalone

# OR use raft-cluster mode (now available)
./target/release/chronik-server --node-id 1 raft-cluster --raft-addr ... --peers ...
```

### Q: Why is `--node-id` a global flag instead of a raft-cluster flag?

**A:** The `--node-id` is used across multiple modes (not just Raft):
- Identifies the node in cluster configurations
- Used in metrics and monitoring
- Required for broker registration in Kafka protocol

It's a global flag so it can be used by any mode that needs it, not just `raft-cluster`.

### Q: What happens if I forget `--node-id` in raft-cluster mode?

**A:** Currently, all nodes default to `node_id=1`, which causes leader election to fail because:
- All vote responses are sent `to=1` (to themselves)
- No node receives a quorum of votes
- Leader is never elected

**Future improvement**: Should add validation to require `--node-id` in raft-cluster mode.

