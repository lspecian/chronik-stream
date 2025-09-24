# Chronik Stream v1.3.0 Release Notes

## Release Date: September 23, 2025

## Major Feature: Full KSQL Compatibility

This release introduces complete compatibility with KSQL (Confluent's SQL engine for Apache Kafka), making Chronik Stream a drop-in replacement for Kafka when using KSQL for stream processing.

## Key Features

### 🎯 KSQL Integration
- **Full KSQL Support**: Chronik Stream now works seamlessly with KSQL without requiring any modifications or workarounds
- **ListOffsets v7 Fix**: Fixed critical parsing issue with flexible protocol headers that prevented KSQL from starting
- **SASL Authentication**: Added support for SaslAuthenticate API to maintain stable AdminClient connections
- **IncrementalAlterConfigs**: Implemented API for dynamic configuration updates

### 🔧 Protocol Improvements
- Fixed tagged field parsing in flexible protocol headers
- Enhanced metadata management with WAL-backed storage
- Improved consumer group coordination
- Added proper error handling for unsupported SASL mechanisms

### 📚 Documentation
- Comprehensive KSQL integration guide
- Detailed compatibility progress tracking
- Step-by-step setup instructions for KSQL with Chronik Stream

### 🧹 Project Organization
- Cleaned up test artifacts and deprecated files
- Improved project structure
- Updated .gitignore for better organization

## Breaking Changes
None - This release maintains backward compatibility with existing Chronik Stream deployments.

## Bug Fixes
- Fixed "Failed to get topic offsets" error preventing KSQL startup
- Resolved AdminClient disconnection issues
- Corrected flexible protocol header parsing for Kafka API v7+

## Installation

```bash
# Using cargo
cargo install chronik-stream --version 1.3.0

# From source
git clone https://github.com/chronik-stream/chronik-stream
cd chronik-stream
git checkout v1.3.0
cargo build --release
```

## KSQL Quick Start

1. Start Chronik Stream:
```bash
chronik-server
```

2. Configure KSQL to connect to Chronik:
```properties
# ksql-server.properties
bootstrap.servers=localhost:9092
ksql.service.id=ksql_service_1
ksql.queries.file=/path/to/queries.sql
```

3. Start KSQL Server:
```bash
ksql-server-start ksql-server.properties
```

## Performance
- No performance regressions
- Improved metadata operation efficiency
- Optimized protocol parsing for flexible headers

## Compatibility
- KSQL: 7.5.0+ (tested)
- Kafka Protocol: Full support for flexible protocol versions
- Operating Systems: Linux, macOS, Windows
- Rust: 1.70+

## Contributors
Thanks to all contributors who made this release possible!

## What's Next
- Enhanced KSQL query optimization
- Additional Kafka Streams support
- Performance improvements for large-scale deployments

## Support
For issues or questions:
- GitHub Issues: https://github.com/chronik-stream/chronik-stream/issues
- Documentation: https://chronik-stream.github.io/docs

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.2.6...v1.3.0