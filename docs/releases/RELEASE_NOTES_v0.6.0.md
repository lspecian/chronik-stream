# Release Notes - v0.6.0

## Release Date: September 5, 2025

## Overview

This release brings **critical librdkafka v2.11.1 compatibility fixes** to chronik-stream, resolving "Bad message format" errors that prevented modern Kafka clients from connecting. The release also includes significant project organization improvements and comprehensive test utilities.

## Critical Fixes

### librdkafka v2.11.1 Compatibility

**Issue**: librdkafka v2.11.1 clients were receiving "Bad message format" errors when connecting to chronik-stream.

**Root Cause**: Two protocol compatibility issues:
1. Missing v8+ fields in Produce v9 responses
2. Incorrect parsing of client_id field in flexible protocol versions

**Solution**:
- Added missing `record_errors` and `error_message` fields to Produce v9 responses
- Fixed client_id parsing to use compact strings for flexible protocol versions (v3+)
- Verified compatibility with confluent-kafka-go/v2 using librdkafka v2.11.1

**Impact**: Full compatibility with modern Kafka clients including librdkafka v2.11.1+

## Features & Improvements

### Protocol Enhancements
- Proper compact string encoding/decoding for flexible protocol versions
- Complete Produce v9 response structure compliance
- Improved request header parsing with better error handling

### Testing Infrastructure
- Comprehensive librdkafka compatibility test suite
- Protocol analysis tools for debugging wire format issues
- TCP intercept proxy for real-time protocol debugging
- Go integration tests using confluent-kafka-go/v2

### Documentation
- Added comprehensive librdkafka compatibility documentation
- Detailed protocol debugging guides
- Test utility documentation with usage examples

### Project Organization
- Reorganized test files into structured directories
- Moved investigation documentation to docs folder
- Created clear separation between protocol tests, debug utilities, and integration tests

## Breaking Changes

None - This release maintains full backward compatibility.

## Migration Guide

No migration required. Simply update to v0.6.0 to get librdkafka v2.11.1 compatibility.

## Testing

This release has been tested with:
- librdkafka v2.11.1 (via confluent-kafka-go/v2)
- Apache Kafka 3.x clients
- Raw protocol tests for all supported versions
- Comprehensive integration test suite

## Docker Images

Docker images are available at:
```
docker pull chronikstream/chronik-server:0.6.0
docker pull chronikstream/chronik-server:latest
```

## Known Issues

None at this time.

## Contributors

Thanks to all contributors who helped identify and fix the librdkafka compatibility issues.

## What's Next

- Enhanced protocol version negotiation
- Performance optimizations for high-throughput scenarios
- Additional client library compatibility testing

## Technical Details

### Files Changed

**Core Fixes:**
- `crates/chronik-protocol/src/parser.rs` - Fixed client_id parsing for flexible versions
- `crates/chronik-protocol/src/handler.rs` - Added missing v8+ fields to Produce responses
- `crates/chronik-server/src/integrated_server.rs` - Improved TCP transmission reliability

**Test Utilities:**
- `tests/python/protocol/librdkafka/` - Protocol analysis tools
- `tests/python/debug/librdkafka/` - Debug utilities
- `tests/go/librdkafka/` - Go integration tests

**Documentation:**
- `docs/LIBRDKAFKA_COMPATIBILITY.md` - Complete compatibility guide
- Test directory README files

### Commit History Since v0.5.0

- fix: Critical librdkafka v2.11.1 compatibility fixes for ApiVersions v3
- test: Extensive librdkafka compatibility investigation
- fix: Handle empty/short record batches and add debug logging
- fix: Critical correlation ID preservation and null records handling
- docs: Clarify Docker image tags
- fix: Resolve axum dependency issue for non-search builds
- fix: Remove search-server binary and fix compact bytes methods

## Installation

### Binary Installation
```bash
# Download the latest release
curl -L https://github.com/chronik-stream/chronik-stream/releases/download/v0.6.0/chronik-server-linux-amd64 -o chronik-server
chmod +x chronik-server
./chronik-server
```

### Docker Installation
```bash
docker run -p 9092:9092 chronikstream/chronik-server:0.6.0
```

### From Source
```bash
git clone https://github.com/chronik-stream/chronik-stream.git
cd chronik-stream
git checkout v0.6.0
cargo build --release
./target/release/chronik-server
```

## Verification

To verify the librdkafka compatibility:

```go
// test_librdkafka.go
package main

import (
    "fmt"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
    p, err := kafka.NewProducer(&kafka.ConfigMap{"bootstrap.servers": "localhost:9092"})
    if err != nil {
        panic(err)
    }
    defer p.Close()
    
    topic := "test"
    p.Produce(&kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
        Value:          []byte("Hello from librdkafka v2.11.1"),
    }, nil)
    
    p.Flush(5000)
    fmt.Println("✅ Message sent successfully!")
}
```

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v0.5.0...v0.6.0