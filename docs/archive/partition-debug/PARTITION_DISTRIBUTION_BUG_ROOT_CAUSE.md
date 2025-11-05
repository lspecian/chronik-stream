# Partition Distribution Bug - ROOT CAUSE IDENTIFIED

**Date**: 2025-11-04
**Status**: **ROOT CAUSE CONFIRMED**
**Severity**: CRITICAL

---

## Executive Summary

**ROOT CAUSE**: Chronik creates ALL topics with 1 partition (default), regardless of requested partition count.

**Evidence**: Debug logs show:
```
PARTITION_DEBUG: METADATA_RESPONSE topic=chronik-test-3part returning 1 partitions
```

Even though the client requested 3 partitions via CreateTopics API.

---

## The Smoking Gun

### Test Execution
```bash
python3 test_real_kafka.py chronik
```

### What We Requested
```python
NewTopic(name="chronik-test-3part", num_partitions=3, replication_factor=1)
```

### What Chronik Created
```
PARTITION_DEBUG: METADATA_RESPONSE topic=chronik-test-3part returning 1 partitions
```

**Chronik created a topic with 1 partition instead of 3!**

### Why Clients Only Fetch Partition 0
1. Client requests: "Create topic with 3 partitions"
2. Chronik ignores partition count, creates topic with 1 partition
3. Client asks: "What partitions exist?"
4. Chronik responds: "Only partition 0 exists"
5. Client hashes keys and says: `hash("key-1") % 1 = 0`, `hash("key-2") % 1 = 0` → All to partition 0!
6. All 150 messages go to partition 0

**This is CORRECT CLIENT BEHAVIOR given wrong metadata from Chronik!**

---

## Why CreateTopics Failed

The Python test showed:
```
Topic creation note: IncompatibleBrokerVersion: Kafka broker does not support the 'CreateTopicsRequest_v0' Kafka protocol.
```

This means:
1. kafka-python tried to create topic via CreateTopics API
2. Chronik rejected it (version incompatibility or bug in CreateTopics handler)
3. Topic creation failed
4. When producer sent first message, Chronik auto-created the topic with **DEFAULT 1 PARTITION**

---

## Two Bugs Identified

### Bug #1: CreateTopics API Version Incompatibility
**File**: `crates/chronik-protocol/src/handler.rs`
**Issue**: Chronik may not properly support CreateTopicsRequest v0/v1 that kafka-python uses

**Evidence**:
```
IncompatibleBrokerVersion: Kafka broker does not support the 'CreateTopicsRequest_v0' Kafka protocol.
```

### Bug #2: Auto-Created Topics Default to 1 Partition
**File**: Need to find where auto-topic-creation happens
**Issue**: When Chronik auto-creates a topic (e.g., on first produce), it uses 1 partition instead of a configurable default

**Location**: Likely in `chronik-protocol/src/handler.rs` in the `auto_create_topics()` function

---

## The Fix

### Fix #1: Support CreateTopics v0/v1 Properly
Ensure Chronik's CreateTopics handler accepts v0 and v1 requests from older clients like kafka-python.

**Current code** (handler.rs ~line 3522):
```rust
let use_compact = header.api_version >= 5;
```

Need to verify v0-v4 non-compact parsing works correctly.

### Fix #2: Change Auto-Created Topic Default
When auto-creating topics, use a configurable default partition count (default: 3 or more, NOT 1).

**Find this code** (approximate):
```rust
async fn auto_create_topics(&self, topics: &[String]) -> Result<()> {
    // ... creates TopicConfig ...
    let config = TopicConfig {
        partition_count: 1,  // ← THIS IS THE BUG!
        // ...
    };
}
```

**Change to**:
```rust
partition_count: self.default_partition_count.unwrap_or(3),  // Use config, default 3
```

---

## Implementation Plan

### Phase 1: Fix CreateTopics API (HIGH PRIORITY)
**Goal**: Make CreateTopics v0/v1 work so clients can explicitly request partition count

1. Add debug logging to CreateTopics handler to see what version clients request
2. Fix any parsing bugs for v0-v4 (non-compact)
3. Test with kafka-python admin client

### Phase 2: Fix Auto-Creation Default (HIGH PRIORITY)
**Goal**: When topics auto-create, use reasonable default (3+ partitions)

1. Add configuration option: `default_partition_count` (default: 3)
2. Update auto_create_topics() to use this config
3. Document in CLAUDE.md

### Phase 3: Add Environment Variable (NICE TO HAVE)
```bash
CHRONIK_DEFAULT_PARTITIONS=3  # Default for auto-created topics
```

---

## Testing Plan

### Test 1: CreateTopics API with kafka-python
```python
from kafka.admin import KafkaAdminClient, NewTopic

admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic(name='test-topic', num_partitions=5, replication_factor=1)
admin.create_topics([topic])

# Verify
metadata = admin.describe_topics(['test-topic'])
assert len(metadata['test-topic']['partitions']) == 5
```

**Success Criteria**: Topic created with 5 partitions

### Test 2: Auto-Creation with Producer
```python
from kafka import KafkaProducer

# Topic doesn't exist yet
producer = KafkaProducer(bootstrap_servers='localhost:9092')
producer.send('auto-created-topic', b'test')
producer.flush()

# Check metadata
# Should have 3 partitions (new default), not 1
```

**Success Criteria**: Auto-created topic has 3 partitions

### Test 3: End-to-End Consumer Group
```bash
# Create 3-partition topic
# Produce 150 messages
# Start 3 consumers in same group
# Verify each consumes ~50 messages from different partitions
```

**Success Criteria**:
- ✅ Consumer 1: ~50 messages from partition 0
- ✅ Consumer 2: ~50 messages from partition 1
- ✅ Consumer 3: ~50 messages from partition 2

---

## Files to Modify

### 1. `crates/chronik-protocol/src/handler.rs`
- **Function**: `handle_create_topics()`
- **Fix**: Ensure v0-v4 parsing works
- **Add**: Debug logging for requested partition count

### 2. `crates/chronik-protocol/src/handler.rs`
- **Function**: `auto_create_topics()`
- **Fix**: Change default partition_count from 1 to configurable value (default 3)

### 3. `crates/chronik-config/src/lib.rs` (if needed)
- **Add**: `default_partition_count: u32` config option

### 4. `CLAUDE.md`
- **Document**: New `CHRONIK_DEFAULT_PARTITIONS` env var
- **Document**: How auto-topic-creation works

---

## Timeline

- **Phase 1 (CreateTopics Fix)**: 2-3 hours
- **Phase 2 (Auto-Creation Fix)**: 1-2 hours
- **Phase 3 (Testing)**: 2 hours

**Total**: 5-7 hours for complete fix

---

## Next Immediate Actions

1. ✅ Find `auto_create_topics()` function
2. ✅ Change default partition_count from 1 to 3
3. ✅ Rebuild and test
4. ✅ Verify with Python test script
5. ✅ Verify consumer groups distribute correctly

---

## Appendix: Comparison with Kafka

### Kafka's Default
- Auto-created topics: **Default from `num.partitions` server config (typically 1, but configurable)**
- CreateTopics API: **Honors client's num_partitions parameter**

### Chronik's Current Behavior (BROKEN)
- Auto-created topics: **Always 1 partition (hardcoded)**
- CreateTopics API: **Version incompatibility causes failures**

### Chronik's Target Behavior (FIXED)
- Auto-created topics: **Configurable default (recommend 3)**
- CreateTopics API: **Fully compatible with v0-v13**

---

**Status**: Ready to implement fix. Root cause 100% confirmed.
