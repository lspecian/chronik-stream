# Chronik Stream v1.0.3 Release Notes

## 🎉 Critical Fixes Release

This release resolves critical issues with message fetching and segment persistence that prevented consumers from retrieving messages.

## 🐛 Bug Fixes

### Fixed Consumer Fetch Issues
- **ListOffsets v0 Protocol Fix**: Corrected the response format for ListOffsets API v0 to properly return an array of offsets instead of empty results
- **Consumer Loop Resolution**: Fixed issue where consumers were stuck in an infinite ListOffsets loop, repeatedly requesting EARLIEST offset without progressing to fetch phase
- **Segment Not Found Errors**: Resolved "NotFound" errors when fetching segments due to missing physical files

### Fixed Segment Persistence
- **Immediate Segment Write**: Segments are now written to disk immediately when metadata is created, not just on rotation
- **Physical File Creation**: Fixed critical issue where segment metadata was registered but physical files were never written to object store
- **Metadata Consistency**: Ensured segment metadata and physical files remain synchronized

## ✨ Improvements

### Enhanced Debugging
- Added comprehensive debug logging for segment read/write operations
- Improved error messages to show expected segment paths and actual errors
- Better tracking of segment lifecycle from creation to retrieval

### Code Quality
- Made `SegmentBuilder` cloneable for safe segment reuse
- Improved error handling in fetch handler
- Better separation of concerns between metadata and physical storage

## 📊 Test Results

```
Testing Fetch Handler Fix
==================================================
Messages sent: 5
Messages received: 5
🎉 SUCCESS: Fetch handler is working correctly!
```

## 🔧 Technical Details

### Files Modified
- `crates/chronik-protocol/src/handler.rs` - Fixed ListOffsets v0 encoding
- `crates/chronik-storage/src/segment_writer.rs` - Added immediate segment persistence
- `crates/chronik-storage/src/segment.rs` - Made SegmentBuilder cloneable
- `crates/chronik-server/src/fetch_handler.rs` - Added debug logging

### Breaking Changes
None - This is a backward-compatible bug fix release.

## 📦 Installation

```bash
# Using cargo
cargo install chronik-server --version 1.0.3

# From source
git clone https://github.com/lspecian/chronik-stream
cd chronik-stream
git checkout v1.0.3
cargo build --release
```

## 🙏 Acknowledgments

Thanks to all users who reported fetch issues. This release ensures reliable message storage and retrieval for production use.

## 📝 Full Changelog

See [GitHub Commits](https://github.com/lspecian/chronik-stream/compare/v1.0.2...v1.0.3)

---
*Released: 2025-09-14*