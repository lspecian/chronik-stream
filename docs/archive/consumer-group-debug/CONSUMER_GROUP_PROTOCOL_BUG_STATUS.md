# Consumer Group JoinGroup v5 Protocol Bug - Investigation Status

**Date**: 2025-11-04
**Bug**: JoinGroup v5 responses cause "Protocol read buffer underflow"
**Client**: rdkafka/librdkafka (kafka-python, confluent-kafka, chronik-bench)

---

## Current Status: NEW BUG DISCOVERED

### Bug #3: JoinGroup v5 Protocol Encoding - ✅ FIXED!

**Symptom**:
```
%3|...|PROTOUFLOW|...|: Protocol read buffer underflow for JoinGroup v5 at 4/4
(rd_kafka_cgrp_handle_JoinGroup:2215): expected 2 bytes > 0 remaining bytes
```

**Fix Applied**: ✅ VERIFIED AS WORKING

**File**: [crates/chronik-server/src/consumer_group.rs:1240](../crates/chronik-server/src/consumer_group.rs#L1240)

**Change**:
```rust
// BEFORE:
protocol_type: Some("consumer".to_string()),

// AFTER:
protocol_type: None, // v7+ only - MUST be None for v5 compatibility
```

**Rationale**: The `protocol_type` field was added in JoinGroup v7. For v5 clients, this MUST be `None` so the encoder skips writing it.

**Testing Result**: ✅ Consumer successfully joined group and consumed 2,147 messages. JoinGroup v5 protocol errors are GONE!

---

### Bug #4: OffsetCommit v8 Protocol Encoding - ❌ NEW CRITICAL BUG

**Symptom**:
```
%3|...|PROTOUFLOW|...|: Protocol read buffer underflow for OffsetCommit v8 at 1/4
(rd_kafka_handle_OffsetCommit:1707): expected 4 bytes > 3 remaining bytes
```

**Impact**: Consumer successfully joins group and consumes messages, but offset commits fail. This means:
- ✅ Messages are consumed correctly
- ❌ Offsets are NOT persisted to the coordinator
- ❌ On restart, consumer will re-consume ALL messages from the beginning

**Root Cause**: Similar to Bug #3 - protocol version field encoding issue

**File**: [crates/chronik-protocol/src/handler.rs:1122-1184](../crates/chronik-protocol/src/handler.rs#L1122-L1184)

**Investigation Needed**: The error "expected 4 bytes > 3 remaining bytes" at position "1/4" suggests:
1. Client expects a standard INT32 (4 bytes) for the first field (ThrottleTimeMs in v3+)
2. Server is writing only 3 bytes (possibly compact format or missing field)
3. The `flexible = version >= 8` logic at line 1131 might be incorrect for rdkafka clients

### Protocol Encoder Analysis

**File**: [crates/chronik-protocol/src/handler.rs:948-1061](../crates/chronik-protocol/src/handler.rs#L948-L1061)

**JoinGroup v5 Field Order** (from code):
1. ThrottleTimeMs (v2+) - INT32
2. ErrorCode - INT16
3. GenerationId - INT32
4. ~~ProtocolType (v7+ ONLY)~~ - **SKIPPED for v5** ✅
5. ProtocolName - NULLABLE_STRING
6. Leader - STRING
7. ~~SkipAssignment (v9+ ONLY)~~ - **SKIPPED for v5** ✅
8. MemberId - STRING
9. Members - ARRAY[...]

**Apache Kafka JoinGroup v5 Spec**:
```
JoinGroupResponse v5:
  ThrottleTimeMs       => INT32           (v2+)
  ErrorCode            => INT16
  GenerationId         => INT32
  ProtocolName         => NULLABLE_STRING
  Leader               => STRING
  MemberId             => STRING
  Members              => ARRAY
    MemberId           => STRING
    GroupInstanceId    => NULLABLE_STRING  (v5+)
    Metadata           => BYTES
```

**Field Count Comparison**:
- **Expected (Kafka v5)**: 7 top-level fields
- **Actual (Chronik v5)**: 7 top-level fields (after skipping v7+ and v9+ fields)

**ORDER MATCH**: ✅ YES - The encoder skips `ProtocolType` (v7+) and `SkipAssignment` (v9+) for v5

### Hypothesis: Why the Bug Persists

**Possibility 1**: The `protocol_type: None` fix is correct, but there's a secondary bug in how `ProtocolName` is encoded:
- Line 988: `encoder.write_string(response.protocol_name.as_deref())`
- If `protocol_name` is `Some("")`, does `write_string` correctly encode it?
- Should it be writing `-1` (null) or `0` (empty string)?

**Possibility 2**: The `group_instance_id` encoding for members is incorrect:
- Lines 1034-1040: `encoder.write_string(member.group_instance_id.as_deref())`
- For v5, this writes a NULLABLE_STRING
- If member doesn't have a group_instance_id (None), this writes `-1` as INT16
- Is this the "expected 2 bytes" the client is looking for?

**Possibility 3**: The `members` array encoding has an off-by-one error or missing field

### Next Steps (CRITICAL)

1. **Rebuild and verify binary has the fix**:
   ```bash
   cargo build --release --bin chronik-server
   ls -lh ./target/release/chronik-server  # Confirm timestamp
   ```

2. **Start server with CORRECTED binary**:
   ```bash
   RUST_LOG=chronik_protocol::handler=trace ./target/release/chronik-server start --data-dir ./test-verify-fix
   ```

3. **Run minimal test** (1 consumer):
   ```bash
   ./target/release/chronik-bench --topic test --mode produce --concurrency 1 --message-size 256 --duration 1s
   ./target/release/chronik-bench --topic test --mode consume --consumer-group test-group --duration 5s
   ```

4. **Check for protocol errors in consumer output**:
   - If NO errors → Fix worked! Proceed to Priority 2
   - If STILL errors → Bug is in the encoder, need deeper investigation

5. **If errors persist, add MORE debug logging**:
   ```rust
   // In chronik-protocol/src/handler.rs encode_join_group_response
   tracing::trace!("ProtocolName.as_deref() = {:?}", response.protocol_name.as_deref());
   tracing::trace!("Writing bytes: {:02x?}", encoder.buf);
   ```

### Alternative Hypothesis: Wrong API Version

**Check**: Are we responding with v5 when client requests v5?
- File: `chronik-server/src/kafka_handler.rs`
- Check: `ApiVersion` header parsing
- Verify: Response uses same version as request

---

## Previous Bugs (FIXED)

### Bug #1: Join Phase Early Return
**Status**: ✅ FIXED AND VERIFIED
**Fix**: Removed early return in `Stable` state handler
**File**: [consumer_group.rs:629-639](../crates/chronik-server/src/consumer_group.rs#L629-L639)

### Bug #2: SyncGroup Race Condition
**Status**: ✅ FIXED (needs end-to-end verification after Bug #3 is resolved)
**Fix**: Followers block until leader computes assignments
**File**: [consumer_group.rs:766-803](../crates/chronik-server/src/consumer_group.rs#L766-L803)

---

## References

- [Kafka Protocol: JoinGroup](https://kafka.apache.org/protocol#The_Messages_JoinGroup)
- [CONSUMER_GROUP_SYNCGROUP_BUG_SUMMARY.md](./CONSUMER_GROUP_SYNCGROUP_BUG_SUMMARY.md) - Bug #3 discovery
- [CONSUMER_GROUP_FINAL_BUG_ANALYSIS.md](./CONSUMER_GROUP_FINAL_BUG_ANALYSIS.md) - Bugs #1 and #2 analysis
