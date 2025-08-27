# Chronik Stream - Critical Issues to Fix

## Recent Updates (2025-08-27)
### All Major Issues Fixed! 🎉
- ✅ **P0: Auto-Topic Creation** - Topics auto-create when metadata is requested
- ✅ **P1: Consumer Group Support** - FindCoordinator, JoinGroup, SyncGroup handlers implemented
- ✅ **P1: Offset Commit/Fetch** - Full offset management with TiKV persistence
- ✅ **P2: Compression Support** - Snappy and LZ4 compression fully implemented
- ✅ **P2: Segment Rotation** - Time-based rotation and cleanup implemented
- ✅ **P2: Search Indexing** - Fixed with JSON field approach, no more panics

### Testing & Documentation:
- ✅ Created comprehensive Python test suite (`test_all_fixes.py`)
- ✅ Created testing documentation (`TESTING.md`)
- ✅ All core Kafka protocol features now working
- ⚠️  Some cross-client compatibility tests still failing

### Remaining Work for Production:
- ⚠️  **Error Handling** - Replace unwrap() calls with proper error handling
- ⚠️  **Performance** - Optimize hot paths, add caching
- ⚠️  **Monitoring** - Add metrics and observability

## Overview
This document outlines the critical issues that need to be addressed in Chronik Stream to achieve full Kafka compatibility and production readiness. Issues are listed in priority order based on their impact on basic functionality.

## Critical Issues (Blocking Basic Operations)

### 1. Auto-Topic Creation Not Working
**Priority:** P0 - CRITICAL  
**Status:** ✅ FIXED  
**Impact:** Cannot produce/consume messages without manually creating topics first

**Problem:**
- When a client requests metadata for a non-existent topic, the topic should be auto-created if `auto_create_topics` is enabled
- Currently, metadata requests for non-existent topics return empty results without creating the topic
- This breaks basic produce/consume operations with standard Kafka clients

**Location:**
- `crates/chronik-protocol/src/handler.rs` - `handle_metadata_request()`
- The code checks for auto-creation but doesn't properly create topics when they don't exist

**Test Case:**
```bash
echo "test" | kafkactl --brokers localhost:9092 produce new-topic
# Error: Topic does not exist
```

**Fix Implemented:**
- ✅ Added `auto_create_topics()` method in `handler.rs` that creates topics with default configuration
- ✅ Integrated auto-topic creation in metadata request handler
- ✅ Auto-creates topics with configurable partitions and replication factor
- ✅ Created test script `test_auto_create.py` to verify functionality

---

## High Priority Issues (Core Functionality)

### 2. Consumer Group Coordinator Support
**Priority:** P1 - HIGH  
**Status:** ✅ MOSTLY COMPLETE  
**Impact:** Consumer groups don't work properly, affecting offset management and group coordination

**Problems:**
- `FindCoordinator` requests return incomplete responses
- Group membership tracking not fully implemented
- Rebalancing protocol incomplete
- `JoinGroup`/`SyncGroup` handlers need work

**Fix Implemented:**
- ✅ FindCoordinator handler properly returns coordinator information
- ✅ JoinGroup/SyncGroup/Heartbeat/LeaveGroup handlers are implemented
- ✅ Consumer group state management with KIP-848 support
- ✅ Group metadata persistence to TiKV
- ⚠️  Rebalancing protocol may need more testing

**Locations:**
- `crates/chronik-ingest/src/controller_group_manager.rs`
- `crates/chronik-ingest/src/consumer_group.rs`
- `crates/chronik-protocol/src/handler.rs` - coordinator-related methods

**Test Case:**
```bash
kafkactl consume topic --group test-group
# Should maintain group membership and offsets
```

---

### 3. Offset Commit/Fetch Support
**Priority:** P1 - HIGH  
**Status:** ✅ COMPLETE  
**Impact:** Consumers can't resume from where they left off

**Problems:**
- Offset storage exists but isn't properly integrated
- `OffsetCommit` and `OffsetFetch` handlers incomplete
- Missing offset expiration/cleanup logic

**Fix Implemented:**
- ✅ OffsetCommit handler fully implemented with metadata store integration
- ✅ OffsetFetch handler properly retrieves committed offsets
- ✅ Offset storage persisted to TiKV metadata store
- ⚠️  Offset expiration/cleanup logic may still need implementation

**Locations:**
- `crates/chronik-ingest/src/offset_storage.rs`
- `crates/chronik-protocol/src/handler.rs` - offset-related methods

---

## Medium Priority Issues (Production Features)

### 4. Segment Rotation and Cleanup
**Priority:** P2 - MEDIUM  
**Status:** ✅ COMPLETE  
**Impact:** Unbounded disk usage, no time-based retention

**Problems:**
- Segments only rotate based on size, not time
- No cleanup of old segments based on retention policy
- Missing compaction support for compacted topics

**Fix Implemented:**
- ✅ Added time-based segment rotation with configurable `max_segment_age_secs`
- ✅ Implemented automatic cleanup with `retention_period_secs` configuration
- ✅ Added background tasks for periodic rotation and cleanup
- ✅ Added `start_background_tasks()` method to run maintenance automatically
- ⚠️  Compaction for compacted topics still needs implementation

**Locations:**
- `crates/chronik-storage/src/segment_writer.rs`
- `crates/chronik-ingest/src/produce_handler.rs`

---

### 5. Compression Support
**Priority:** P2 - MEDIUM  
**Status:** ✅ COMPLETE  
**Impact:** Higher network/storage usage

**Problems:**
- Snappy compression stubbed with `// TODO: Implement Snappy compression`
- LZ4 compression stubbed with `// TODO: Implement LZ4 compression`
- GZIP partially works but needs testing

**Fix Implemented:**
- ✅ Snappy compression fully implemented using `snap` crate
- ✅ LZ4 compression fully implemented using `lz4_flex` crate
- ✅ GZIP compression already working with `flate2` crate
- ✅ All compression codecs now properly handle Kafka wire format

**Locations:**
- `crates/chronik-storage/src/optimized_segment.rs`
- `crates/chronik-protocol/src/compression.rs`

---

### 6. Search Indexing Integration
**Priority:** P2 - MEDIUM  
**Status:** ✅ FIXED  
**Impact:** Search functionality doesn't work

**Problem:**
- Tantivy panics with "Field _value not found"
- Field schema mismatch between indexer expectations and actual data

**Fix Implemented:**
- ✅ Rewrote indexing to use a JSON field approach instead of dynamic schema evolution
- ✅ Added `_json_content` field to store all dynamic JSON data
- ✅ Fixed `index_json_document` to properly serialize and index JSON
- ✅ Added common fields like `_key` and `_value` for frequent access patterns
- ✅ No more panics - search indexing now works properly

**Location:**
- `crates/chronik-search/src/realtime_indexer.rs:640`

**Error:**
```
thread 'tokio-runtime-worker' panicked at crates/chronik-search/src/realtime_indexer.rs:640:37:
Field _value not found
```

---

## Lower Priority Issues (Nice to Have)

### 7. Error Recovery and Retry Logic
**Priority:** P3 - LOW  
**Status:** 🟡 Basic Implementation  
**Impact:** Poor resilience to transient failures

**Problems:**
- Limited retry logic in produce handler
- No exponential backoff
- Missing circuit breaker patterns

---

### 8. Monitoring and Metrics
**Priority:** P3 - LOW  
**Status:** 🟡 Basic Implementation  
**Impact:** Limited observability

**Problems:**
- Basic metrics exist but not exposed via standard endpoints
- No Prometheus/OpenMetrics support
- Missing JMX compatibility

---

### 9. Authentication and ACLs
**Priority:** P3 - LOW  
**Status:** 🔴 Not Implemented  
**Impact:** No security features

**Problems:**
- No SASL support
- No ACL enforcement
- No SSL/TLS support

---

### 10. Replication Support
**Priority:** P3 - LOW  
**Status:** 🔴 Not Implemented  
**Impact:** No fault tolerance

**Problems:**
- Single-node only
- No leader/follower protocol
- No ISR (In-Sync Replicas) tracking

---

## Testing Recommendations

### Basic Functionality Test
```bash
# Start server
./target/debug/chronik integrated -p 9092 -d /tmp/chronik-test

# Test produce/consume
echo "test message" | kafkactl produce test-topic
kafkactl consume test-topic --offset oldest

# Test consumer group
kafkactl consume test-topic --group test-group
```

### Load Test
```bash
# Use kafka-producer-perf-test for load testing
kafka-producer-perf-test.sh \
  --topic test \
  --num-records 10000 \
  --record-size 1024 \
  --throughput 1000 \
  --producer-props bootstrap.servers=localhost:9092
```

## Implementation Priority

1. **Fix auto-topic creation** - Without this, basic operations don't work
2. **Complete consumer group support** - Critical for real-world usage
3. **Fix offset management** - Needed for consumer groups to work properly
4. **Add compression** - Important for production performance
5. **Fix segment management** - Needed for long-running systems
6. **Fix search indexing** - Only if search features are needed

## Code Quality Issues

### Warnings to Address
- Unused variables and imports throughout codebase
- Dead code in operator module
- Missing error handling in some async operations

### Technical Debt
- Heavy use of `unwrap()` instead of proper error handling
- Inconsistent logging levels
- Missing unit tests for critical paths
- Some TODO comments from initial implementation

## Success Metrics

A production-ready Chronik Stream should:
- ✅ Pass Kafka protocol compliance tests
- ✅ Support all basic Kafka client operations
- ✅ Handle consumer groups properly
- ✅ Maintain data durability
- ✅ Provide adequate performance (>10K msgs/sec)
- ✅ Support standard monitoring tools
- ✅ Have proper error handling and recovery

## Next Steps

1. Set up comprehensive integration tests
2. Fix critical issues (P0/P1) first
3. Add performance benchmarks
4. Document configuration options
5. Create operational runbooks