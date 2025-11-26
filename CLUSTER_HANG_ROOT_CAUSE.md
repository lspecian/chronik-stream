# Cluster rdkafka Hang - Root Cause Analysis

## Date: 2025-11-26

## Critical New Finding

**The issue is NOT about multiple bootstrap servers** - it's about the CLUSTER itself!

### Test Results

| Configuration | Result |
|---------------|--------|
| Standalone + single bootstrap (`localhost:9092`) | ✅ WORKS (333k msg/s) |
| Cluster + single bootstrap (`localhost:9092`) | ❌ HANGS |
| Cluster + multiple bootstrap (`localhost:9092,localhost:9093,localhost:9094`) | ❌ HANGS |

### Smoking Gun Evidence

1. **Zero packets captured**: After topic creation, rdkafka sends **ZERO packets** to the cluster for producing
2. **Topic creation succeeds**: The topic is created successfully with 3 partitions
3. **No connection attempts**: Diagnostic logging shows zero connection attempts for producing
4. **Client-side hang**: The hang is happening entirely within rdkafka, not on the server

### What Happens

```
Timeline of events:

00:20:47.157 - Benchmark starts, creates topic
00:20:47.225 - Topic "cluster-hang-test" created successfully (3 partitions)
00:20:47.225 - "Running warmup for 5s..." message printed
00:20:47.225 - 00:21:47.000 - **COMPLETE SILENCE** - zero packets, zero connections
[timeout after 60s]
```

### v2.2.13 Fix Attempt - UNSUCCESSFUL

**Changes tested (2025-11-26):**
1. ✅ **integrated_server.rs:289** - Added `metadata_store` to `WalReplicationManager`
   - Purpose: Enable leader partition determination for replication
   - Result: Did NOT resolve hang issue
   
2. ✅ **produce_handler.rs:980** - Increased `pipelined_pool` capacity 1000 → 10000
   - Purpose: Prevent channel blocking under high concurrency
   - Result: Did NOT resolve hang issue

3. ✅ **Converted diagnostic logging** from `info!()` to `debug!()`
   - Purpose: Eliminate 5.3x performance regression (333k → 62k msg/s)
   - Result: Performance restored to baseline, but hang persists

**Conclusion**: The fixes were intended for different issues. The hang is caused by something else in the cluster's Metadata response.

### Hypothesis: Metadata Response Issue

The cluster's Metadata response must contain something that causes rdkafka to hang:

**Possible causes:**
1. **Broker IDs mismatch**: Cluster advertises broker IDs that confuse rdkafka
2. **Invalid advertised addresses**: Cluster returns addresses that rdkafka can't resolve/connect to
3. **Partition leadership info**: Something wrong with partition→broker mapping
4. **ISR configuration**: In-Sync Replicas list causing issues

### Next Investigation Steps

1. ✅ Confirmed hang happens with both single and multiple bootstrap
2. ✅ Confirmed zero network activity after topic creation
3. ✅ Tested v2.2.13 fixes - did NOT resolve hang
4. 🔄 **NEED**: Capture actual Metadata response from cluster (tcpdump/tshark)
5. 🔄 **NEED**: Compare cluster Metadata response vs standalone
6. 🔄 **NEED**: Check broker IDs, advertised addresses in Metadata response

### Cluster Configuration

**Node 1** (tests/cluster/node1.toml):
```toml
node_id = 1

[node.addresses]
kafka = "0.0.0.0:9092"
wal = "0.0.0.0:9291"
raft = "0.0.0.0:5001"

[node.advertise]
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 1
kafka = "localhost:9092"
# ... (nodes 2 and 3 similar)
```

### Suggested Fix Approaches

1. **Capture Metadata response with tcpdump**: See what the cluster is actually returning to rdkafka
2. **Verify broker IDs**: Ensure Metadata response returns correct broker_id for each node
3. **Check advertised address**: Ensure cluster nodes advertise reachable addresses
4. **Simplify ISR**: Test with single-node ISR to isolate replication logic
5. **Enable rdkafka debug**: Add `debug=all` to rdkafka config to see client-side behavior

---

**Status**: Root cause NOT yet identified, but narrowed down to cluster Metadata response
**Blocker**: Cannot benchmark cluster performance until this is resolved
**Impact**: CRITICAL - Cluster is completely unusable with rdkafka clients
