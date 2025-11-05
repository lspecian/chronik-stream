# Murmur2 Partitioning - Do We Need to Implement It?

**Date**: 2025-11-04
**Status**: ANALYSIS COMPLETE
**Answer**: **NO - Not needed for server-side. Already have it for coordinator assignment.**

---

## Executive Summary

**Question**: Does Chronik need to implement Murmur2 partitioning algorithm for message distribution?

**Answer**: **NO** - Murmur2 partitioning is **CLIENT-SIDE ONLY** in the Kafka protocol.

**Why**: The Kafka Produce request includes the partition index explicitly. The client calculates which partition to use, and the server just accepts it.

**What Chronik Already Has**: Murmur2 implementation for coordinator assignment (consumer groups → brokers).

---

## How Kafka Partitioning Actually Works

### The Protocol Flow

```
┌─────────────────────────────────────────────────────────────┐
│                    KAFKA PARTITIONING FLOW                  │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  1. CLIENT SIDE (Producer)                                  │
│     ┌────────────────────────────────────────┐             │
│     │  Producer.send(topic, key, value)      │             │
│     │                                         │             │
│     │  // Client calculates partition:       │             │
│     │  if (key == null):                     │             │
│     │      partition = round_robin()         │             │
│     │  else:                                  │             │
│     │      hash = Murmur2(key)               │             │
│     │      partition = hash % num_partitions │             │
│     │                                         │             │
│     │  ProduceRequest {                      │             │
│     │      topic: "my-topic",                │             │
│     │      partition: 2,  ← EXPLICIT INDEX  │             │
│     │      records: [...]                    │             │
│     │  }                                     │             │
│     └────────────────────────────────────────┘             │
│                      │                                      │
│                      │ Send over wire                       │
│                      ▼                                      │
│  2. SERVER SIDE (Chronik)                                   │
│     ┌────────────────────────────────────────┐             │
│     │  handle_produce(request):              │             │
│     │                                         │             │
│     │  partition = request.partition  // ← Just read it!  │
│     │  write_to_partition(partition, records)│             │
│     │                                         │             │
│     │  // NO HASHING ON SERVER!              │             │
│     │  // NO MURMUR2 CALCULATION!           │             │
│     └────────────────────────────────────────┘             │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Key Insight

**The Kafka Produce request wire format includes `partition_index` as a required field!**

From `crates/chronik-protocol/src/produce_types.rs`:
```rust
pub struct ProduceRequestPartition {
    pub index: i32,  // ← Client tells us which partition!
    pub records: Vec<u8>,
}
```

The client has ALREADY calculated which partition to use. Chronik just writes to that partition.

---

## What Chronik Currently Has

### 1. Murmur2 for Coordinator Assignment ✅

**File**: `crates/chronik-server/src/coordinator_manager.rs`

**Purpose**: Determine which broker handles which consumer group

**Code**:
```rust
use murmur2::murmur2;

pub fn get_coordinator_broker(&self, group_id: &str) -> i32 {
    let hash = murmur2(group_id.as_bytes(), 0);
    let broker_index = (hash as usize) % self.brokers.len();
    self.brokers[broker_index].broker_id
}
```

**This is CORRECT and NEEDED** - it's for a different purpose (coordinator assignment, not message partitioning).

### 2. Partition Index from Produce Request ✅

**File**: `crates/chronik-server/src/produce_handler.rs`

**Code** (simplified):
```rust
pub async fn handle_produce(&self, request: ProduceRequest) -> Result<...> {
    for topic in request.topics {
        for partition_data in topic.partitions {
            let partition_index = partition_data.index;  // ← From client
            self.write_to_partition(&topic.name, partition_index, partition_data.records).await?;
        }
    }
}
```

**This is CORRECT** - we use the partition index the client calculated.

---

## Do We Need Server-Side Partitioning?

### Scenario 1: Standard Kafka Protocol ❌ NO

**Standard Produce Flow**:
```
Client → Calculate partition → Send ProduceRequest(partition=X) → Server writes to partition X
```

**Chronik's current implementation**: ✅ CORRECT - Just uses the partition index from the request.

### Scenario 2: Proprietary API with Server-Side Partitioning ⚠️ MAYBE

**Hypothetical custom API**:
```rust
// IF we added a custom HTTP/REST API:
POST /produce
{
  "topic": "my-topic",
  "key": "user-123",
  "value": "data",
  "partition": null  // ← Client doesn't specify partition
}

// Server would need to calculate:
let partition = murmur2(key) % num_partitions;
```

**Chronik's current implementation**: ❌ Doesn't support this (but also doesn't need to for Kafka compatibility).

### Scenario 3: Kafka Streams / ksqlDB Compatibility ✅ YES (Already Works)

**How Kafka Streams works**:
1. Kafka Streams client calculates partition using Murmur2
2. Sends standard Produce request with partition index
3. Server writes to that partition

**Chronik's current implementation**: ✅ WORKS - Because clients do the partitioning.

---

## Implementation Decision

### What We Need to Fix ✅ REQUIRED

1. **Fix topic creation to honor `num_partitions` parameter**
   - Currently creates 1 partition
   - Should create N partitions as requested
   - **This is the actual bug causing the issue**

2. **Fix CreateTopics API version compatibility**
   - Support v0-v4 for older clients
   - Currently fails with kafka-python

### What We DON'T Need to Implement ❌ NOT NEEDED

1. **Server-side Murmur2 message partitioning**
   - NOT part of Kafka protocol
   - Clients always specify partition in Produce request
   - Would add unnecessary complexity

2. **Custom partitioning strategies**
   - Kafka clients support pluggable partitioners
   - Client-side implementation
   - Server is partition-agnostic

### What We ALREADY Have ✅ COMPLETE

1. **Murmur2 for coordinator assignment**
   - Used for FindCoordinator API
   - Maps consumer groups to brokers
   - Already implemented and working

2. **Partition index handling**
   - Correctly reads partition from Produce request
   - Writes to specified partition
   - No changes needed

---

## Testing to Confirm

### Test 1: Verify Client Calculates Partition

**Run with debug logging**:
```bash
RUST_LOG=debug chronik-server
```

**Expected logs** (from our debug code):
```
PARTITION_DEBUG: PRODUCE topic=test-topic partition_count=1
PARTITION_DEBUG:   partition=0 records_bytes=150
```

**Confirms**: Client sends partition index explicitly.

### Test 2: Check Produce Request Wire Format

**Capture with tcpdump**:
```bash
tcpdump -i lo -w produce.pcap 'port 9092'
# Send messages with different keys
# Analyze in Wireshark
```

**Expected**: ProduceRequest contains `partition_index` field.

### Test 3: Test with Different Client Libraries

**kafka-python**:
```python
# Uses Murmur2 client-side
producer.send('topic', key=b'key-1', value=b'val')  # Client calculates partition
```

**rdkafka**:
```rust
// Uses Murmur2 client-side
producer.send(record);  // Client calculates partition
```

**Java client**:
```java
// Uses Murmur2 client-side (DefaultPartitioner)
producer.send(new ProducerRecord<>("topic", "key", "value"));
```

**Expected**: All clients work correctly because they calculate partitions themselves.

---

## Conclusion

### What's Broken ❌
- Topic creation defaults to 1 partition instead of requested count
- CreateTopics API has version compatibility issues

### What's Working ✅
- Chronik correctly handles partition indices from clients
- Murmur2 implementation exists for coordinator assignment
- No server-side message partitioning needed (by design)

### What to Fix 🔧

**Priority 1: Fix Topic Creation**
```rust
// Current (BROKEN):
let config = TopicConfig {
    partition_count: 1,  // ← Always 1!
    // ...
};

// Fixed:
let config = TopicConfig {
    partition_count: requested_partitions,  // ← From request
    // ...
};
```

**Priority 2: Fix CreateTopics API Compatibility**
- Support v0-v4 properly
- Test with kafka-python admin client

**Not Needed: Server-Side Partitioning**
- Already works correctly
- Clients handle all partitioning
- No changes needed

---

## Implementation Plan (Final)

### Phase 1: Fix Auto-Creation Default (2 hours)
**File**: `crates/chronik-protocol/src/handler.rs`

**Find**: `auto_create_topics()` function

**Change**:
```rust
let config = TopicConfig {
    partition_count: self.config.default_partition_count.unwrap_or(3),
    // ...
};
```

**Add config option**:
```rust
pub struct ServerConfig {
    pub default_partition_count: u32,  // Default: 3
    // ...
}
```

### Phase 2: Fix CreateTopics API (3 hours)
**File**: `crates/chronik-protocol/src/handler.rs`

**Function**: `handle_create_topics()`

**Fix**: Ensure parsing works for v0-v4 (non-compact encoding)

**Test**: With kafka-python admin client

### Phase 3: Testing (2 hours)
1. Test topic creation with explicit partition count
2. Test auto-creation with new default
3. Test consumer groups with multiple partitions
4. Test with kafka-python, rdkafka, and Java clients

**Total**: 7 hours

---

## References

### Kafka Protocol Specification
- ProduceRequest includes `partition_index` (required field)
- No "calculate partition from key" server-side operation
- Source: https://kafka.apache.org/protocol.html#The_Messages_Produce

### Kafka Client Partitioning
- **Java**: `org.apache.kafka.clients.producer.internals.DefaultPartitioner`
- **Python**: `kafka.partitioner.default.DefaultPartitioner`
- **Go**: `sarama.HashPartitioner`
- **Rust**: `rdkafka::producer::DefaultPartitioner`

All implement Murmur2 hashing **client-side**.

---

**Final Answer**:
- ✅ **Murmur2 is NOT needed for message partitioning** (clients handle it)
- ✅ **Chronik already has Murmur2** (for coordinator assignment)
- ✅ **Just need to fix topic creation** (honor requested partition count)
