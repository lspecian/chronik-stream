# Raft Cluster Mode Testing Status

**Created**: 2025-10-23
**Session**: Deep investigation of multi-node Raft clustering
**Status**: ✅ **raft-cluster mode is complete and ready to test**

## Executive Summary

After deep investigation, we discovered that:

1. ✅ **raft-cluster mode IS complete** - All necessary CLI flags exist
2. ✅ **All callback fixes are in place** - Ready for multi-node testing
3. ⚠️ **Just needs correct CLI syntax** - Global options must come BEFORE subcommand

## What Was Tested and Fixed Today

### ✅ Successfully Tested
- **Java Kafka Client Compatibility** - kafka-console-producer/consumer work perfectly with standalone mode
- **Standalone server** - Fully functional with WAL, recovery, and all Kafka APIs

### ✅ Bugs Fixed (4 Critical Issues)

#### 1. leader_id=0 Forwarding Crash
- **File**: `crates/chronik-raft/src/raft_meta_log.rs:689-713`
- **Issue**: When Raft has no leader (leader_id=0), code tried to forward to non-existent peer 0
- **Fix**: Added check for `leader_id == 0` to return "No leader elected yet" error
- **Impact**: Graceful error handling during leader election

#### 2. Topic Creation Callback Not Firing
- **File**: `crates/chronik-raft/src/raft_meta_log.rs:346-353`
- **Issue**: Callback only fired for `CreateTopicWithAssignments`, not `CreateTopic`
- **Fix**: Changed matching to fire for BOTH operation types
- **Impact**: Ensures Raft replicas created when topics auto-created via produce

#### 3. Missing Callback Implementation
- **File**: `crates/chronik-server/src/integrated_server.rs:216-284`
- **Issue**: MetadataStateMachine created without topic creation callback
- **Fix**: Implemented comprehensive callback that:
  - Creates WAL-backed Raft log storage for each partition
  - Calls `raft_mgr.create_replica()` for all partitions
  - Runs asynchronously via `tokio::spawn()`
- **Impact**: Data partition replicas automatically created

#### 4. TopicCreatedCallback Not Exported
- **File**: `crates/chronik-raft/src/lib.rs:94`
- **Issue**: Type defined but not in public API
- **Fix**: Added to public exports
- **Impact**: Type accessible for callbacks

### ✅ Architecture Decision
- **Multi-node standalone mode blocked** with clear error message
- **File**: `crates/chronik-server/src/integrated_server.rs:174-198`
- **Reason**: Standalone lacks distributed bootstrap coordination
- **Solution**: Direct users to `raft-cluster` mode for multi-node

## raft-cluster Mode - Complete Implementation

### CLI Structure (CRITICAL: Order Matters!)

**Global options must come BEFORE the subcommand:**

```bash
chronik-server [GLOBAL_OPTIONS] raft-cluster [RAFT_OPTIONS]
```

### Available Options

#### Global Options (Before `raft-cluster`)
```bash
--kafka-port <PORT>           # Kafka protocol port (default: 9092)
--advertised-addr <ADDR>      # Advertised hostname/IP for clients
--advertised-port <PORT>      # Advertised port (defaults to kafka-port)
--bind-addr <ADDR>            # Bind address (default: 0.0.0.0)
--node-id <ID>                # Node ID for cluster (required)
--data-dir <DIR>              # Data directory (default: ./data)
--metrics-port <PORT>         # Metrics endpoint port (default: 9093)
--file-metadata               # Use file-based metadata (not recommended)
```

#### Raft-Specific Options (After `raft-cluster`)
```bash
--raft-addr <ADDR>            # Raft gRPC listen address (default: 0.0.0.0:5001)
--peers <PEERS>               # Comma-separated peer list: "2@host:port,3@host:port"
--bootstrap                   # Bootstrap flag (first node only)
```

### Correct Usage Examples

#### 3-Node Cluster on Localhost

**Node 1** (Kafka 9092, Raft 9192):
```bash
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  raft-cluster \
    --raft-addr 0.0.0.0:9192 \
    --peers "2@localhost:9292,3@localhost:9392" \
    --bootstrap
```

**Node 2** (Kafka 9093, Raft 9292):
```bash
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  raft-cluster \
    --raft-addr 0.0.0.0:9292 \
    --peers "1@localhost:9192,3@localhost:9392"
```

**Node 3** (Kafka 9094, Raft 9392):
```bash
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  raft-cluster \
    --raft-addr 0.0.0.0:9392 \
    --peers "1@localhost:9192,2@localhost:9292"
```

#### Multi-Machine Cluster

**Node 1** (on server1.example.com):
```bash
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr server1.example.com \
  --node-id 1 \
  raft-cluster \
    --raft-addr 0.0.0.0:9192 \
    --peers "2@server2.example.com:9192,3@server3.example.com:9192" \
    --bootstrap
```

**Node 2** (on server2.example.com):
```bash
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr server2.example.com \
  --node-id 2 \
  raft-cluster \
    --raft-addr 0.0.0.0:9192 \
    --peers "1@server1.example.com:9192,3@server3.example.com:9192"
```

**Node 3** (on server3.example.com):
```bash
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr server3.example.com \
  --node-id 3 \
  raft-cluster \
    --raft-addr 0.0.0.0:9192 \
    --peers "1@server1.example.com:9192,2@server2.example.com:9192"
```

### Implementation Details (From Code Analysis)

**File**: `crates/chronik-server/src/main.rs:480-550`

The CLI parsing code shows:
- `node_id` comes from `cli.node_id` (line 486)
- `kafka_port` comes from `cli.kafka_port` (line 542)
- `advertised_host/port` parsed from `cli.advertised_addr` (lines 520-523)
- `bind_addr` comes from `cli.bind_addr` (line 544)
- `data_dir` comes from `cli.data_dir` (line 540)
- `metrics_port` auto-derived or from `cli.metrics_port` (lines 526-533)

**RaftClusterConfig Struct** (crates/chronik-server/src/raft_cluster.rs:23-36):
```rust
pub struct RaftClusterConfig {
    pub node_id: u64,              // From --node-id
    pub raft_addr: String,         // From --raft-addr
    pub peers: Vec<(u64, String)>, // From --peers
    pub bootstrap: bool,           // From --bootstrap
    pub data_dir: PathBuf,         // From --data-dir
    pub file_metadata: bool,       // From --file-metadata
    pub kafka_port: u16,           // From --kafka-port
    pub metrics_port: u16,         // Auto-derived or --metrics-port
    pub bind_addr: String,         // From --bind-addr
    pub advertised_host: String,   // From --advertised-addr
    pub advertised_port: u16,      // From --advertised-port or kafka-port
    pub gossip_config: Option<GossipConfig>,
}
```

## Testing Checklist for Next Session

### Phase 1: Cluster Startup ✅ Ready
- [ ] Start 3-node cluster using correct CLI syntax
- [ ] Verify all nodes running (check with `ps aux | grep chronik-server`)
- [ ] Check logs for "Raft clustering enabled" messages
- [ ] Verify __meta partition created on all nodes
- [ ] Verify leader election completes (check for "leader=X" in logs)

### Phase 2: Topic Creation and Callback ✅ Ready
- [ ] Use Java kafka-console-producer to create topic
- [ ] Check logs for "Topic creation callback fired" messages
- [ ] Verify Raft replicas created on ALL nodes (not just one)
- [ ] Check for "Successfully created Raft replica for X-Y" logs
- [ ] Verify no "replica not found" errors

### Phase 3: End-to-End Validation ✅ Ready
- [ ] Produce messages via Java producer to cluster
- [ ] Consume messages via Java consumer from cluster
- [ ] Verify data replicated across nodes
- [ ] Test failover by killing leader node
- [ ] Verify new leader elected
- [ ] Verify produce/consume still works

## Log Patterns to Watch For

### Success Indicators
```
INFO chronik_server: Raft clustering enabled on node N
INFO chronik_raft::replica: ready() HAS_READY for __meta-0: raft_state=Leader, term=X
INFO chronik_server: Topic creation callback fired for 'topic-name'
INFO chronik_server: Successfully created Raft replica for topic-X with Y peers
```

### Potential Issues
```
ERROR chronik_raft: No address for peer 0  # Should be FIXED by our changes
WARN: Replica not found                    # Should only appear during first 10s (grace period)
ERROR: Failed to forward proposal           # Check if leader_id=0 check is working
```

## Files Modified in This Session

1. `crates/chronik-raft/src/raft_meta_log.rs`
   - Lines 689-713: leader_id=0 check
   - Lines 346-353: Callback matching fix

2. `crates/chronik-raft/src/lib.rs`
   - Line 94: Export TopicCreatedCallback

3. `crates/chronik-server/src/integrated_server.rs`
   - Lines 216-284: Callback implementation
   - Lines 174-198: Multi-node standalone block

4. `CHANGELOG.md`
   - Added 5 fix entries under `[Unreleased]`

5. `docs/ROADMAP_v2.x.md`
   - Updated Task #4 with investigation results

## Next Steps

1. **Test raft-cluster mode** using correct CLI syntax (global options before subcommand)
2. **Verify our 4 fixes work** in multi-node environment
3. **Validate end-to-end** produce → replicate → consume flow
4. **Document results** and update CHANGELOG if issues found

## Why This Matters

Our callback fixes enable **automatic Raft replica creation** when topics are created. This is critical for:

- ✅ Multi-node data replication
- ✅ Fault tolerance (quorum-based commits)
- ✅ Automatic leader election per partition
- ✅ Zero message loss with replication

Without these fixes, topics would be created in metadata but have **no Raft replicas**, causing all produce requests to fail with `NOT_LEADER_OR_FOLLOWER` errors.

## Conclusion

**Status**: ✅ **READY TO TEST**

All code is in place, all bugs fixed, and raft-cluster mode has all necessary features. Just needs testing with correct CLI syntax to validate everything works end-to-end.
