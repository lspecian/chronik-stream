# Raft Multi-Node Port Conflict Fix - COMPLETE

**Date**: 2025-10-17
**Issue**: UnrecognizedBrokerVersion error (misdiagnosed)
**Root Cause**: Port conflicts preventing nodes 2 & 3 from starting
**Status**: ✅ **FIXED AND VERIFIED**

---

## Summary

The reported `UnrecognizedBrokerVersion` error was a red herring. The actual problem was **port conflicts** causing nodes 2 & 3 to crash during startup, before they could accept Kafka connections.

### Root Cause

Three services tried to bind to the same ports across all nodes:

1. **Kafka Protocol** - ✅ Configurable via `--kafka-port` (was working)
2. **Metrics Server** - ⚠️ Configurable via `--metrics-port` (but defaulted to 9093 on all nodes)
3. **Search API** - ❌ Hardcoded to port 6080 (NOT configurable)

When nodes 2 & 3 tried to start:
- Metrics port 9093 → Already in use by Node 1 → **CRASH**
- Search API port 6080 → Already in use by Node 1 → **CRASH**

The crashes happened AFTER Kafka handlers initialized, so test clients connecting to wrong ports received connection errors that looked like protocol issues.

---

## Fix Applied

Added two new CLI arguments and environment variables:

### 1. `--search-port` / `CHRONIK_SEARCH_PORT`

**Purpose**: Configure Search API port
**Default**: 6080
**Type**: u16
**Rationale**: Port 6080 chosen to avoid conflicts with common web development tools (8080 often used by Tomcat, webpack-dev-server, Create React App, etc.)

**Usage**:
```bash
# CLI argument
chronik-server --search-port 8180 standalone

# Environment variable
CHRONIK_SEARCH_PORT=8180 chronik-server standalone
```

### 2. `--disable-search` / `CHRONIK_DISABLE_SEARCH`

**Purpose**: Disable Search API entirely
**Default**: false (Search API enabled if search feature compiled)
**Type**: bool

**Usage**:
```bash
# CLI argument
chronik-server --disable-search standalone

# Environment variable
CHRONIK_DISABLE_SEARCH=true chronik-server standalone
```

---

## Implementation Details

**Files Modified**:
- `crates/chronik-server/src/main.rs`:
  - Added `search_port: u16` field to `Cli` struct (line 107-108)
  - Added `disable_search: bool` field to `Cli` struct (line 110-112)
  - Updated Search API binding in `run_standalone_server()` (line 722-757)
  - Updated Search API binding in `run_all_mode()` (line 959-995)

**Code Changes**:

```rust
// CLI struct additions
struct Cli {
    // ... existing fields ...

    /// Port for Search API (default: 6080, only used if search feature is enabled)
    #[arg(long, env = "CHRONIK_SEARCH_PORT", default_value = "6080")]
    search_port: u16,

    /// Disable Search API (default: false, search API enabled if search feature compiled)
    #[arg(long, env = "CHRONIK_DISABLE_SEARCH", default_value = "false")]
    disable_search: bool,
}
```

```rust
// Search API binding (before)
#[cfg(feature = "search")]
{
    let addr = format!("{}:6080", search_bind);  // Hardcoded!
    // ...
}

// Search API binding (after)
#[cfg(feature = "search")]
if !cli.disable_search {
    let search_port = cli.search_port;  // Configurable!
    let addr = format!("{}:{}", search_bind, search_port);
    // ...
} else {
    #[cfg(feature = "search")]
    info!("Search API disabled via --disable-search flag");
}
```

---

## Testing Results

### Test Setup

**3-node cluster on localhost**:
```bash
# Node 1
chronik-server --kafka-port 9092 --metrics-port 9093 --search-port 6080 standalone

# Node 2
chronik-server --kafka-port 9192 --metrics-port 9193 --search-port 8180 standalone

# Node 3
chronik-server --kafka-port 9292 --metrics-port 9293 --search-port 8280 standalone
```

### Test Results

✅ **All nodes started successfully**:
```
Node 1 (PID 81458) is running
Node 2 (PID 81470) is running
Node 3 (PID 81480) is running
```

✅ **All Kafka ports accessible**:
```
Node 1 Kafka port is open (9092)
Node 2 Kafka port is open (9192)
Node 3 Kafka port is open (9292)
```

✅ **Kafka protocol works on all nodes**:
```
Node 1 (:9092) - Connected, 3 topics
Node 2 (:9192) - Connected, 3 topics
Node 3 (:9292) - Connected, 3 topics
```

✅ **Produce/Consume works**:
```
Produced 100 messages to Node 1
Consumed 100 messages from Node 1
```

⚠️ **Raft replication not yet active** (expected):
```
Node 2 consumed: 0 messages (replication not implemented)
Node 3 consumed: 0 messages (replication not implemented)
```

---

## Impact Assessment

### Before Fix

| Component | Status | Issue |
|-----------|--------|-------|
| Kafka Handler | ✅ Working | Properly initialized |
| Protocol Negotiation | ✅ Working | ApiVersions working |
| Node 1 Startup | ✅ Success | First to start, no conflicts |
| Node 2 Startup | ❌ **CRASH** | Metrics port conflict |
| Node 3 Startup | ❌ **CRASH** | Metrics + Search port conflicts |
| Multi-Node Testing | ❌ **BLOCKED** | Nodes 2 & 3 never started |

### After Fix

| Component | Status | Issue |
|-----------|--------|-------|
| Kafka Handler | ✅ Working | All 3 nodes accept connections |
| Protocol Negotiation | ✅ Working | All 3 nodes negotiate correctly |
| Node 1 Startup | ✅ Success | Kafka=9092, Metrics=9093, Search=6080 |
| Node 2 Startup | ✅ Success | Kafka=9192, Metrics=9193, Search=8180 |
| Node 3 Startup | ✅ Success | Kafka=9292, Metrics=9293, Search=8280 |
| Multi-Node Testing | ✅ **UNBLOCKED** | Ready for Raft implementation |

---

## Usage Examples

### Example 1: Standard 3-Node Cluster (Different Ports)

```bash
# Node 1
CHRONIK_NODE_ID=1 \
CHRONIK_KAFKA_PORT=9092 \
CHRONIK_METRICS_PORT=9093 \
CHRONIK_SEARCH_PORT=6080 \
chronik-server standalone

# Node 2
CHRONIK_NODE_ID=2 \
CHRONIK_KAFKA_PORT=9192 \
CHRONIK_METRICS_PORT=9193 \
CHRONIK_SEARCH_PORT=8180 \
chronik-server standalone

# Node 3
CHRONIK_NODE_ID=3 \
CHRONIK_KAFKA_PORT=9292 \
CHRONIK_METRICS_PORT=9293 \
CHRONIK_SEARCH_PORT=8280 \
chronik-server standalone
```

### Example 2: Production Cluster (Separate Machines)

```bash
# All nodes can use same ports (isolated networks)
# Node 1 on machine 10.0.1.10
chronik-server --kafka-port 9092 --metrics-port 9093 --search-port 6080 standalone

# Node 2 on machine 10.0.1.11
chronik-server --kafka-port 9092 --metrics-port 9093 --search-port 6080 standalone

# Node 3 on machine 10.0.1.12
chronik-server --kafka-port 9092 --metrics-port 9093 --search-port 6080 standalone
```

### Example 3: Disable Search API (Save Resources)

```bash
# Disable Search API on follower nodes
chronik-server --disable-search standalone

# Or via environment variable
CHRONIK_DISABLE_SEARCH=true chronik-server standalone
```

---

## Backward Compatibility

✅ **Fully backward compatible**:

- `--search-port` defaults to 6080 (same as before)
- `--disable-search` defaults to false (Search API enabled)
- Existing deployments work without changes
- Only affects multi-node localhost testing

---

## Next Steps

Now that all nodes can start successfully:

1. ✅ **Port Conflicts** - Fixed with `--search-port` flag
2. ⚠️ **Raft Partition Assignment** - Still needs implementation
   - Currently `has_replica()` returns false for all partitions
   - Need to implement partition → replica mapping
3. ⚠️ **Raft Consensus Path** - Ready to test
   - Code exists but never executes (no replicas assigned)
   - Needs integration testing once partitions assigned
4. ⚠️ **Multi-Node Data Replication** - Not yet active
   - Nodes 2 & 3 don't receive replicated data
   - Expected until Raft partition assignment implemented

---

## Conclusion

The `UnrecognizedBrokerVersion` error was completely unrelated to protocol negotiation. It was a simple port conflict issue that:

1. Prevented nodes 2 & 3 from starting
2. Caused test clients to connect to wrong ports or crashed nodes
3. Resulted in connection errors that looked like protocol failures

**Fix verified**: All 3 nodes now start successfully and accept Kafka protocol connections.

**Raft clustering**: Unblocked and ready for partition assignment implementation.

---

**Report Date**: 2025-10-17 06:46 UTC
**Fix Version**: v1.3.66 (to be tagged)
**Status**: ✅ **COMPLETE AND VERIFIED**
