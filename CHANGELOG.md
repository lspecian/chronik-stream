# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.2] - 2025-09-14

### Fixed
- **CRITICAL**: Fixed "Unknown Group" errors in Kafka Consumer Group Coordination
  - Consumer groups are now automatically created when clients attempt to join non-existent groups
  - Enhanced JoinGroup and SyncGroup protocol handlers for proper consumer group coordination
  - Matches standard Kafka broker behavior for consumer group lifecycle management
- Improved protocol conversion between internal consumer group format and Kafka wire protocol
- Enhanced consumer group state transitions and member management
- Better error handling for consumer group edge cases

### Added
- Automatic consumer group creation during coordination process
- Enhanced protocol compliance for JoinGroup and SyncGroup operations
- Comprehensive test suite for consumer group functionality in `tests/consumer-group/`
- Improved logging and debugging for consumer group operations

### Changed
- Consumer group coordination now follows standard Kafka broker patterns
- Enhanced client compatibility with kafka-python and other standard Kafka clients
- Improved consumer group state management and assignment distribution

### Compatibility
- Full backward compatibility with v1.0.1
- Drop-in replacement with no breaking changes
- Enhanced compatibility with all major Kafka client libraries

## [0.7.2] - 2025-09-09

### Fixed
- **CRITICAL**: Fixed port duplication bug when `CHRONIK_ADVERTISED_ADDR` includes port
  - v0.7.1 incorrectly appended port to addresses like `localhost:9092` resulting in `localhost:9092:9092`
  - Now correctly parses `host:port` format and handles port separately
  - Supports IPv6 addresses with proper bracket notation
- All advertised address formats now work correctly:
  - `localhost` → advertises as `localhost:9092`
  - `localhost:9092` → advertises as `localhost:9092` (not duplicated)
  - `[::1]:9092` → advertises as `[::1]:9092`

### Verified
- Comprehensive testing with 6 different address formats
- Kafka admin clients can now connect successfully
- Backwards compatible with all v0.7.x configurations

## [0.7.1] - 2025-09-09

### Added
- **Smart advertised address defaults** - automatically detect and use hostname when binding to `0.0.0.0`
  - Uses `HOSTNAME` environment variable (set by Docker) when available
  - Falls back to `localhost` with clear warnings when no hostname is detected
  - Prevents silent failures from advertising `0.0.0.0` to clients
- Enhanced bind address parsing to handle `host:port` format correctly
- Improved logging to guide users on advertised address configuration

### Fixed
- Handle `CHRONIK_BIND_ADDR` with port specification (e.g., `0.0.0.0:9092`)
- All server modes now properly handle advertised address configuration

### Changed
- **BREAKING**: Updated README to emphasize `CHRONIK_ADVERTISED_ADDR` is required for Docker deployments
- Added critical Docker configuration section to README
- Improved error messages when advertised address is misconfigured

### Documentation
- Added prominent Docker configuration warning in README
- Updated all Docker examples to include `CHRONIK_ADVERTISED_ADDR`
- Clarified that `CHRONIK_BIND_ADDR` should be host-only without port

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