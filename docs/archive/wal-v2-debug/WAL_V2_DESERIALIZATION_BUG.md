# CRITICAL: WAL V2 Deserialization Bug - Root Cause of Consumer Failures

**Date**: 2025-11-03
**Severity**: CRITICAL
**Status**: Root cause identified

## Executive Summary

The consumer group assignment issue was a **red herring**. The REAL bug is:

**WAL V2 fails to deserialize records it wrote, causing ALL consumers to receive 0 messages.**

## Discovery

While investigating consumer group assignments, traced the full consume path:

1. ✅ Consumer joins group - SUCCESS
2. ✅ Server assigns partitions - SUCCESS
3. ✅ Consumer sends Fetch request - SUCCESS
4. ✅ Server detects data available (HWM=1005, offset=0) - SUCCESS
5. ✅ WAL reads 1005 records from segment - SUCCESS
6. ❌ **WAL fails to deserialize records - FAILURE**

```
WAL read completed: found 1005 total records from 1 segment files
Failed to deserialize CanonicalRecord from WAL V2: io error: unexpected end of file
Failed to deserialize CanonicalRecord from WAL V2: io error: unexpected end of file
```

## Symptoms

- Server logs show "Data available for fetch"
- WAL successfully finds records: "found 1005 total records"
- But immediately fails: "Failed to deserialize CanonicalRecord from WAL V2: io error: unexpected end of file"
- Consumers receive 0 messages (empty Fetch responses)
- Happens with 1, 2, and 3 partition topics (tested all)

## Evidence

```
[10:48:58] INFO fetch_partition called - topic: test-1part, partition: 0, fetch_offset: 0, max_bytes: 1048576
[10:48:58] INFO High watermark for test-1part-0: 1005 (from ProduceHandler)
[10:48:58] INFO Data available for fetch - topic: test-1part, partition: 0, fetch_offset: 0, high_watermark: 1005
[10:48:58] INFO Trying to fetch raw bytes (CRC-preserving) for test-1part-0
[10:48:58] INFO fetch_raw_bytes - topic: test-1part, partition: 0, fetch_offset: 0, high_watermark: 1005
[10:48:58] INFO RAW→WAL: Buffer empty or no match, trying WAL
[10:48:58] INFO Reading from WAL (GroupCommitWal): topic=test-1part, partition=0, offset=0, max_records=104857
[10:48:58] INFO WAL read completed: found 1005 total records from 1 segment files
[10:48:58] WARN Failed to deserialize CanonicalRecord from WAL V2: io error: unexpected end of file
[10:48:58] WARN Failed to deserialize CanonicalRecord from WAL V2: io error: unexpected end of file
```

## Root Cause Analysis

The WAL V2 format has a deserialization bug where:

1. Records are **successfully written** to WAL (HWM advances correctly to 1005)
2. WAL **successfully finds** the records in segment files
3. But **fails to deserialize** with "unexpected end of file"

This is likely caused by:

- Incomplete/corrupt record serialization
- Mismatch between write format and read format
- Missing length prefix or incorrect size calculation
- Buffer boundary issue

## Impact

**ALL message consumption fails.** This affects:

- kafka-python consumers ❌
- rdkafka consumers (chronik-bench) ❌
- Any Kafka-compatible consumer ❌

The consumer group protocol is **working correctly**. The assignments are **correct**. The Fetch API is **working**. But the underlying WAL storage layer cannot retrieve the data.

## Files to Investigate

1. **`crates/chronik-wal/src/manager.rs`** - WAL read_from implementation
2. **`crates/chronik-wal/src/group_commit.rs`** - GroupCommitWal serialization
3. **`crates/chronik-server/src/fetch_handler.rs`** - Where deserialization fails (line ~870)

## Related Issues

- Consumer group bug investigation was actually correct - consumer groups work fine
- The "heartbeat not updating" bug was real and fixed
- The "assignment version mismatch" was real and fixed
- But the final issue was **WAL deserialization**, not consumer protocol

## Next Steps

1. **Find WAL V2 deserialization code** - locate where CanonicalRecord is deserialized
2. **Compare with write code** - ensure format matches
3. **Add debug logging** - log record sizes, offsets, buffer positions
4. **Test with single message** - isolate the deserialization failure
5. **Fix and verify** - ensure consumers can read messages

## Testing

To reproduce:

```bash
# Start server
./target/release/chronik-server start --data-dir ./test-wal

# Produce messages
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic test \
  --mode produce \
  --message-count 10

# Try to consume (will get 0 messages)
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic test \
  --mode consume \
  --duration 10s \
  --consumer-group test-group

# Check logs for:
grep "Failed to deserialize" <server-log>
```

## Priority

**P0 - CRITICAL**: This completely breaks message consumption. Must be fixed immediately before any consumer testing can proceed.
