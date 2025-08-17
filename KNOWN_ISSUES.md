# Known Issues in Chronik Stream

## Critical Bugs

### 1. ~~Produce Request Panic (kafka_records.rs:413)~~ [FIXED]
**Severity**: HIGH
**Impact**: All produce requests fail with a panic
**Location**: `crates/chronik-storage/src/kafka_records.rs:413`
**Error**: `thread 'tokio-runtime-worker' panicked at crates/chronik-storage/src/kafka_records.rs:413:31: capacity overflow`

**Root Cause**: The batch length validation was incorrectly checking the cursor position plus batch length against total data length, but the batch length field represents the size of data AFTER the length field itself.

**Fix Applied**: 
- Changed the validation to correctly calculate expected total size as: `12 + batch_length` (8 bytes offset + 4 bytes length + batch_length)
- This ensures we have enough data to parse the complete record batch
- Added better error messages with expected vs actual sizes

### 2. Correlation ID Mismatch in Protocol Tests
**Severity**: MEDIUM
**Impact**: Cross-client compatibility tests failing
**Location**: `crates/chronik-protocol/tests/cross_client_test.rs`
**Failed Tests**:
- test_confluent_kafka_go_compatibility
- test_java_client_compatibility  
- test_kafkactl_patterns
- test_librdkafka_compatibility
- test_node_rdkafka_compatibility
- test_response_parseability

**Root Cause**: Correlation IDs in responses don't match what's expected

## Missing Implementations

### 3. Message Persistence
**Severity**: HIGH
**Impact**: Messages are accepted but not actually persisted
**Status**: Produce handler accepts messages but actual storage is not implemented

### 4. Fetch Handler Implementation
**Severity**: HIGH
**Impact**: Consumers cannot retrieve messages
**Location**: `crates/chronik-ingest/src/fetch_handler.rs`
**Status**: Skeleton exists but not fully implemented

### 5. Consumer Group Management
**Severity**: MEDIUM
**Impact**: Consumer groups cannot track offsets
**Status**: Basic structure exists but no offset tracking

## Protocol Compatibility Issues

### 6. Record Batch Format Version
**Issue**: The code assumes Kafka record batch format v2 but may need to handle v0/v1 for older clients
**Impact**: Older Kafka clients may not work

### 7. API Version Negotiation
**Issue**: Not all API versions are properly negotiated
**Impact**: Some clients may receive unexpected response formats

## Infrastructure Issues

### 8. Admin API Authentication
**Issue**: Admin API requires authentication but no clear documentation on how to authenticate
**Impact**: Cannot use chronik-ctl tool effectively

### 9. Topic Auto-Creation
**Issue**: Topics are not automatically created when producing to non-existent topics
**Impact**: Standard Kafka behavior not replicated

## Next Steps

1. Fix the produce panic by debugging the varint decoding
2. Implement proper correlation ID handling
3. Complete message persistence layer
4. Implement fetch handler for message retrieval
5. Add consumer group offset management
6. Document authentication setup for admin API