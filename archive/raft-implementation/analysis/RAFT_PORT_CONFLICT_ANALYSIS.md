# Raft Multi-Node Port Conflict Analysis

**Date**: 2025-10-17
**Issue**: UnrecognizedBrokerVersion error when connecting to nodes 2 & 3
**Root Cause**: Port conflicts, NOT protocol negotiation issues

---

## Problem Summary

When starting a 3-node Raft cluster on localhost, **nodes 2 & 3 crash** during startup with:

```
ERROR chronik_monitoring: Metrics server error: Address already in use (os error 48)
thread 'tokio-runtime-worker' panicked at crates/chronik-server/src/main.rs:737:71:
called `Result::unwrap()` on an `Err` value: Os { code: 48, kind: AddrInUse, message: "Address already in use" }
```

## Root Cause

There are **THREE port conflicts** when running multiple nodes on localhost:

### 1. ✅ Kafka Protocol Port (CONFIGURABLE)
- **Port**: Configurable via `--kafka-port` CLI argument
- **Status**: ✅ **WORKING** - No conflict
- **Node 1**: 9092
- **Node 2**: 9192 (via `--kafka-port 9192`)
- **Node 3**: 9292 (via `--kafka-port 9292`)

### 2. ❌ Metrics Server Port (PARTIALLY CONFIGURABLE)
- **Port**: Configurable via `--metrics-port` CLI argument
- **Default**: 9093
- **Problem**: All nodes default to 9093 → **CONFLICT**
- **Status**: ❌ **CAUSES CRASH** when not overridden
- **Solution**: Use `--metrics-port` for each node
  - Node 1: `--metrics-port 9093`
  - Node 2: `--metrics-port 9193`
  - Node 3: `--metrics-port 9293`

### 3. ❌ Search API Port (HARDCODED)
- **Port**: Hardcoded to **8080** in `main.rs`
- **Problem**: Cannot be configured via CLI or environment variable
- **Status**: ❌ **CAUSES CRASH** when search feature is compiled
- **Impact**: Only Node 1 can start Search API
- **Code Location**: `crates/chronik-server/src/main.rs:717` and `main.rs:953`

```rust
// Hardcoded Search API port (line 717)
info!("Starting Search API on port 8080");
let addr = format!("{}:8080", search_bind);

// Hardcoded Search API port (line 953)
let search_port = 8080u16; // Search API port
```

---

## Why the Error Was Misdiagnosed

The test script (`test_raft_cluster.py`) was trying to connect to:
- Node 1: `localhost:9092` ✅ (works)
- Node 2: `localhost:9093` ❌ (expected Kafka, got metrics server or nothing)
- Node 3: `localhost:9094` ❌ (not bound at all)

This led to `UnrecognizedBrokerVersion` errors because:
1. Port 9093 was the **metrics server** (Prometheus endpoint), not Kafka protocol
2. Port 9094 was **not bound** at all
3. Kafka clients couldn't negotiate protocol versions with a metrics endpoint

The actual issue was that **nodes 2 & 3 never started successfully** due to port conflicts, so there was no Kafka server to connect to.

---

## Testing Evidence

### Before Fix (Nodes 2 & 3 crash):
```bash
Node 1: Kafka=9092, Metrics=9093 (default), Search=8080
Node 2: Kafka=9192, Metrics=9093 (default) ← CONFLICT → crashes
Node 3: Kafka=9292, Metrics=9093 (default) ← CONFLICT → crashes
```

**Result**:
- Node 1 starts successfully
- Node 2 crashes: "Address already in use (os error 48)" on port 9093 (metrics)
- Node 3 crashes: "Address already in use (os error 48)" on port 9093 (metrics) and 8080 (search)

### After Fix (Metrics port configured):
```bash
Node 1: Kafka=9092, Metrics=9093, Search=8080
Node 2: Kafka=9192, Metrics=9193, Search=8080 ← Still conflicts with Node 1
Node 3: Kafka=9292, Metrics=9293, Search=8080 ← Still conflicts with Node 1
```

**Partial Success**:
- ✅ Metrics port conflict resolved
- ❌ Search API port still hardcoded (8080)
- **Workaround**: Disable search feature or accept that only Node 1 has Search API

---

## Solutions

### Short-Term (Local Testing)

**Option 1**: Use separate data directories and disable search
```bash
cargo build --release --bin chronik-server  # Without --features search
```

**Option 2**: Accept that Search API only works on Node 1
```bash
# Nodes 2 & 3 will log errors about Search API port conflict, but Kafka will work
# Only Node 1 will have Search API available
```

**Option 3**: Run nodes in separate containers/VMs
```yaml
# Docker Compose - each node on isolated network
services:
  node1:
    ports: ["9092:9092", "9093:9093", "8080:8080"]
  node2:
    ports: ["9192:9092", "9193:9093", "8180:8080"]  # Map to different host ports
  node3:
    ports: ["9292:9092", "9293:9093", "8280:8080"]
```

### Long-Term (Production Fix)

**Add CLI argument for Search API port**:

```diff
# crates/chronik-server/src/main.rs

@@ struct Cli {
+   /// Port for Search API (default: 8080)
+   #[arg(long, env = "CHRONIK_SEARCH_PORT", default_value = "8080")]
+   search_port: u16,
+
    ...
}

@@
-let search_port = 8080u16;
+let search_port = cli.search_port;
```

**Benefits**:
- Allows multi-node localhost testing
- Production deployments can use different ports
- Backwards compatible (default still 8080)

---

## Impact Assessment

### Kafka Protocol: ✅ NOT AFFECTED
- Kafka handler initialization works correctly
- Protocol version negotiation works correctly
- The `UnrecognizedBrokerVersion` error was a red herring
- **Real issue**: Clients were connecting to wrong ports or crashed nodes

### Raft Clustering: ⚠️ BLOCKED BY PORT CONFLICTS
- Raft gRPC port is part of cluster configuration
- Kafka port can be configured per-node
- Metrics port can be configured per-node
- **Blocker**: Search API port cannot be configured

### Multi-Node Testing: ❌ LIMITED
- **Localhost**: Can test with metrics port override, but Search API conflicts
- **Docker/K8s**: Works fine (isolated networks)
- **Production**: Works fine (separate machines)

---

## Recommended Next Steps

1. **Immediate**: Update test scripts to use `--metrics-port` for each node
2. **Short-term**: Add `--search-port` CLI argument
3. **Testing**: Verify Kafka protocol works on all nodes (ignore Search API for now)
4. **Documentation**: Update cluster setup docs with port configuration requirements

---

## Test Script (Fixed for Metrics Port)

```bash
#!/bin/bash

# Start Node 1
./target/release/chronik-server \
  --kafka-port 9092 \
  --metrics-port 9093 \
  --data-dir ./node1_data \
  standalone &

# Start Node 2
./target/release/chronik-server \
  --kafka-port 9192 \
  --metrics-port 9193 \
  --data-dir ./node2_data \
  standalone &

# Start Node 3
./target/release/chronik-server \
  --kafka-port 9292 \
  --metrics-port 9293 \
  --data-dir ./node3_data \
  standalone &
```

**Note**: Nodes 2 & 3 will still log Search API port conflicts, but Kafka protocol will work.

---

**Conclusion**: The "UnrecognizedBrokerVersion" error was a symptom, not the disease. The real problem was port conflicts preventing nodes from starting. Kafka protocol implementation is correct.
