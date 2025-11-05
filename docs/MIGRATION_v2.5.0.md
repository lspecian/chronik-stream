# Migration Guide: v2.4.x → v2.5.0

**Breaking Changes**: CLI redesigned for simplicity. No backward compatibility.

---

## Overview

Chronik v2.5.0 introduces a dramatically simplified CLI:
- **16 flags → 2 flags** (88% reduction)
- **5 subcommands → 1 main command** (`start`)
- **All ports in config file** (no more CLI flag confusion)
- **Auto-detection** (single-node vs cluster)

**Migration Time**: < 5 minutes per deployment

---

## What Changed

### CLI Structure

**Before (v2.4.x)**:
```bash
chronik-server [16 FLAGS] [SUBCOMMAND]
```

**After (v2.5.0)**:
```bash
chronik-server [2 FLAGS] <COMMAND>
```

### Commands

| v2.4.x | v2.5.0 | Notes |
|--------|--------|-------|
| `standalone` | `start` | Auto-detects single-node mode |
| `raft-cluster` | `start --config` | Auto-detects cluster mode from config |
| `ingest` | **REMOVED** | Never implemented |
| `search` | **REMOVED** | Never implemented |
| `all` | **REMOVED** | Confusing, removed |
| `version` | `version` | Unchanged |
| `compact` | `compact` | Unchanged |
| N/A | `cluster` | **NEW**: Zero-downtime operations (Phase 3) |

---

## Migration Examples

### 1. Single-Node Startup

**Before (v2.4.x)**:
```bash
./chronik-server --advertised-addr localhost standalone
```

**After (v2.5.0)**:
```bash
./chronik-server start --advertise localhost:9092
```

**Changes**:
- ✅ Removed `standalone` subcommand
- ✅ `--advertised-addr` → `--advertise`
- ✅ Can now specify port in advertise address

### 2. Cluster Startup (3 Nodes)

**Before (v2.4.x)**:
```bash
# Node 1
./chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  raft-cluster \
  --raft-addr 0.0.0.0:5001 \
  --peers "2@localhost:5002,3@localhost:5003" \
  --bootstrap

# Node 2
./chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  raft-cluster \
  --raft-addr 0.0.0.0:5002 \
  --peers "1@localhost:5001,3@localhost:5003" \
  --bootstrap

# Node 3
./chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  raft-cluster \
  --raft-addr 0.0.0.0:5003 \
  --peers "1@localhost:5001,2@localhost:5002" \
  --bootstrap
```

**After (v2.5.0)**:

1. **Create cluster config file** (cluster.toml):
```toml
# Node 1
node_id = 1  # Change to 2, 3 on other nodes

replication_factor = 3
min_insync_replicas = 2

[node.addresses]
kafka = "0.0.0.0:9092"  # Node 2: 9093, Node 3: 9094
wal = "0.0.0.0:9291"    # Node 2: 9292, Node 3: 9293
raft = "0.0.0.0:5001"   # Node 2: 5002, Node 3: 5003

[node.advertise]
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 1
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 2
kafka = "localhost:9093"
wal = "localhost:9292"
raft = "localhost:5002"

[[peers]]
id = 3
kafka = "localhost:9094"
wal = "localhost:9293"
raft = "localhost:5003"
```

2. **Start nodes** (much simpler!):
```bash
# Node 1
./chronik-server start --config cluster.toml

# Node 2 (copy config, change node_id=2 and ports)
./chronik-server start --config cluster-node2.toml

# Node 3 (copy config, change node_id=3 and ports)
./chronik-server start --config cluster-node3.toml
```

**Benefits**:
- ✅ 11 fewer flags per node
- ✅ All configuration in reviewable TOML file
- ✅ No need to remember peer format (`id@host:port`)
- ✅ Clear bind vs advertise separation

### 3. Environment Variable Configuration

**Before (v2.4.x)**:
```bash
export CHRONIK_KAFKA_PORT=9092
export CHRONIK_REPLICATION_FOLLOWERS="localhost:9291,localhost:9292"
export CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9291"

./chronik-server standalone
```

**After (v2.5.0)**:
```bash
export CHRONIK_NODE_ID=1
export CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9291:5001,node3:9092:9291:5001"
export CHRONIK_REPLICATION_FACTOR=3
export CHRONIK_MIN_INSYNC_REPLICAS=2

./chronik-server start
```

**Changes**:
- ❌ `CHRONIK_KAFKA_PORT` → Use config file or accept default 9092
- ❌ `CHRONIK_REPLICATION_FOLLOWERS` → Automatically discovered from cluster
- ❌ `CHRONIK_WAL_RECEIVER_ADDR` → Now in config file as `wal` address
- ✅ `CHRONIK_CLUSTER_PEERS` → New format: `host:kafka_port:wal_port:raft_port`

---

## Removed Flags

### Global Flags (REMOVED)

| Flag | Replacement | Migration |
|------|-------------|-----------|
| `--kafka-port` | Config file | Add to `[node.addresses]` |
| `--admin-port` | **REMOVED** | Never used |
| `--metrics-port` | Config file | Add to `[node.addresses]` (optional) |
| `--search-port` | Config file | Add to `[node.addresses]` (optional) |
| `--bind-addr` | `--bind` | Rename (shorter, clearer) |
| `--advertised-addr` | `--advertise` | Merge addr+port into one |
| `--advertised-port` | `--advertise` | Merge addr+port into one |
| `--cluster-config` | `--config` | Rename (shorter) |

### Subcommand-Specific Flags (REMOVED)

| Flag | Replacement |
|------|-------------|
| `--raft-addr` | Config file `[node.addresses].raft` |
| `--peers` | Config file `[[peers]]` sections |
| `--bootstrap` | Automatic (first node with `--config`) |

---

## Deprecated Environment Variables

These environment variables still work but trigger **deprecation warnings**:

| Old Var | Status | Migration |
|---------|--------|-----------|
| `CHRONIK_KAFKA_PORT` | **DEPRECATED** | Use config file |
| `CHRONIK_REPLICATION_FOLLOWERS` | **DEPRECATED** | Auto-discovered in v2.5.0 |
| `CHRONIK_WAL_RECEIVER_ADDR` | **DEPRECATED** | Use config `[node.addresses].wal` |
| `CHRONIK_WAL_RECEIVER_PORT` | **DEPRECATED** | Use config `[node.addresses].wal` |

**Warning message example**:
```
WARN chronik_server: CHRONIK_KAFKA_PORT is deprecated. Use cluster config file instead.
```

---

## New Environment Variables

| Var | Purpose | Format |
|-----|---------|--------|
| `CHRONIK_CONFIG` | Path to cluster config | `/path/to/cluster.toml` |
| `CHRONIK_BIND` | Bind address for all services | `0.0.0.0` (default) |
| `CHRONIK_ADVERTISE` | Advertised address for clients | `hostname:port` |
| `CHRONIK_NODE_ID` | Node ID (overrides config) | `1`, `2`, `3`, etc. |
| `CHRONIK_CLUSTER_PEERS` | Env-based cluster config | `host:9092:9291:5001,...` |

---

## Config File Format

### Location

- **Recommended**: `cluster.toml` in project root
- **Alternative**: Any path via `--config` or `CHRONIK_CONFIG`

### Structure

```toml
# Node Identity
node_id = 1  # MUST be unique per node

# Replication
replication_factor = 3
min_insync_replicas = 2

# This Node's Bind Addresses (where to listen)
[node.addresses]
kafka = "0.0.0.0:9092"    # Kafka API
wal = "0.0.0.0:9291"      # WAL replication
raft = "0.0.0.0:5001"     # Raft consensus
metrics = "0.0.0.0:13092" # Metrics (optional)
search = "0.0.0.0:6092"   # Search (optional)

# This Node's Advertised Addresses (what clients connect to)
[node.advertise]
kafka = "node1.example.com:9092"
wal = "node1.example.com:9291"
raft = "node1.example.com:5001"

# All Cluster Peers (including this node)
[[peers]]
id = 1
kafka = "node1.example.com:9092"
wal = "node1.example.com:9291"
raft = "node1.example.com:5001"

[[peers]]
id = 2
kafka = "node2.example.com:9092"
wal = "node2.example.com:9291"
raft = "node2.example.com:5001"

[[peers]]
id = 3
kafka = "node3.example.com:9092"
wal = "node3.example.com:9291"
raft = "node3.example.com:5001"
```

### Validation

Config is validated on load:
- ✅ `node_id` must exist in `[[peers]]` list
- ✅ No duplicate node IDs
- ✅ No duplicate addresses (kafka, wal, raft)
- ✅ `min_insync_replicas <= replication_factor <= peer_count`

**Error example**:
```
Error: Invalid cluster configuration: node_id 5 not found in peers list
```

---

## Docker Changes

**Before (v2.4.x)**:
```bash
docker run -d -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=chronik-server \
  chronik-stream
```

**After (v2.5.0)**:
```bash
docker run -d -p 9092:9092 \
  -e CHRONIK_ADVERTISE=chronik-server:9092 \
  chronik-stream
```

**Changes**:
- ✅ `CHRONIK_ADVERTISED_ADDR` → `CHRONIK_ADVERTISE`
- ✅ Can specify port in advertise address

---

## Kubernetes/Helm Changes

Update your Helm values or K8s manifests:

**Before (v2.4.x)**:
```yaml
args:
  - standalone
env:
  - name: CHRONIK_KAFKA_PORT
    value: "9092"
  - name: CHRONIK_ADVERTISED_ADDR
    valueFrom:
      fieldRef:
        fieldPath: status.podIP
```

**After (v2.5.0)**:
```yaml
args:
  - start
  - --advertise
  - $(POD_IP):9092
env:
  - name: POD_IP
    valueFrom:
      fieldRef:
        fieldPath: status.podIP
```

**For cluster mode**:
```yaml
args:
  - start
  - --config
  - /etc/chronik/cluster.toml
volumeMounts:
  - name: config
    mountPath: /etc/chronik
volumes:
  - name: config
    configMap:
      name: chronik-cluster-config
```

---

## Testing Your Migration

### 1. Single-Node Test

```bash
# Start server
./chronik-server start --advertise localhost:9092

# Test with Kafka clients
kafka-console-producer --bootstrap-server localhost:9092 --topic test
kafka-console-consumer --bootstrap-server localhost:9092 --topic test --from-beginning
```

### 2. Cluster Test

```bash
# Start 3 nodes (3 terminals)
./chronik-server start --config cluster-node1.toml
./chronik-server start --config cluster-node2.toml
./chronik-server start --config cluster-node3.toml

# Test with Kafka clients
kafka-console-producer --bootstrap-server localhost:9092,localhost:9093,localhost:9094 --topic test
kafka-console-consumer --bootstrap-server localhost:9092,localhost:9093,localhost:9094 --topic test --from-beginning
```

---

## Rollback Plan

If you need to rollback to v2.4.x:

1. **Stop v2.5.0 servers**:
```bash
# Find processes
ps aux | grep chronik-server

# Stop gracefully
kill -TERM <pid>
```

2. **Switch to v2.4.x binary**:
```bash
git checkout v2.4.x
cargo build --release --bin chronik-server
```

3. **Use old CLI**:
```bash
./target/release/chronik-server --advertised-addr localhost standalone
```

**Data compatibility**: WAL format unchanged, data is compatible.

---

## Common Issues

### Issue: "No field `kafka_port` on type `Cli`"

**Cause**: Using old CLI syntax with new binary

**Fix**: Update to new CLI:
```bash
# Old
./chronik-server --kafka-port 9092 standalone

# New
./chronik-server start  # Uses default port 9092
```

### Issue: "Invalid peer format: expected 'host:kafka_port:wal_port:raft_port'"

**Cause**: Using old 3-field env var format

**Fix**: Update `CHRONIK_CLUSTER_PEERS`:
```bash
# Old (3 fields)
export CHRONIK_CLUSTER_PEERS="node1:9092:5001,node2:9092:5002"

# New (4 fields)
export CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9292:5002"
```

### Issue: Deprecation warnings flooding logs

**Cause**: Old environment variables still set

**Fix**: Unset deprecated vars:
```bash
unset CHRONIK_KAFKA_PORT
unset CHRONIK_REPLICATION_FOLLOWERS
unset CHRONIK_WAL_RECEIVER_ADDR
unset CHRONIK_WAL_RECEIVER_PORT
```

---

## Support

**Documentation**:
- [CLAUDE.md](../CLAUDE.md) - Updated with v2.5.0 examples
- [examples/cluster-3node.toml](../examples/cluster-3node.toml) - Production config template
- [examples/cluster-local-3node.toml](../examples/cluster-local-3node.toml) - Local testing template

**Help**:
```bash
chronik-server --help
chronik-server start --help
chronik-server cluster --help
```

---

## Summary

**Complexity Reduction**:
- 88% fewer flags (16 → 2)
- 43% fewer commands (7 → 4)
- 100% fewer required env vars (6+ → 0)

**Migration Effort**:
- Single-node: 1 minute (change command)
- Cluster: 5 minutes (create config file)

**Breaking Changes Justified**:
- Userbase = just us (development team)
- Clean break enables best design
- Much simpler for new users

---

**Version**: v2.5.0
**Date**: 2025-11-02
**Status**: Final
