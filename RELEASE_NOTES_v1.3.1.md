# Chronik Stream v1.3.1 - CRITICAL HOTFIX

## Release Date: September 24, 2025

## 🔴 CRITICAL BUG FIX

This is an emergency hotfix release that fixes a **critical crash** introduced in v1.3.0.

## The Issue

v1.3.0 had a severe buffer overflow bug that caused Chronik to crash immediately when any Kafka client connected. The crash occurred when parsing ApiVersions v3 requests with flexible protocol encoding.

### Crash Details
- **Error**: `thread 'tokio-runtime-worker' panicked at bytes-1.10.1/src/bytes.rs:711:9: cannot advance past remaining: 46 <= 4`
- **Exit Code**: 139 (SIGSEGV)
- **Trigger**: Any client connection attempting ApiVersions v3 request
- **Impact**: Complete service failure - Chronik could not run at all

## The Fix

Fixed the buffer overflow in the protocol parser by:
1. Adding proper bounds checking to the `decoder.advance()` method
2. Validating buffer has enough bytes before advancing
3. Adding safety limits for unreasonable tagged field sizes (>1MB)
4. Properly handling errors instead of panicking

## Changes

### Fixed
- ✅ **Buffer overflow in tagged field parsing** - No more panics when parsing flexible protocols
- ✅ **ApiVersions v3 request handling** - Properly parses requests without crashing
- ✅ **All advance() operations** - Now properly check bounds before advancing

### Technical Details

The bug was in `crates/chronik-protocol/src/parser.rs`:

```rust
// BEFORE (v1.3.0) - Would panic
pub fn advance(&mut self, n: usize) {
    self.buf.advance(n);  // Panics if n > remaining!
}

// AFTER (v1.3.1) - Returns error
pub fn advance(&mut self, n: usize) -> Result<()> {
    if self.buf.remaining() < n {
        return Err(Error::Protocol(format!(
            "Cannot advance {} bytes, only {} remaining",
            n, self.buf.remaining()
        )));
    }
    self.buf.advance(n);
    Ok(())
}
```

## Upgrading

**URGENT**: All users of v1.3.0 must upgrade immediately as that version is completely non-functional.

```bash
# Using Docker
docker pull ghcr.io/lspecian/chronik-stream:v1.3.1

# From source
git pull
git checkout v1.3.1
cargo build --release
```

## Testing

This release has been tested to ensure:
- ✅ Service starts without crashing
- ✅ Accepts client connections properly
- ✅ Handles ApiVersions v3 requests correctly
- ✅ KSQL can connect and initialize

## Known Issues

While this fixes the critical crash, some Kafka compatibility issues remain:
- Producer/consumer operations may still have timeout issues
- Full KSQL operation compatibility is still being improved

## Apologies

We sincerely apologize for releasing v1.3.0 with such a critical bug. Our testing procedures have been updated to prevent similar issues in the future.

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.0...v1.3.1