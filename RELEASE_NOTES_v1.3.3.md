# Chronik Stream v1.3.3 - Diagnostic Release

## Release Date: September 24, 2025

## 🔍 Purpose

This is a diagnostic release focused on identifying and fixing the protocol parsing error that's blocking all client connections.

## The Critical Issue

**Error**: "Cannot advance 46 bytes, only 4 remaining"
- Occurs every 30 seconds from Kafka UI
- Blocks all client connections
- Indicates protocol version mismatch or incorrect frame parsing

## What's New

### Enhanced Diagnostics

1. **Buffer Content Logging**
   - Shows actual bytes when advance() fails
   - Displays first 100 bytes of buffer on error
   - Helps identify protocol mismatches

2. **Varint Parsing Diagnostics**
   - Logs suspicious varint values (>10000)
   - Shows byte sequence that produced the value
   - Helps identify when we're misreading data

3. **Request Entry Logging**
   - Logs first 64 bytes of every request
   - Shows total request size
   - Helps identify request format issues

4. **Tagged Field Diagnostics**
   - Logs each tagged field's tag, size, and remaining buffer
   - Shows buffer contents when size is unreasonable
   - Helps identify flexible vs fixed format confusion

## How This Helps

With these diagnostics, when the error occurs we'll see:
1. The exact request bytes that trigger the error
2. What we interpreted as field sizes
3. Where in the parsing we failed
4. The actual buffer state at failure

## Next Steps

1. Deploy this version
2. Collect diagnostic logs when the error occurs
3. Use the logs to identify the exact parsing issue
4. Release v1.3.4 with the actual fix

## Running with Debug Logging

To get maximum diagnostic information:

```bash
RUST_LOG=debug,chronik=trace ./chronik-server
```

## For Testers

Please run this version and share the logs when you see the "Cannot advance 46 bytes" error. The logs will contain:
- The request bytes that triggered the error
- What we tried to parse
- Where exactly we failed

This information will allow us to fix the root cause in v1.3.4.

## Technical Details

The error suggests we're either:
1. Trying to parse a flexible format request as fixed format
2. Trying to parse a fixed format request as flexible format
3. Misreading a varint that indicates field size

The enhanced logging will tell us exactly which case it is.

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.2...v1.3.3