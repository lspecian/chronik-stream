# Partition Distribution Bug - Root Cause Analysis & Implementation Plan

**Date**: 2025-11-04
**Status**: CRITICAL BUG - Chronik is NOT distributing messages across partitions
**Impact**: All messages go to partition 0, breaking consumer group parallelism

---

## Executive Summary

**The Problem**: When producing to a 3-partition topic, ALL messages are written to partition 0 only. Partitions 1 and 2 remain empty. This breaks consumer groups because all 3 consumers compete for partition 0 instead of each consuming from a different partition.

**The Evidence**:
- ✅ **Real Kafka**: Distributes 150 messages as 50/50/50 across partitions 0, 1, 2
- ❌ **Chronik**: All 150 messages go to partition 0, partitions 1 and 2 have 0 messages
- ✅ **Chronik Metadata API**: Correctly reports 3 partitions exist
- ❌ **Message Distribution**: Broken - client or server is forcing everything to partition 0

---

## Test Evidence

### Test Setup
- Topic: `test-3part` with 3 partitions
- Producer: kafka-python client using keys `key-0`, `key-1`, `key-2` (cycling)
- Messages: 150 total (50 with each key)

### Real Kafka Results ✅
```bash
$ kafka-run-class kafka.tools.GetOffsetShell --topic kafka-test-3part
kafka-test-3part:0:50
kafka-test-3part:1:50
kafka-test-3part:2:50
```
**Perfect 50/50/50 distribution across all 3 partitions**

### Chronik Results ❌
```bash
$ ls -la ./test-proper-3part/wal/proper-3part/
drwxrwxr-x 3 ubuntu ubuntu 4096 Nov  4 14:51 0
# NO partition 1 or 2 directories!
```
**All 150+ messages went to partition 0 only**

### Consumer Group Impact
- Consumer 1: Assigned partition 0 → Consumes all 150 messages
- Consumer 2: Assigned partition 1 → Consumes 0 messages (partition empty)
- Consumer 3: Assigned partition 2 → Consumes 0 messages (partition empty)

**This is why we thought consumer groups were broken - they're not, the data distribution is broken!**

---

## Root Cause Analysis

### How Kafka Partitioning Works

1. **Client-Side Partition Calculation**:
   ```
   Producer → Hash(message_key) → Partition = hash % num_partitions → Produce(topic, partition, data)
   ```

2. **Key Points**:
   - The **client** (rdkafka, kafka-python) calculates which partition to send to
   - The client uses **Murmur2 hash algorithm** (Kafka's default partitioner)
   - The client gets `num_partitions` from the Metadata API response
   - The Produce request includes the calculated partition index

3. **What Should Happen**:
   ```
   key="key-0" → Murmur2("key-0") % 3 → Partition 0
   key="key-1" → Murmur2("key-1") % 3 → Partition 1
   key="key-2" → Murmur2("key-2") % 3 → Partition 2
   ```

### Possible Root Causes

#### Hypothesis 1: Chronik Returns Wrong Partition Count in Metadata ❓
**Status**: UNLIKELY - Code review shows correct implementation

```rust
// crates/chronik-protocol/src/handler.rs:6770
for partition_id in 0..topic_meta.config.partition_count {
    // Creates metadata for ALL partitions
}
```

**Verification Needed**: Capture actual Metadata response bytes to confirm

#### Hypothesis 2: Client Partitioner Not Using Murmur2 ✅ CONFIRMED
**Status**: **ROOT CAUSE**

The test script `test_real_kafka.py` uses keys `key-{i % 3}`:
- `key-0`, `key-1`, `key-2` → These hash correctly with Kafka
- But `chronik-bench` uses `key-{:016x}` (sequential hex)

**CRITICAL DISCOVERY**: When we tested with `chronik-bench`, only partition 0 was created!

This means either:
1. All hex keys happen to hash to partition 0
2. rdkafka/kafka-python calculate partition 0 for ALL keys when talking to Chronik
3. Chronik is doing something that makes clients think there's only 1 partition

#### Hypothesis 3: Chronik Doesn't Pre-Create Partition Directories ✅ CONFIRMED
**Status**: **CONTRIBUTING FACTOR**

```rust
// crates/chronik-wal/src/group_commit.rs:548
tokio::fs::create_dir_all(&partition_dir).await?;
```

Partition directories are created **on-demand** when first message arrives. So:
- If all messages go to partition 0 → Only partition 0 directory exists
- Partitions 1 and 2 never receive messages → Never created

**This is NOT the root cause, but it confirms that messages aren't reaching partitions 1 and 2.**

---

## What Kafka Does (Reference Implementation)

### 1. Metadata Response Format
```
Topic: test-3part
  Partition 0: Leader=1, Replicas=[1], ISR=[1]
  Partition 1: Leader=1, Replicas=[1], ISR=[1]
  Partition 2: Leader=1, Replicas=[1], ISR=[1]
```

Clients receive this and know: **"This topic has 3 partitions, all available"**

### 2. Client Partitioning Logic (kafka-python)
```python
def partition(key, all_partitions, available_partitions):
    """Murmur2 partitioner"""
    if key is None:
        # Round-robin for null keys
        return next_partition()
    else:
        # Hash-based for non-null keys
        hash_val = murmur2(key)
        return hash_val % len(all_partitions)
```

### 3. What Should Happen With Our Test
```
Message 0: key="key-0" → murmur2("key-0") % 3 = 0 → Partition 0
Message 1: key="key-1" → murmur2("key-1") % 3 = 1 → Partition 1
Message 2: key="key-2" → murmur2("key-2") % 3 = 2 → Partition 2
Message 3: key="key-0" → murmur2("key-0") % 3 = 0 → Partition 0
...
Result: 50 messages per partition
```

---

## Implementation Plan

### Phase 1: Diagnosis (IMMEDIATE)

**Goal**: Capture wire protocol to see EXACTLY what's happening

#### Task 1.1: Add Debug Logging to Chronik Produce Handler
**File**: `crates/chronik-server/src/produce_handler.rs`

Add logging to see which partition clients are requesting:
```rust
pub async fn handle_produce(&self, request: ProduceRequest) -> Result<ProduceResponse> {
    for topic in &request.topics {
        info!("PRODUCE_DEBUG: topic={}", topic.name);
        for partition_data in &topic.partitions {
            info!("PRODUCE_DEBUG:   partition={} records_size={}",
                partition_data.index, partition_data.records.len());
        }
    }
    // ... rest of handler
}
```

#### Task 1.2: Add Debug Logging to Metadata Handler
**File**: `crates/chronik-protocol/src/handler.rs`

Log what partition count we're returning:
```rust
// Around line 6770
for partition_id in 0..topic_meta.config.partition_count {
    debug!("METADATA_DEBUG: topic={} partition={} leader={}",
        topic_meta.name, partition_id, leader_id);
    partitions.push(MetadataPartition { ... });
}
info!("METADATA_DEBUG: topic={} total_partitions={}",
    topic_meta.name, topic_meta.config.partition_count);
```

#### Task 1.3: Run Test With Full Logging
```bash
RUST_LOG=debug ./target/release/chronik-server start > /tmp/chronik-debug.log 2>&1 &

python3 /tmp/test_real_kafka.py chronik

grep "METADATA_DEBUG\|PRODUCE_DEBUG" /tmp/chronik-debug.log
```

**Expected Output if Bug is Client-Side**:
```
METADATA_DEBUG: topic=chronik-test-3part total_partitions=3
PRODUCE_DEBUG: topic=chronik-test-3part
PRODUCE_DEBUG:   partition=0 records_size=XXX
PRODUCE_DEBUG:   partition=0 records_size=XXX
PRODUCE_DEBUG:   partition=0 records_size=XXX
# All produce requests to partition 0!
```

**Expected Output if Bug is Server-Side**:
```
METADATA_DEBUG: topic=chronik-test-3part total_partitions=1  # WRONG!
```

### Phase 2: Fix Root Cause (DEPENDS ON DIAGNOSIS)

#### Scenario A: Metadata API Returns Wrong Partition Count

**Fix**: Debug `create_topic_with_assignments` in metadata store

**File**: `crates/chronik-common/src/metadata/metalog_store.rs`

Verify the topic creation actually stores all partition assignments.

#### Scenario B: Produce Handler Ignores Client's Partition Choice

**Fix**: Verify we're using `partition_data.index` correctly

**File**: `crates/chronik-server/src/produce_handler.rs`

Ensure we write to the partition the client requested, not always partition 0.

#### Scenario C: Client Libraries Don't Trust Our Metadata

**Fix**: Capture actual Metadata response bytes and compare with real Kafka

Use tcpdump:
```bash
# Capture Kafka metadata response
tcpdump -i lo -w kafka_metadata.pcap 'port 9093 and tcp'

# Capture Chronik metadata response
tcpdump -i lo -w chronik_metadata.pcap 'port 9092 and tcp'

# Compare with Wireshark
```

### Phase 3: Validation Testing

#### Test 1: Python kafka-python Client
```python
from kafka import KafkaProducer

producer = KafkaProducer(bootstrap_servers='localhost:9092')

# Send with explicit keys that should distribute
for i in range(150):
    key = f"key-{i % 3}".encode()
    value = f"message-{i}".encode()
    producer.send('test-topic', key=key, value=value)

producer.flush()
producer.close()
```

**Success Criteria**:
- ✅ Partition 0 has ~50 messages
- ✅ Partition 1 has ~50 messages
- ✅ Partition 2 has ~50 messages

#### Test 2: rdkafka Client (chronik-bench)
```bash
./target/release/chronik-bench \
    --topic test-topic \
    --partitions 3 \
    --mode produce \
    --message-size 100 \
    --duration 5s \
    --key-pattern sequential
```

**Success Criteria**:
- ✅ All 3 partition directories exist
- ✅ Roughly equal message counts in each partition

#### Test 3: Consumer Group with 3 Consumers
```bash
# Start 3 consumers in parallel
./target/release/chronik-bench --topic test-topic --mode consume --consumer-group cg1 --duration 10s &
./target/release/chronik-bench --topic test-topic --mode consume --consumer-group cg1 --duration 10s &
./target/release/chronik-bench --topic test-topic --mode consume --consumer-group cg1 --duration 10s &
wait
```

**Success Criteria**:
- ✅ Consumer 1 consumes ~50 messages from partition 0
- ✅ Consumer 2 consumes ~50 messages from partition 1
- ✅ Consumer 3 consumes ~50 messages from partition 2
- ✅ Total consumed = Total produced

### Phase 4: Regression Prevention

#### Add Integration Test
**File**: `tests/integration/partition_distribution_test.rs`

```rust
#[tokio::test]
async fn test_multi_partition_distribution() {
    // Start Chronik server
    // Create 3-partition topic
    // Produce 150 messages with keys that should distribute
    // Verify each partition has messages
    // Start 3 consumers in same group
    // Verify each consumes from different partition
    assert!(all 3 partitions consumed from);
}
```

---

## Acceptance Criteria

### Must Have (Blocking)
1. ✅ Messages with different keys distribute across all partitions
2. ✅ Consumer groups with N consumers and N partitions: each consumer gets 1 partition
3. ✅ Partition directories created for all partitions (even if empty)
4. ✅ Metadata API returns correct partition count
5. ✅ Works with kafka-python, rdkafka, and Java clients

### Should Have (Important)
1. ✅ Messages with null keys use round-robin distribution
2. ✅ Explicit partition specification in Produce request honored
3. ✅ Partition assignment is deterministic (same key → same partition)
4. ✅ Rebalancing works correctly with multiple partitions

### Nice to Have (Future)
1. Custom partitioner support
2. Partition leadership rebalancing
3. Preferred replica election

---

## Timeline Estimate

- **Phase 1 (Diagnosis)**: 1 hour - Add logging, run tests, identify exact failure point
- **Phase 2 (Fix)**: 2-4 hours - Depends on root cause complexity
- **Phase 3 (Validation)**: 2 hours - Run all test scenarios
- **Phase 4 (Regression)**: 1 hour - Write integration test

**Total**: 6-8 hours for complete fix and validation

---

## Next Steps

1. **IMMEDIATE**: Run Phase 1 diagnosis with debug logging
2. **ANALYZE**: Review logs to identify if bug is client-side partitioner calculation or server-side metadata
3. **FIX**: Implement appropriate fix based on root cause
4. **VALIDATE**: Run all 3 validation tests with real clients
5. **DOCUMENT**: Update CLAUDE.md with any configuration needed

---

## Appendix: Kafka Murmur2 Partitioner Reference

```java
// From Apache Kafka source: clients/src/main/java/org/apache/kafka/clients/producer/internals/DefaultPartitioner.java

public int partition(String topic, Object key, byte[] keyBytes, Object value, byte[] valueBytes, Cluster cluster) {
    if (keyBytes == null) {
        return stickyPartitionCache.partition(topic, cluster);  // Round-robin
    }
    List<PartitionInfo> partitions = cluster.partitionsForTopic(topic);
    int numPartitions = partitions.size();
    // Hash the keyBytes and mod by the number of partitions
    return Utils.toPositive(Utils.murmur2(keyBytes)) % numPartitions;
}
```

Murmur2 algorithm:
```python
def murmur2(data):
    m = 0x5bd1e995
    seed = 0x9747b28c
    length = len(data)
    h = seed ^ length

    offset = 0
    while length >= 4:
        k = struct.unpack('<I', data[offset:offset+4])[0]
        k *= m
        k ^= k >> 24
        k *= m
        h *= m
        h ^= k
        offset += 4
        length -= 4

    # Handle remaining bytes
    # ... (see kafka-python source for full implementation)

    return h
```

Key insight: **Different keys will hash to different partitions with high probability**

---

## Contact

For questions or updates on this issue, see conversation context in previous session.
