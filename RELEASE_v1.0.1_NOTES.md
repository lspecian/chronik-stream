# Chronik Stream v1.0.1 Release Notes

## Overview

Chronik Stream v1.0.1 is a patch release that fixes critical compilation issues that prevented v1.0.0 from building successfully. This release ensures the codebase compiles cleanly and is ready for production deployment.

## Bug Fixes

### 🔧 Compilation Fixes

- **TopicPartition Serialization Support**: Fixed missing `serde::Serialize` and `serde::Deserialize` derives on the `TopicPartition` struct in `crates/chronik-wal/src/manager.rs:22`. This was causing compilation errors when WAL replication features were enabled.

- **Async Send Trait Violations**: Resolved critical Send trait violations in `crates/chronik-wal/src/replication.rs` where parking lot `RwLockReadGuard`s were being held across `.await` points. Fixed by collecting transport references before async operations to ensure proper trait bounds.

- **Version Consistency**: Updated workspace version from 1.0.0 to 1.0.1 across all crates to ensure version consistency.

## Technical Details

### WAL Manager Fixes
- Added `serde` import to manager.rs
- Added `Serialize, Deserialize` derives to `TopicPartition` struct
- Ensures proper serialization support for WAL replication functionality

### Replication Layer Fixes  
- Modified async code in replication module to avoid holding lock guards across await points
- Used pre-collection pattern to gather transport references before async operations
- Maintains thread safety while satisfying Rust's Send trait requirements

### Build System
- Full release build now completes successfully with only warnings (no errors)
- All core functionality preserved during compilation fixes
- Maintains backward compatibility with existing deployments

## Testing

- ✅ Full `cargo build --release` completes successfully
- ✅ All WAL functionality compiles without errors  
- ✅ Replication module builds with proper async Send support
- ✅ No breaking changes to public APIs

## Deployment Notes

This is a drop-in replacement for v1.0.0. No configuration changes or migration steps are required.

## Files Changed

- `crates/chronik-wal/src/manager.rs` - Added serde derives
- `crates/chronik-wal/src/replication.rs` - Fixed Send trait violations  
- `Cargo.toml` - Version bump to 1.0.1

---

**Release Date**: September 13, 2025  
**Git Tag**: v1.0.1