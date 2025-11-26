# Cluster rdkafka Hang - Critical Diagnostic Analysis

## Executive Summary

**Status:** BLOCKING ISSUE - Cluster benchmarking completely non-functional
**Impact:** Cannot measure cluster performance (user expects ~90k msg/s based on standalone's 300k msg/s)
**Root Cause:** rdkafka client hangs when given multiple bootstrap servers

---

## Test Results Summary

| Test Configuration | Result | Throughput | Notes |
|--------------------|--------|------------|-------|
| **Single node bootstrap** (`-b localhost:9092`) | ✅ WORKS | 20 msg/s | Expected (50ms fsync interval) |
| **Cluster bootstrap** (`-b localhost:9092,localhost:9093,localhost:9094`) | ❌ HANGS | 0 msg/s | Hangs in warmup, zero server activity |
| **Concurrency** | c=1: ✅ | c=2+: ❌ | Both hang with cluster bootstrap |

---

## Detailed Findings

### What Works ✅

1. **Single-node connection:**
```bash
./target/release/chronik-bench -b localhost:9092 -c 2 -s 256 -d 5s --acks 1 --topic single-node-test --create-topic
```
**Result:**
- ✅ Warmup completed (52 seconds)
- ✅ Benchmark ran successfully
- ✅ 202 messages processed
- ✅ 20 msg/s (matches expected 50ms p99 latency)
- ✅ 0% failures

### What Fails ❌

2. **Cluster connection (2 connections):**
```bash
./target/release/chronik-bench -b localhost:9092,localhost:9093,localhost:9094 -c 2 -s 256 -d 5s --acks 1 --topic concurrency-test-2 --create-topic
```
**Result:**
- ❌ Topic created successfully at 23:35:48.512050Z
- ❌ HUNG in "Running warmup for 5s..."
- ❌ No server activity - zero requests received
- ❌ Zero TCP connections established (netstat shows 0 connections)

3. **Cluster connection (128 connections):**
```bash
./target/release/chronik-bench -b localhost:9092,localhost:9093,localhost:9094 -c 128 -s 256 -d 10s --acks 1 --topic perf-test-now --create-topic
```
**Result:**
- ❌ Topic created successfully at 23:28:06.262195Z
- ❌ HUNG in "Running warmup for 5s..."
- ❌ No server activity for "perf-test-now" topic
- ❌ Zero TCP connections established

---

## Infrastructure Verification

### All cluster ports are listening ✅
```bash
$ netstat -tlnp | grep -E ":(9092|9093|9094)"
tcp  0  0  0.0.0.0:9092  0.0.0.0:*  LISTEN  1760272/chronik-ser
tcp  0  0  0.0.0.0:9093  0.0.0.0:*  LISTEN  1760312/chronik-ser
tcp  0  0  0.0.0.0:9094  0.0.0.0:*  LISTEN  1760354/chronik-ser
```

### All ports are TCP-connectable ✅
```bash
$ nc -zv localhost 9092 9093 9094
Connection to localhost (127.0.0.1) 9092 port [tcp/*] succeeded!
Connection to localhost (127.0.0.1) 9093 port [tcp/*] succeeded!
Connection to localhost (127.0.0.1) 9094 port [tcp/*] succeeded!
```

### Cluster configuration is correct ✅
**node1.toml:**
```toml
[advertise]
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 1
kafka = "localhost:9092"

[[peers]]
id = 2
kafka = "localhost:9093"

[[peers]]
id = 3
kafka = "localhost:9094"
```

---

## Key Observations

1. **Network is NOT the problem:**
   - ✅ All ports listening
   - ✅ All ports connectable
   - ✅ Single-node bootstrap works perfectly

2. **rdkafka/librdkafka specific:**
   - ❌ Hangs when given comma-separated bootstrap servers
   - ❌ No TCP connections established
   - ❌ Appears to hang during metadata discovery phase

3. **Timing evidence:**
   - Topic creation succeeds in ~60-90ms (fast!)
   - Hang occurs AFTER topic creation
   - Server logs show topic creation but NO subsequent requests

4. **Server is idle:**
   - Zero requests received for cluster-bootstrapped topics
   - No errors in server logs
   - Raft heartbeats continue normally

---

## Hypothesis: Metadata Response Issue

**Most Likely Cause:** When rdkafka requests metadata with multiple bootstrap servers, it may be:

1. **Receiving invalid broker list in MetadataResponse**
   - Cluster might be advertising wrong addresses
   - Broker IDs might conflict
   - Listener addresses might be malformed

2. **Getting confused by cluster topology**
   - Multiple brokers with overlapping partition assignments
   - ISR (In-Sync Replicas) configuration issues
   - Controller ID mismatch

3. **Waiting for leader election that never completes**
   - Raft leader elected but not advertised correctly
   - Partition leaders not set properly
   - Metadata propagation delay

---

## Next Steps (Priority Order)

### 1. **Capture actual Metadata response**
Use a raw Kafka client or tcpdump to see what metadata the cluster is returning:
```bash
tcpdump -i lo -w /tmp/cluster-metadata.pcap port 9092 or port 9093 or port 9094
# Then connect with rdkafka and capture the Metadata response
```

### 2. **Test with native Java Kafka client**
Determine if this is rdkafka-specific or affects all Kafka clients:
```bash
kafka-console-producer --bootstrap-server localhost:9092,localhost:9093,localhost:9094 --topic java-test
```

### 3. **Enable rdkafka debug logging**
See what rdkafka is actually doing:
```rust
// In chronik-bench, add debug output configuration
config.set("debug", "broker,topic,msg,protocol,security,fetch,feature,queue");
```

### 4. **Check Metadata handler implementation**
Review how `handle_metadata` in the cluster returns broker information.

---

## Impact Assessment

**User Expectations:**
- Standalone: 300,000 msg/s ✅
- Cluster: ~90,000 msg/s (expected)
- **Actual Cluster: 0 msg/s** ❌ (complete hang)

**Severity:** CRITICAL - Blocks all cluster performance testing

---

## Files to Investigate

1. **Metadata Handler:**
   - [crates/chronik-server/src/kafka_handler.rs:123](crates/chronik-server/src/kafka_handler.rs#L123) - Metadata routing
   - [crates/chronik-server/src/handler.rs](crates/chronik-server/src/handler.rs) - Metadata response construction

2. **Cluster Configuration:**
   - [tests/cluster/node1.toml](tests/cluster/node1.toml) - Node 1 config
   - [tests/cluster/node2.toml](tests/cluster/node2.toml) - Node 2 config
   - [tests/cluster/node3.toml](tests/cluster/node3.toml) - Node 3 config

3. **Benchmark Client:**
   - [crates/chronik-bench/src/benchmark.rs:229-256](crates/chronik-bench/src/benchmark.rs#L229-L256) - Warmup logic where hang occurs

---

## Workaround (Temporary)

Until fixed, cluster benchmarking must use single-node bootstrap:
```bash
# Instead of:
# ./target/release/chronik-bench -b localhost:9092,localhost:9093,localhost:9094 ...

# Use:
./target/release/chronik-bench -b localhost:9092 ...
```

**Note:** This defeats the purpose of cluster testing since clients won't discover other nodes.

---

**Generated:** 2025-11-25 23:40 UTC
**Cluster Version:** v2.2.8+
**rdkafka Version:** librdkafka 1.x (via rust rdkafka crate)
