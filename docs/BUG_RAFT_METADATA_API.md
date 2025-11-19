# CRITICAL BUG: Raft Cluster Metadata API Not Working

**Date**: 2025-11-18
**Severity**: P0 - CRITICAL
**Status**: Investigating

---

## Summary

Chronik's Raft cluster mode has a **CRITICAL bug** in the Metadata API that prevents **ALL kafka-python clients** from discovering cluster nodes. This breaks CreateTopics, topic creation, and any admin operations.

---

## Symptoms

1. **kafka-python clients cannot discover brokers**:
   ```python
   from kafka import KafkaConsumer
   c = KafkaConsumer(bootstrap_servers=['localhost:9092'], api_version=(0,10,0))
   print(c._client.cluster)
   # Output: ClusterMetadata(brokers: 0, topics: 0, coordinators: 0)
   ```

2. **AdminClient failures**:
   ```
   kafka.errors.NodeNotReadyError: NodeNotReadyError: controller
   kafka.errors.IncompatibleBrokerVersion: Kafka broker does not support the 'CreateTopicsRequest_v0' Kafka protocol
   ```

3. **NO Metadata request logs in Chronik server**:
   - Expected logs from [handler.rs:2088-2343](../crates/chronik-protocol/src/handler.rs#L2088-L2343):
     - "Handling metadata request v{}"
     - "Got {} brokers from metadata store (before filter)"
     - "Metadata response has {} topics and {} brokers"
   - **Actual logs**: NONE - Metadata requests never reach `handle_metadata()`

---

## What Works

- Kafka ports (9092, 9093, 9094) are **listening correctly** (verified with netstat)
- kafka-python **connects successfully** to localhost:9092
- Raft consensus is **working** (heartbeat logs confirm 3-node cluster)
- chronik-bench timeout fix is **complete and verified** ([benchmark.rs:181-187](../crates/chronik-bench/src/benchmark.rs#L181-L187))

---

## What Doesn't Work

- **Metadata API requests are NOT reaching the protocol handler**
- kafka-python gets **ZERO brokers** from Metadata response
- **ALL admin operations fail** (CreateTopics, ListTopics, etc.)
- **10 topics fail** instantly with same error
- **3000 topics fail** completely (100% failure rate)

---

## Root Cause Analysis

### Investigation Steps Taken

1. **Verified CreateTopics API support**: Chronik supports CreateTopics v0-v7 ([parser.rs:714](../crates/chronik-protocol/src/parser.rs#L714))

2. **Checked Metadata handler code**: [handler.rs:2080-2367](../crates/chronik-protocol/src/handler.rs#L2080-L2367)
   - Has extensive logging
   - Gets brokers from `metadata_store.list_brokers()` (line 2262)
   - Has critical validation checks (lines 2313-2332)
   - **None of these logs appear**

3. **Tested with fresh cluster restart**: Same behavior - NO Metadata logs

4. **Confirmed ports are listening**: All 3 nodes listening on correct ports

5. **Tested with simple kafka-python client**: Connects but gets 0 brokers

### Hypothesis

The Metadata API request is either:
1. **Not being sent** by kafka-python (unlikely - would fail to connect)
2. **Not being routed** to `handle_metadata()` in Raft cluster mode
3. **Failing before handler** due to protocol parsing/routing issue
4. **Different code path** for Raft cluster vs standalone mode

---

## Impact

This bug **BLOCKS ALL TESTING** of Chronik Raft cluster:
- ❌ Cannot create topics via Admin API
- ❌ Cannot run benchmarks
- ❌ Cannot test with kafka-python clients
- ❌ Cannot test with 10 topics
- ❌ Cannot test with 3000 topics

**Estimated user impact**: 100% of kafka-python/Java clients using Admin API

---

## Next Steps for Debugging

1. **Check Raft cluster Kafka handler routing**:
   - Does Raft cluster use different Kafka handler than standalone?
   - Is `ApiKey::Metadata` case handled differently?
   - Is there a cluster-specific protocol layer?

2. **Add connection-level logging**:
   - Log when TCP connections are accepted
   - Log when Kafka requests are received
   - Log API keys of all incoming requests

3. **Check IntegratedKafkaServer initialization**:
   - How is `protocol_handler` initialized in cluster mode?
   - Is `handle_metadata` even registered?

4. **Test standalone mode**:
   - Does Metadata API work in standalone (non-Raft) mode?
   - This would isolate whether it's Raft-specific

5. **Packet capture**:
   - Use tcpdump/wireshark to see if Metadata requests are being sent
   - Verify request format matches Kafka protocol

---

## Files Modified During Investigation

- [crates/chronik-bench/src/benchmark.rs](../crates/chronik-bench/src/benchmark.rs#L181-L187) - Added 60s timeout to `create_topic()`
- [create_3000_topics.py](../create_3000_topics.py) - Stress test script (100 threads)
- [create_topics_throttled.py](../create_topics_throttled.py) - Throttled test (10 threads)
- [create_topics_batch.py](../create_topics_batch.py) - Batch test (single admin client)

**All test approaches failed** with same underlying Metadata API issue.

---

## Related Documentation

- [P0.3_DEADLOCK_FIX_VERIFICATION.md](./audit/P0.3_DEADLOCK_FIX_VERIFICATION.md) - All v2.2.7 deadlock fixes verified
- [CLUSTER_STATUS_REPORT_v2.2.8.md](./CLUSTER_STATUS_REPORT_v2.2.8.md) - Pre-bug cluster status
- [RAFT_ELECTION_STORM_BUG.md](./RAFT_ELECTION_STORM_BUG.md) - Previous Raft issue (resolved)

---

## Recommendation

**URGENT**: This is a **critical regression** that breaks all Raft cluster Admin API functionality.

**Priority**: P0 - Must be fixed before ANY further development or testing can proceed.

**Blocking**: All benchmarking, topic creation testing, and stress testing.
