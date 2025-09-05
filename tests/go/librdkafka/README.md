# librdkafka Go Integration Tests

This directory contains Go-based integration tests using the librdkafka client library through confluent-kafka-go.

## Test Files

### test_librdkafka_produce.go
Main librdkafka produce test that verifies message production through the fixed server implementation.

### test_librdkafka_debug.go
Debug version with verbose output for troubleshooting librdkafka connectivity issues.

### test_librdkafka_full.go
Comprehensive librdkafka test suite covering multiple operations.

### test_produce_with_topic.go
Topic-specific produce tests for validating topic creation and message routing.

### test_simple_produce.go
Minimal produce test for basic functionality verification.

### test_produce.go
Standard produce test implementation.

### test_with_admin.go
Tests using Kafka admin client for cluster management operations.

### test_no_version_request.go
Tests behavior when ApiVersions request is skipped.

### test_librdkafka_*.go
Various numbered test files for specific port configurations (9092, 9093, 9095, 9096).

## Running Tests

Ensure chronik-server is running on port 9092, then:

```bash
# Run the main produce test
go run test_librdkafka_produce.go

# Run with debug output
go run test_librdkafka_debug.go

# Run comprehensive test suite
go run test_librdkafka_full.go
```

## Dependencies

These tests require:
- Go 1.19+
- github.com/confluentinc/confluent-kafka-go/v2
- A running chronik-server instance on localhost:9092

## Key Findings

These tests helped identify that librdkafka v2.11.1 sends compact strings for the client_id field in flexible protocol versions, which was the root cause of the compatibility issue.