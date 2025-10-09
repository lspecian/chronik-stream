# Release Notes - v1.3.37

**Version**: v1.3.37
**Date**: 2025-10-08
**Type**: Critical Bugfix
**Status**: In Progress

## Critical Issue Found in v1.3.36

**Problem**: v1.3.36 produce path is **fundamentally broken**.

**Root Cause**: Tried to parse `CanonicalRecord` from `ProduceRequest.partition_data.records` BEFORE offset assignment and batch formation.

**Error**: `"RecordBatch too small (minimum 61 bytes)"`

**Why It Fails**:
1. `ProduceRequest.partition_data.records` contains RAW wire bytes
2. These bytes might be empty, partial, or not yet formed into complete batches
3. **Offsets are NOT assigned yet** - they're assigned by ProduceHandler
4. **Batches are NOT formed yet** - they're formed by ProduceHandler
5. CanonicalRecord.from_kafka_batch() expects a COMPLETE, VALID Kafka RecordBatch

**Architectural Mistake**:
```rust
// v1.3.36 - WRONG
pub async fn handle_produce(request: ProduceRequest) -> Result<ProduceResponse> {
    // ❌ BROKEN: partition_data.records is not a complete batch!
    let canonical_record = CanonicalRecord::from_kafka_batch(&partition_data.records)?;

    // Write to WAL...
    // Then proceed with produce handling...
}
```

## The Correct Approach (v1.3.37)

**Principle**: WAL should store what ProduceHandler ACTUALLY wrote to buffer, not what client sent.

**Why**:
- ProduceHandler assigns offsets
- ProduceHandler forms complete batches
- ProduceHandler handles compression
- **THAT** is what should go to WAL

**Correct Flow**:
```
1. Client sends ProduceRequest with raw bytes
2. ProduceHandler:
   - Decodes records
   - Assigns offsets
   - Forms batches
   - Writes to buffer (raw Kafka bytes)
   - Stores batch metadata
3. WAL Integration intercepts the FORMED batches:
   - Converts raw buffer bytes → CanonicalRecord
   - Writes to WAL V2
4. Return response to client
```

**Implementation Strategy**:
- Keep ProduceHandler unchanged (it works correctly)
- Add a hook AFTER buffer write to capture formed batches
- Convert those batches to CanonicalRecord
- Write to WAL V2

## Changes in v1.3.37

### 1. Revert v1.3.36 Produce Path

**Status**: Pending

Revert the broken `handle_produce()` in wal_integration.rs that tries to parse from ProduceRequest.

### 2. Add Post-Produce WAL Hook

**Status**: Pending

Add method in ProduceHandler to capture formed batches after they're written to buffer:

```rust
impl ProduceHandler {
    pub async fn get_last_written_batches(&self) -> Vec<(String, i32, Bytes)> {
        // Return (topic, partition, raw_batch_bytes) for last produce
    }
}
```

### 3. Update WAL Integration

**Status**: Pending

Write to WAL AFTER produce succeeds, using actual formed batches:

```rust
pub async fn handle_produce(request: ProduceRequest) -> Result<ProduceResponse> {
    // 1. Normal produce (forms batches, assigns offsets)
    let response = self.inner_handler.handle_produce(request).await?;

    // 2. Get the batches that were actually written
    let batches = self.inner_handler.get_last_written_batches().await;

    // 3. Convert to CanonicalRecord and write to WAL V2
    for (topic, partition, batch_bytes) in batches {
        let canonical = CanonicalRecord::from_kafka_batch(&batch_bytes)?;
        let serialized = bincode::serialize(&canonical)?;
        manager.append_canonical(topic, partition, serialized).await?;
    }

    Ok(response)
}
```

### 4. Alternative Simpler Approach (RECOMMENDED)

**Don't convert at all in produce path** - just keep using V1!

**Why**:
- V1 works perfectly for WAL durability
- WalIndexer ALREADY converts V1 → Tantivy with CanonicalRecord
- Trying to convert in hot path adds complexity and risk
- V2 is for OPTIMIZED storage, not mandatory

**New Strategy for v1.3.37**:
1. **Keep V1 in produce path** (individual records to WAL)
2. **WalIndexer converts V1 → CanonicalRecord → Tantivy** (already implemented)
3. **Fetch path handles both** V1 (from WAL) and V2 (from Tantivy) (already implemented in v1.3.36)
4. **Recovery handles both** V1 and V2 (already implemented in v1.3.36)

**Result**: System works end-to-end without breaking produce!

## Decision

**RECOMMENDED**: Use simpler approach - **revert produce path to V1, keep everything else from v1.3.36**.

**Rationale**:
- ✅ Produce path proven to work with V1
- ✅ WalIndexer converts to CanonicalRecord for Tantivy
- ✅ Fetch and recovery already handle both formats
- ✅ Zero risk of breaking production writes
- ✅ V2 optimization happens in background (WalIndexer), not hot path

## Implementation Plan for v1.3.37

1. **Revert `handle_produce()` in wal_integration.rs to V1 approach**
   - Keep existing V1 record parsing
   - Write individual records to WAL with append()
   - Remove CanonicalRecord conversion from produce path

2. **Keep v1.3.36 fetch and recovery improvements**
   - fetch_from_wal() handles V2 ✅ (already done)
   - restore_partition_state() handles V2 ✅ (already done)

3. **Test end-to-end**:
   - Produce with kafka-python
   - Verify WAL has V1 records
   - Wait for WalIndexer to run
   - Verify Tantivy segments created
   - Fetch and verify data returned

## Status

- [ ] Revert produce path to V1
- [ ] Test produce/fetch
- [ ] Test WAL indexing
- [ ] Test recovery
- [ ] Update documentation

---

**Next**: Implement v1.3.37 with reverted produce path.
