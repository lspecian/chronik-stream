# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2025-09-09

### Added
- **CRITICAL**: Advertised address configuration support
  - New CLI arguments: `--advertised-addr` and `--advertised-port`
  - New environment variables: `CHRONIK_ADVERTISED_ADDR` and `CHRONIK_ADVERTISED_PORT`
  - Separate bind address from advertised address for proper client connectivity
- Warning when advertised address is set to `0.0.0.0`
- Comprehensive test script for verifying client connectivity
- Detailed documentation for advertised address configuration

### Fixed
- **CRITICAL**: Kafka clients can now connect when server binds to `0.0.0.0`
  - Metadata responses now return configured advertised address instead of bind address
  - Resolves connectivity issues with all Kafka clients (Python, Go, Java, KSQLDB, Kafka UI)
  - Fixes Docker deployment connectivity problems
  - Enables proper Kubernetes deployments

### Changed
- Updated README with advertised address configuration examples
- Updated docker-compose.yml with advertised address environment variable
- Broker registration now uses advertised address in metadata store

### Documentation
- Added `docs/ADVERTISED_ADDRESS_FIX.md` with comprehensive fix documentation
- Created `test_advertised_address.py` for testing client connectivity
- Updated configuration examples across all documentation

## [0.6.1] - 2025-09-06

### Fixed
- **CRITICAL**: Implemented workaround for librdkafka v2.11.1 encoding bug
  - librdkafka incorrectly sends client_id string after null marker in Metadata v12 requests
  - Added detection and skip logic to handle malformed client_id encoding
  - Fixes "Protocol read buffer underflow" errors with Go/librdkafka clients
  - Maintains backward compatibility with correctly-formatted Python clients

## [0.6.0] - 2025-09-05

### Fixed
- **CRITICAL**: Fixed librdkafka v2.11.1 compatibility issues
  - Added missing `record_errors` and `error_message` fields to Produce v9 responses
  - Fixed client_id parsing to use compact strings for flexible protocol versions (v3+)
  - Resolved "Bad message format" errors for modern Kafka clients
- Improved TCP transmission reliability in integrated server
- Enhanced request header parsing with better error handling

### Added
- Comprehensive librdkafka compatibility test suite
- Protocol analysis tools for debugging wire format issues
- TCP intercept proxy for real-time protocol debugging
- Go integration tests using confluent-kafka-go/v2
- Detailed librdkafka compatibility documentation
- Test utility documentation with usage examples

### Changed
- Reorganized test files into structured directories
- Moved investigation documentation to docs folder
- Created clear separation between protocol tests, debug utilities, and integration tests

## [0.5.0] - 2024-XX-XX

### Added
- Initial release of Chronik Stream
- Kafka wire protocol v2 compatibility
- Distributed storage with TiKV backend
- Built-in full-text search capabilities
- REST Admin API for management
- Prometheus metrics support
- Docker and Docker Compose support
- Kubernetes deployment manifests
- Terraform configurations for Hetzner and AWS
- Comprehensive test suite
- CI/CD pipeline with GitHub Actions

### Fixed
- Fixed produce request panic in kafka_records.rs
- Fixed correlation ID handling in protocol implementation

### Security
- Added TLS/SSL support for all connections
- Implemented authentication middleware
- Added rate limiting for API endpoints

## [0.1.0] - 2024-01-XX (Upcoming)

### Added
- First public release
- Core Kafka protocol implementation
- Basic topic and partition management
- Consumer group coordination
- Message production and consumption
- Search indexing for messages
- Admin REST API
- Monitoring and metrics
- Documentation and examples

### Known Issues
- Fetch handler not fully implemented
- Message persistence layer needs optimization
- Some Kafka client compatibility issues remain
- Consumer group offset management incomplete

[Unreleased]: https://github.com/lspecian/chronik-stream/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/lspecian/chronik-stream/releases/tag/v0.1.0