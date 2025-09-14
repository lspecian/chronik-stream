# Release v0.7.6: Message Duplication Fix

## Critical Bug Fix

This release fixes a **critical message duplication bug** introduced in v0.7.5 where messages were being returned multiple times to consumers.

### The Problem

Version 0.7.5 introduced a high watermark synchronization fix that inadvertently caused messages to be duplicated. The issue was caused by:

1. **Double buffer updates**: The `flush_partition` method was updating the fetch handler buffer with messages that had already been added during production
2. **Buffer append instead of replace**: The `update_buffer` method was using `extend()` to append records instead of replacing them, causing messages to accumulate in the buffer multiple times

### The Solution

- **Removed duplicate buffer update** in `flush_partition` - messages are now only added to the buffer once during production
- **Fixed buffer update logic** to replace records instead of appending them
- **Simplified fetch logic** to properly handle the transition from buffer to segments

### Impact

Users of v0.7.5 may have experienced duplicate messages being delivered to consumers. **All users should upgrade to v0.7.6 immediately** to resolve this issue.

## Changes

### Core Fixes
- Removed duplicate `update_buffer` call in `flush_partition` method
- Changed buffer update from `extend()` to direct assignment to prevent accumulation
- Cleaned up unnecessary deduplication logic that was attempting to work around the root cause

### Files Changed
- `crates/chronik-server/src/produce_handler.rs` - Removed duplicate buffer update
- `crates/chronik-server/src/fetch_handler.rs` - Fixed buffer update logic

## Testing

The fix has been validated with:
- Single partition test scenarios
- Multi-partition test scenarios  
- Consumer group offset tracking tests

## Known Issues

While this release significantly reduces duplication compared to v0.7.5, some edge cases may still show partial duplication when messages transition from buffer to segments. A complete architectural review is planned for v0.8.0 to ensure clean separation between buffered and persisted messages.

## Upgrade Instructions

1. Stop your current Chronik Stream server
2. Upgrade to version 0.7.6
3. Restart the server
4. Run validation tests to ensure no duplication

```bash
# Upgrade
cargo install chronik-stream --version 0.7.6

# Or if building from source
git pull
cargo build --release

# Test the fix
python3 compat-tests/test_v076_duplication_fix.py
```

## Docker Users

Docker image will be available at:
```bash
docker pull ghcr.io/chronik-stream/chronik-stream:0.7.6
docker pull ghcr.io/chronik-stream/chronik-stream:latest
```

## Compatibility

This version maintains full compatibility with Kafka clients and the existing Chronik Stream API. No client-side changes are required.

## Next Steps

Version 0.8.0 will include a more comprehensive redesign of the buffer/segment interaction to completely eliminate any possibility of message duplication.

---

**Note**: This is a critical fix release for v0.7.5. Users should skip v0.7.5 and upgrade directly to v0.7.6.