# Chronik Stream Testing Standards

**Version**: 2.0.0
**Last Updated**: 2025-11-04
**Status**: MANDATORY for all test development

---

## Critical Truth: Chronik is a Rust Project

**CANONICAL TESTS**: Rust integration tests in `tests/integration/*.rs`
**EXPERIMENTAL DEBRIS**: 292 Python files from ad-hoc client testing
**ACTION REQUIRED**: Delete all Python test files

### Why Rust Tests Only?

1. **Chronik is written in Rust** - Tests should match the language
2. **Rust tests are compiled** - Type safety, no runtime errors
3. **Rust tests are integrated** - Share code with main project
4. **Python was for experimentation** - Quick client testing during development
5. **Python became technical debt** - 292 files of unmaintained debris

---

## Current State Assessment (2025-11-04)

**Canonical Tests**: 28 Rust integration tests (`tests/integration/*.rs`)
**Experimental Debris**: 292 Python files, debug directories, test output files

**IMMEDIATE ACTION**: Run cleanup script to remove Python debris

```bash
# Review what will be deleted
./tests/scripts/audit_tests.sh

# Dry run (see what would be deleted)
./tests/scripts/cleanup_tests.sh --dry-run

# Execute cleanup
./tests/scripts/cleanup_tests.sh --execute
```

---

## Testing Philosophy (from CLAUDE.md)

### Core Principles

1. **TEST FIRST, RELEASE SECOND** - ALWAYS test with real clients BEFORE committing/tagging
2. **NO DOCKER ON MACOS** - Native binary testing only (`cargo run --bin chronik-server`)
3. **TEST WITH ACTUAL CLIENT** - If Java reports bug, test with Java. If Python reports bug, test with Python client (NOT Python test file)
4. **END-TO-END VERIFICATION** - Fix not complete until verified: produce → crash → recover → consume
5. **CLEAN AS YOU GO** - Delete experimental/debug files immediately after resolution
6. **ONE CANONICAL TEST PER FEATURE** - No multiple partial implementations

### Work Ethic Standards

- ✅ **ABSOLUTE OWNERSHIP** - If you discover a problem, YOU FIX IT
- ✅ **PRODUCTION-READY CODE** - Every test should be maintainable
- ✅ **FIX FORWARD, NEVER REVERT** - Learn from failures
- ✅ **DO THINGS PROPERLY** - Properly is normal, it's how we do things

---

## Test Organization Structure

### Directory Layout (MANDATORY)

```
tests/
├── integration/             # Rust integration tests (CANONICAL)
│   ├── mod.rs              # Test module declaration
│   ├── common.rs           # Shared test utilities
│   ├── kafka_compatibility_test.rs
│   ├── wal_recovery_test.rs
│   ├── consumer_groups.rs
│   ├── admin_api_test.rs
│   └── *.rs               # More Rust tests
│
├── raft/                    # Raft-specific tests
│   └── *.rs
│
├── scripts/                 # Helper scripts ONLY
│   ├── audit_tests.sh      # Find test debris
│   ├── cleanup_tests.sh    # Remove debris
│   └── start_cluster.sh    # Cluster helpers
│
├── run_all_tests.sh         # Master test runner
└── TEST_INVENTORY.md        # Catalog of all tests
```

### What Gets DELETED Immediately

**ALL of these should be deleted:**
- ❌ **ALL Python files** (`.py`) - 292 files of experimental debris
- ❌ Debug directories (`debug/`, `librdkafka_debug/`)
- ❌ Test output directories in root (`test-*`)
- ❌ Test output files (`*.txt`, `consumer*.txt`, `c[0-9]*.txt`)
- ❌ Old compatibility test framework (`compat-tests/`)
- ❌ Python test directories (`python/`, `consumer-group/`, `compatibility/`)

**Use cleanup script:**
```bash
./tests/scripts/cleanup_tests.sh --execute
```

---

## Test Categories

### 1. Integration Tests (Rust) - CANONICAL

**Location**: `tests/integration/*.rs`
**Count**: 28 files
**Purpose**: Test complete functionality with Chronik server

**How to Run**:
```bash
# All integration tests
cargo test --test integration

# Specific test module
cargo test --test kafka_compatibility_test

# With output
cargo test --test wal_recovery_test -- --nocapture
```

**Example Tests**:
- `kafka_compatibility_test.rs` - Real Kafka protocol testing
- `wal_recovery_test.rs` - Crash recovery scenarios
- `consumer_groups.rs` - Consumer group coordination
- `admin_api_test.rs` - Admin API endpoints

### 2. Unit Tests (Rust)

**Location**: Inline in source files (`#[cfg(test)]` modules)
**Purpose**: Test individual functions and modules

**How to Run**:
```bash
cargo test --workspace --lib --bins
```

### 3. Client Compatibility Testing (Manual)

**Purpose**: Verify Chronik works with real Kafka clients
**Method**: Use actual Kafka clients, NOT Python test files

**Example - kafka-python**:
```bash
# Start Chronik
cargo run --bin chronik-server start

# Test with real kafka-python client
python3 -c "
from kafka import KafkaProducer, KafkaConsumer
producer = KafkaProducer(bootstrap_servers='localhost:9092')
producer.send('test', b'hello')
producer.flush()
print('✓ Produce succeeded')
"
```

**Example - Java (KSQLDB)**:
```bash
# Start Chronik
cargo run --bin chronik-server start

# Start KSQLDB (assumes installation in ksql/confluent-7.5.0/)
cd ksql/confluent-7.5.0/
bin/ksql http://localhost:8088

# Run SQL commands to test
CREATE STREAM test_stream (id INT, name VARCHAR) WITH (kafka_topic='test', value_format='JSON');
```

### 4. Cluster Tests (Rust)

**Location**: `tests/cluster/` (to be created), `tests/raft/*.rs`
**Purpose**: Multi-node Raft cluster functionality

**How to Run**:
```bash
cargo test --test raft_cluster_bootstrap
cargo test --features raft --test admin_api_test
```

---

## Test Naming Conventions

### Rust Test Files

**Format**: `<feature>_test.rs` or `<component>.rs`

**Examples**:
- ✅ `kafka_compatibility_test.rs` - Kafka protocol testing
- ✅ `wal_recovery_test.rs` - WAL recovery
- ✅ `consumer_groups.rs` - Consumer group tests
- ✅ `admin_api_test.rs` - Admin API tests

### Rust Test Functions

```rust
#[tokio::test]
async fn test_<action>_<expected_outcome>() {
    // Test that <action> results in <expected outcome>
}
```

**Examples**:
```rust
#[tokio::test]
async fn test_produce_and_fetch_basic_workflow() {
    // Test basic produce/consume workflow
}

#[tokio::test]
async fn test_wal_recovery_after_crash() {
    // Test WAL recovery restores messages after crash
}
```

---

## Writing New Tests

### Integration Test Template (Rust)

```rust
use chronik_server::IntegratedKafkaServer;
use chronik_common::metadata::InMemoryMetadataStore;
use std::sync::Arc;

#[tokio::test]
async fn test_feature_name() {
    // Arrange - Setup server and dependencies
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let server = IntegratedKafkaServer::new(
        "0.0.0.0:9092".to_string(),
        metadata_store.clone(),
        /* ... */
    ).await.unwrap();

    // Start server in background
    let server_handle = tokio::spawn(async move {
        server.run().await
    });

    // Act - Perform the operation being tested
    // (e.g., produce messages, crash server, restart, consume)

    // Assert - Verify expected outcomes
    assert_eq!(/* ... */);

    // Cleanup
    server_handle.abort();
}
```

### Regression Test Template (Rust)

```rust
/// Regression test for issue #<issue_number>: <Brief description>
///
/// Bug: <What went wrong>
/// Fix: <What was changed>
/// Version: v<version>
#[tokio::test]
async fn test_regression_issue_<issue_number>() {
    // Reproduce the exact scenario that triggered the bug
    // Verify it now works correctly
}
```

---

## Test Execution

### Quick Smoke Test

```bash
# Build server
cargo build --release --bin chronik-server

# Start server
cargo run --bin chronik-server start

# Run basic test (in another terminal)
cargo test --test kafka_compatibility_test::test_basic_produce_fetch
```

### Full Test Suite

```bash
# Master test runner (runs all tests)
./tests/run_all_tests.sh
```

### Specific Categories

```bash
# Unit tests only
cargo test --workspace --lib --bins

# Integration tests only
cargo test --test integration

# Cluster tests (require raft feature)
cargo test --features raft
```

---

## Cleanup and Migration Plan

### Phase 1: Delete Python Debris (IMMEDIATE)

**Action**: Remove all 292 Python test files

```bash
# Review what will be deleted
./tests/scripts/audit_tests.sh

# Execute cleanup
./tests/scripts/cleanup_tests.sh --execute
```

**What Gets Deleted**:
- ✅ All `.py` files (292 files)
- ✅ Debug directories (`debug/`, `librdkafka_debug/`)
- ✅ Test output directories (`test-*`)
- ✅ Old compatibility framework (`compat-tests/`)
- ✅ Python test directories (`python/`, `consumer-group/`, `compatibility/`)

**What Stays**:
- ✅ Rust integration tests (`tests/integration/*.rs`)
- ✅ Shell scripts for cluster management (`scripts/*.sh`)
- ✅ Documentation (`TEST_INVENTORY.md`, etc.)

### Phase 2: Verify Tests Still Pass (IMMEDIATE)

**Action**: Ensure canonical Rust tests still work

```bash
# Run all tests
cargo test --workspace --lib --bins
cargo test --test integration

# If any test fails, fix it before proceeding
```

### Phase 3: Commit Cleanup (IMMEDIATE)

```bash
# Stage deletions
git add -u

# Commit
git commit -m "test: Remove 292 Python test files - experimental debris

- Keep only canonical Rust integration tests (28 files)
- Delete Python files used for ad-hoc client testing
- Delete debug directories and test output
- Canonical tests: tests/integration/*.rs"

# Push
git push
```

---

## Client Compatibility Testing (The Right Way)

### Don't: Write Python Test Files

**BAD**:
```python
# tests/test_kafka_python.py - DON'T DO THIS
from kafka import KafkaProducer
# ... 50 lines of test code ...
```

**Why Bad**: Creates unmaintained Python debris

### Do: Use Real Clients Directly

**GOOD**:
```bash
# Start Chronik
cargo run --bin chronik-server start

# Test with kafka-python (one-liner or interactive)
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='localhost:9092')
producer.send('test', b'hello')
producer.flush()
print('✓ Works')
"
```

**Why Good**: No test file to maintain, quick verification

### Do: Document Results in Issue/PR

```markdown
## Tested With

- ✅ kafka-python 2.0.2 - Basic produce/consume works
- ✅ confluent-kafka 1.9.0 - Consumer groups work
- ✅ KSQLDB 7.5.0 - Stream creation works
- ❌ Java kafka-clients 3.6.0 - CRC validation fails (issue #789)
```

---

## Before Every Release Checklist

- [ ] ✅ Run full test suite: `./tests/run_all_tests.sh`
- [ ] ✅ Test with kafka-python client (manual)
- [ ] ✅ Test with confluent-kafka client (manual)
- [ ] ✅ Test with Java clients if Java features changed (KSQLDB)
- [ ] ✅ Test cluster functionality if cluster changed
- [ ] ✅ Verify NO Python test files exist (`find tests -name '*.py'` should be empty)
- [ ] ✅ Document tested clients in release notes

---

## Maintenance

### Weekly Tasks

- [ ] Run `./tests/scripts/audit_tests.sh` to check for new debris
- [ ] Delete any new experimental files immediately
- [ ] Update `tests/TEST_INVENTORY.md` if tests added/removed

### Monthly Tasks

- [ ] Run full test suite on clean environment
- [ ] Review test execution times and optimize slow tests
- [ ] Update documentation if test structure changed

---

## Summary

**GOAL**: Clean, professional test suite with ONLY Rust tests

**CURRENT STATE**: 28 Rust tests + 292 Python debris files
**TARGET STATE**: 28 Rust tests + 0 Python files

**IMMEDIATE ACTION**: Run cleanup script
```bash
./tests/scripts/cleanup_tests.sh --execute
```

**GOING FORWARD**:
- ✅ Write Rust integration tests only
- ✅ Test with real clients directly (no Python test files)
- ✅ Delete experimental files immediately
- ✅ Keep test suite clean and maintainable
