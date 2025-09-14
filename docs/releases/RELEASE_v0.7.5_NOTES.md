# Release v0.7.5: Critical High Watermark Synchronization Fix

## Critical Bug Fix

This release fixes a **critical data loss bug** where messages were being lost due to incorrect high watermark tracking between the ProduceHandler and FetchHandler components.

### The Problem

Messages produced with `acks=0` or `acks=1` were being buffered correctly but were not fetchable by consumers. This was caused by:

1. **Missing high watermark updates**: The ProduceHandler was only updating the high watermark for `acks=-1`, not for `acks=0` or `acks=1`
2. **Incomplete watermark checking**: The FetchHandler was only checking segment watermarks, not buffer watermarks
3. **Logic error**: The condition `fetch_offset < high_watermark` prevented fetches when both values were 0

### The Solution

- **ProduceHandler** now correctly updates high watermark for all acknowledgment modes (`acks=0`, `acks=1`, and `acks=-1`)
- **FetchHandler** now checks both segment and buffer high watermarks to determine data availability
- **Initialization order** has been corrected to ensure FetchHandler is available before ProduceHandler starts

### Impact

**All users should upgrade to this version immediately** as previous versions may lose messages under certain conditions, particularly when using `acks=0` or `acks=1` modes.

## Changes

### Core Fixes
- Fixed high watermark updates in ProduceHandler for all ack modes
- Added buffer watermark checking in FetchHandler
- Corrected initialization order in integrated_server

### Testing
- Added comprehensive unit tests for high watermark logic
- Added regression tests to prevent future breakage
- Added multi-message consumption tests
- All existing tests continue to pass

### Files Changed
- `crates/chronik-server/src/produce_handler.rs` - High watermark updates
- `crates/chronik-server/src/fetch_handler.rs` - Buffer watermark checking
- `crates/chronik-server/src/integrated_server.rs` - Initialization order
- `crates/chronik-server/src/fetch_handler_test.rs` - New test suite
- `compat-tests/test_regression_multi_message.py` - Regression test

## Upgrade Instructions

1. Stop your current Chronik Stream server
2. Upgrade to version 0.7.5
3. Restart the server
4. Verify message delivery with the included regression test

```bash
# Upgrade
cargo install chronik-stream --version 0.7.5

# Or if building from source
git pull
cargo build --release

# Test the fix
python3 compat-tests/test_regression_multi_message.py
```

## Verification

After upgrading, you can verify the fix is working by running:

```bash
python3 compat-tests/test_regression_multi_message.py
```

This should show:
```
✓✓✓ REGRESSION TEST PASSED ✓✓✓
Multiple messages per partition are correctly stored and retrieved
```

## Docker Users

Docker image available at:
```bash
docker pull ghcr.io/chronik-stream/chronik-stream:0.7.5
docker pull ghcr.io/chronik-stream/chronik-stream:latest
```

## Compatibility

This version maintains full compatibility with Kafka clients and the existing Chronik Stream API. No client-side changes are required.

## Contributors

- Fix identified and implemented by the Chronik Stream team
- Special thanks to users who reported message loss issues

## Next Steps

We are implementing additional monitoring and testing to catch similar issues earlier in the development cycle.

---

**Note**: This is a critical fix release. All production deployments should upgrade immediately.