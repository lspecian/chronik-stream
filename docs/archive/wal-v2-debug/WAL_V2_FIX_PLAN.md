# WAL V2 Deserialization Fix Plan

## Issue
WAL V2 fails with "unexpected end of file" when deserializing CanonicalRecord, even though the write side correctly serializes it with bincode.

## Root Cause Hypothesis
Based on code analysis:
- Write: `bincode::serialize(&canonical_record)` → stores in WAL ✅
- Read: `bincode::deserialize::<CanonicalRecord>(&canonical_data)` → fails with EOF ❌

Possible causes:
1. **Partial data write** - canonical_data is truncated during WAL write
2. **Buffer mismatch** - Reading wrong slice of the WAL record
3. **Bincode format change** - CanonicalRecord structure changed

## Investigation Steps

### 1. Check if data is complete in WAL
Add logging to show canonical_data length during write vs read:

```rust
// In produce_handler.rs after serialize:
warn!("WAL WRITE: canonical_data length = {}", serialized.len());

// In fetch_handler.rs before deserialize:
warn!("WAL READ: canonical_data length = {}", canonical_data.len());
```

### 2. Check v2.1.0 vs current
```bash
git diff v2.1.0 HEAD -- crates/chronik-storage/src/canonical_record.rs
git diff v2.1.0 HEAD -- crates/chronik-wal/src/record.rs
```

### 3. Try raw deserialization test
Create a unit test that writes and reads a CanonicalRecord through WAL V2 format.

## Quick Fix Option
If deserialization is genuinely broken, we can:
1. Skip V2 deserialization errors (already done - returns empty)
2. Fall back to V1 format temporarily
3. Re-serialize data in correct format

## Long-term Fix
1. Identify why deserialization fails
2. Fix the format mismatch
3. Add migration for existing WAL data
4. Add regression test

## Action Items
- [ ] Add debug logging to both write and read paths
- [ ] Compare canonical_data lengths
- [ ] Check what changed since v2.1.0
- [ ] Test with fresh WAL (rm -rf data dir)
- [ ] Create minimal reproduction test
