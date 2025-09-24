# Chronik Stream v1.3.4 - Protocol Detection Fix

## Release Date: September 24, 2025

## 🎯 The Fix

**SOLVED**: "Cannot advance 46 bytes, only 4 remaining" error

The root cause was Kafka UI sending "5.0\0" (a version string) instead of proper Kafka protocol bytes. This was being misinterpreted as a request size, causing the parser to try to read 875835392 bytes!

## What Changed

### Protocol Detection Layer
Added pre-parsing protocol detection that identifies and rejects:
- Version strings like "5.0\0" from Kafka UI
- HTTP requests (GET, POST, etc.)
- TLS handshakes on non-TLS ports
- Other non-Kafka protocol data

When non-Kafka data is detected:
1. Logs a clear warning with the actual bytes received
2. Sends an HTTP 400 response explaining the issue
3. Gracefully closes the connection
4. **Does not crash or block other connections**

## The Technical Details

When Kafka UI sends "5.0\0" (bytes: `[0x35, 0x2e, 0x30, 0x00]`):
- Old behavior: Interpreted as size = 875835392, tried to read that many bytes, failed
- New behavior: Recognized as version string, rejected with helpful error message

## How to Test

1. Start Chronik Stream v1.3.4:
```bash
./chronik-server -p 9092
```

2. Connect Kafka UI - you'll see:
```
WARN Non-Kafka protocol detected from 127.0.0.1:xxxxx: '5.0' (bytes: [35, 2e, 30, 00])
```

3. **But the server keeps running!** Other Kafka clients can still connect normally.

## What This Means

- ✅ Server no longer crashes from protocol mismatches
- ✅ Clear error messages identify the problem
- ✅ Other clients aren't affected by bad connections
- ✅ Kafka UI gets a helpful HTTP response explaining the issue

## Next Steps

If Kafka UI still can't connect after this fix, it means:
1. Kafka UI is not sending proper Kafka protocol at all
2. May need to check Kafka UI's connection settings
3. Possibly Kafka UI expects a different protocol version negotiation

But at least now **your server won't crash** and you'll know exactly what data is being sent!

## For Developers

The fix is in `/crates/chronik-server/src/integrated_server.rs`:
- Added protocol detection before size parsing
- Specifically handles "5.0\0" pattern
- Generic detection for HTTP and TLS
- Returns HTTP 400 with explanation for non-Kafka protocols

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.3...v1.3.4