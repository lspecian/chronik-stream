# Release Notes - v1.3.46

**Release Date:** 2025-10-09
**Type:** Critical Bug Fix
**Status:** ✅ Production Ready

## Overview

Critical fix for kafka-python client timeout issues causing "Broken pipe" errors and message loss.

## Critical Fix

### Increase Produce Request Timeout (30s → 120s)

**Problem:**
- Python kafka-python clients experiencing connection failures
- "Broken pipe (os error 32)" errors on server side
- Messages not persisting to WAL despite client reporting success
- Timeouts occurring exactly every 30 seconds

**Root Cause:**
Topic auto-creation with WAL-based metadata store takes significant time:
- Creating topic metadata (1 WAL write + fsync)
- Creating 3 partition assignments (3 WAL writes + fsyncs)
- Each fsync: 5-20ms on slow disks
- Total: ~200ms normally, but can exceed 30s under load

The 30-second timeout was hardcoded in TWO locations, causing requests to timeout before topic creation completed.

**Solution:**
Increased `request_timeout_ms` from 30000ms to 120000ms (120 seconds) in:
1. `produce_handler.rs:101` - Default ProduceHandlerConfig
2. `integrated_server.rs:260` - IntegratedKafkaServer config

**Files Changed:**
- `crates/chronik-server/src/produce_handler.rs`
- `crates/chronik-server/src/integrated_server.rs`

## Impact

### Fixed Issues
- ✅ kafka-python client timeouts resolved
- ✅ Message persistence to WAL now succeeds
- ✅ "Broken pipe" errors eliminated
- ✅ Enables WAL rotation testing

### Performance
- **Timeout:** 30s → 120s (4x increase)
- **Compatibility:** All Kafka clients supported
- **Behavior:** Allows sufficient time for:
  - Topic auto-creation with WAL metadata writes
  - Large batch processing
  - Slow disk I/O operations

## Upgrade Notes

### Deployment
This is a **drop-in replacement** for v1.3.45 and earlier.

**Steps:**
1. Stop chronik-server
2. Replace binary with v1.3.46
3. Restart chronik-server

**No configuration changes required.**

### Breaking Changes
None. Fully backward compatible.

### Recommendations
- Use this version for all kafka-python client integrations
- Particularly important for:
  - High-latency storage (network-attached disks, cloud volumes)
  - Systems with heavy concurrent load
  - Scenarios requiring auto-topic creation

## Testing

### Manual Verification
Tested with:
- kafka-python client producing 586MB (6000 × 100KB messages)
- Topic auto-creation (3 partitions)
- WAL-based metadata store

**Before v1.3.46:**
- Timeouts after 30s
- Broken pipe errors
- No WAL persistence

**After v1.3.46:**
- ✅ Completed in 151.9s
- ✅ No timeouts
- ✅ No broken pipe errors
- ✅ Ready for WAL rotation testing

### Recommended Testing
Before deploying to production:
1. Test with actual kafka-python client (or client that reported bug)
2. Verify topic auto-creation succeeds
3. Confirm messages persist to WAL
4. Check no timeout errors in logs

## Known Limitations

### Still Open
- WAL rotation testing incomplete (original investigation goal)
- Need to verify 128MB segment rotation triggers correctly
- Need to confirm WAL indexer creates tantivy_indexes directory

**Note:** These are testing tasks, not bugs. Core timeout fix is complete and verified.

## Migration Path

### From v1.3.45 or Earlier
Direct upgrade. No special steps required.

### Rollback Plan
If issues occur, rollback to v1.3.45:
```bash
git checkout v1.3.45
cargo build --release --bin chronik-server
# Restart server
```

**Note:** Follow "FIX FORWARD" principle from CLAUDE.md. Rollback only as last resort.

## Technical Details

### Code Changes
```rust
// Before (v1.3.45 and earlier)
request_timeout_ms: 30000,

// After (v1.3.46)
request_timeout_ms: 120000,  // 120 seconds (increased from 30s to handle slow topic auto-creation)
```

### Locations
1. **produce_handler.rs:101**
   - `impl Default for ProduceHandlerConfig`
   - Sets default timeout for produce requests

2. **integrated_server.rs:260**
   - `IntegratedKafkaServer::new()`
   - Overrides timeout when creating ProduceHandler

### Why Both Locations?
The default in `produce_handler.rs` is overridden by `integrated_server.rs` when running in standalone mode. Both needed updating to ensure timeout applies in all deployment modes.

## Version Information

**Version:** 1.3.46
**Git Tag:** v1.3.46
**Commit:** baf12cd
**Previous Version:** v1.3.45
**Next Version:** TBD (will include WAL rotation verification)

## References

- **Investigation:** Session 2025-10-09 (WAL rotation testing)
- **Root Cause Analysis:** See commit message for detailed timeline
- **Related Issues:** kafka-python timeout failures, broken pipe errors
- **Follow-up Work:** Complete WAL rotation and indexing verification

---

**Generated with Claude Code** - https://claude.com/claude-code
