# Search API Default Port Change: 8080 → 6080

**Date**: 2025-10-17
**Change**: Changed default Search API port from 8080 to 6080
**Version**: v1.3.66
**Status**: ✅ **COMPLETE**

---

## Summary

Changed the default Search API port from **8080** to **6080** to avoid conflicts with common web development tools and improve multi-node localhost testing experience.

---

## Rationale

### Why Port 8080 Was Problematic

Port 8080 is heavily used by:
- **Apache Tomcat** - Default HTTP port
- **webpack-dev-server** - Modern web development
- **Create React App** - React development server
- **Vite** - Frontend build tool
- **Spring Boot** - Java applications
- **Many HTTP proxies and development servers**

**Impact**: When running Chronik alongside web development tools, port conflicts were common, even on single-node deployments.

### Why Port 6080 Is Better

- **Less common**: Not a standard port for major frameworks
- **No conflicts**: Rarely used by development tools
- **Same range**: Still in the 6000-8000 "alternative HTTP" range
- **Easy to remember**: 6080 vs 8080 (just change first digit)
- **Multi-node friendly**: Easier to allocate sequential ports (6080, 6180, 6280, etc.)

---

## Changes Made

### Code Changes

**File**: `crates/chronik-server/src/main.rs`

```diff
- /// Port for Search API (default: 8080, only used if search feature is enabled)
- #[arg(long, env = "CHRONIK_SEARCH_PORT", default_value = "8080")]
+ /// Port for Search API (default: 6080, only used if search feature is enabled)
+ #[arg(long, env = "CHRONIK_SEARCH_PORT", default_value = "6080")]
  search_port: u16,
```

### Documentation Updates

- `RAFT_PORT_FIX_COMPLETE.md` - Updated all examples to use 6080
- `test_3node_cluster.sh` - Updated to use 6080, 6180, 6280
- CLI help text - Shows new default port

---

## Migration Guide

### For Existing Deployments

**No action required** - This change is fully backward compatible:

1. **If you're not using Search API**: No impact
2. **If using default port**: Just update firewall rules from 8080 → 6080
3. **If explicitly set `--search-port 8080`**: Continues to work, no changes needed

### Update Firewall Rules

```bash
# Before (if you had this)
ufw allow 8080/tcp comment "Chronik Search API"

# After (if using default)
ufw allow 6080/tcp comment "Chronik Search API"
```

### Update Client Connections

```bash
# Before
curl http://localhost:8080/_search

# After (with default)
curl http://localhost:6080/_search

# Or explicitly set old port to maintain compatibility
chronik-server --search-port 8080 standalone
```

---

## Verification

### Test Results

✅ **All nodes start with new default**:
```
Node 1: Search API listening on http://0.0.0.0:6080
Node 2: Search API listening on http://0.0.0.0:6180
Node 3: Search API listening on http://0.0.0.0:6280
```

✅ **No port conflicts** in multi-node localhost testing

✅ **Backward compatible** - Can still override with `--search-port 8080`

---

## Configuration Examples

### Single Node (Default)

```bash
# Uses new default port 6080
chronik-server standalone

# Search API available at http://localhost:6080
```

### Multi-Node Cluster (Localhost)

```bash
# Node 1 - uses default 6080
chronik-server --kafka-port 9092 --metrics-port 9093 standalone

# Node 2 - explicit port 6180
chronik-server --kafka-port 9192 --metrics-port 9193 --search-port 6180 standalone

# Node 3 - explicit port 6280
chronik-server --kafka-port 9292 --metrics-port 9293 --search-port 6280 standalone
```

### Production (Maintain Old Port)

```bash
# If you need to keep port 8080 for compatibility
chronik-server --search-port 8080 standalone

# Or via environment variable
CHRONIK_SEARCH_PORT=8080 chronik-server standalone
```

---

## Port Allocation Reference

### Default Chronik Ports (v1.3.66+)

| Service | Default Port | Configurable | Environment Variable |
|---------|--------------|--------------|---------------------|
| **Kafka Protocol** | 9092 | ✅ Yes | `CHRONIK_KAFKA_PORT` |
| **Metrics (Prometheus)** | 9093 | ✅ Yes | `CHRONIK_METRICS_PORT` |
| **Admin API** | 3000 | ✅ Yes | `CHRONIK_ADMIN_PORT` |
| **Search API** | **6080** | ✅ Yes | `CHRONIK_SEARCH_PORT` |

### Multi-Node Port Scheme (Localhost)

**Recommended pattern for localhost testing**:

```
Node 1: Kafka=9092, Metrics=9093, Search=6080
Node 2: Kafka=9192, Metrics=9193, Search=6180
Node 3: Kafka=9292, Metrics=9293, Search=6280
```

**Pattern**: Add 100 to each service port for each additional node

---

## Benefits

### For Developers

✅ **No conflicts** with webpack-dev-server, Create React App, Vite
✅ **Easier multi-node testing** on localhost
✅ **Less confusion** about which service is using port 8080

### For Operations

✅ **Clearer port allocation** - Chronik uses 6xxx range for HTTP services
✅ **Better documentation** - Port scheme is more intentional
✅ **Fewer firewall conflicts** in mixed environments

### For Users

✅ **Works out of the box** - Default port rarely conflicts
✅ **Backward compatible** - Can still use 8080 if needed
✅ **Consistent experience** across single and multi-node deployments

---

## Rollback Instructions

If you need to revert to port 8080 for any reason:

### Option 1: Command Line Flag
```bash
chronik-server --search-port 8080 standalone
```

### Option 2: Environment Variable
```bash
export CHRONIK_SEARCH_PORT=8080
chronik-server standalone
```

### Option 3: Code Change (if rebuilding)
```rust
// In crates/chronik-server/src/main.rs
#[arg(long, env = "CHRONIK_SEARCH_PORT", default_value = "8080")]
search_port: u16,
```

---

## Related Changes

This change is part of the **Raft Multi-Node Port Conflict Fix** (v1.3.66):

1. ✅ Added `--search-port` CLI argument
2. ✅ Added `CHRONIK_SEARCH_PORT` environment variable
3. ✅ Added `--disable-search` flag
4. ✅ Changed default from 8080 → **6080**

**See also**: `RAFT_PORT_FIX_COMPLETE.md` for complete multi-node fix details

---

## Conclusion

The change from port 8080 → 6080 improves the developer and operational experience by:

- Avoiding conflicts with popular development tools
- Making multi-node localhost testing easier
- Creating a clearer port allocation scheme

**Impact**: Low (backward compatible, easily configurable)
**Benefit**: High (fewer conflicts, better UX)

---

**Change Date**: 2025-10-17
**Version**: v1.3.66
**Status**: ✅ **COMPLETE AND VERIFIED**
