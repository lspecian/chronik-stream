# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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