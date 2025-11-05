# Chronik Stream Test Suite Status

**Last Updated**: 2025-11-04
**Total Test Files**: 292 Python, 26 Shell, 34 Rust

---

## 🚨 CRITICAL: Test Suite Needs Cleanup

The test suite has grown organically to **292 Python test files** with significant duplication and experimental debris. This document tracks:

1. **What tests we have** (canonical tests to keep)
2. **What works** (passing tests)
3. **What's broken** (failing tests / known issues)
4. **What to delete** (debug scripts, duplicates)

See [docs/TESTING_STANDARDS.md](../docs/TESTING_STANDARDS.md) for cleanup plan and standards.

---

## Test Categories

### ✅ Unit Tests (Rust)

**Status**: WORKING
**Location**: `tests/integration/*.rs` (actually unit tests in disguise)
**Run**: `cargo test --workspace --lib --bins`

**Coverage**:
- ✅ WAL recovery
- ✅ Protocol encoding/decoding
- ✅ Consumer group coordination
- ✅ Metadata operations
- ✅ Storage layer

**Issues**: None known

---

### ✅ Integration Tests (Rust)

**Status**: PARTIAL
**Location**: `tests/integration/*.rs`
**Run**: `cargo test --test integration`

**Canonical Tests**:
- ✅ `kafka_compatibility_test.rs` - Real Kafka client testing
- ✅ `wal_recovery_test.rs` - Crash recovery scenarios
- ✅ `consumer_groups.rs` - Consumer group functionality
- ✅ `admin_api_test.rs` - Admin API endpoints

**Issues**:
- Some tests may require Docker (conflicts with macOS standards)
- Need audit to identify which tests are actually maintained

---

### ⚠️ Integration Tests (Python)

**Status**: CHAOS - NEEDS CLEANUP
**Location**: `tests/integration/*.py`, `tests/python/integration/*.py`
**Count**: ~50+ files

**Canonical Tests** (keep these):
- ✅ `test_basic_functionality.py` - Protocol smoke tests
- ✅ `test_kafka_compatibility.py` - Kafka client compatibility
- ⚠️ Others need audit

**Duplicates** (need consolidation):
- `tests/integration/test_*.py` (18 files)
- `tests/python/integration/test_*.py` (25 files)
- Many overlap in functionality

**To Delete**:
- `tests/python/integration/simple_test.py` - Too vague
- `tests/python/integration/simple_kafka_test.py` - Duplicate
- All files in `tests/python/debug/` - Debug scripts (50+ files)

---

### 🗑️ Debug Scripts (DELETE ALL)

**Status**: EXPERIMENTAL DEBRIS
**Location**: Multiple directories
**Count**: 100+ files

**Categories**:
1. **Protocol debugging** (`tests/python/debug/librdkafka/`, `tests/librdkafka_debug/`)
   - 30+ files for debugging librdkafka metadata issues
   - DELETE: All `analyze_*.py`, `decode_*.py`, `debug_*.py`, `capture_*.py`

2. **Consumer group debugging** (`tests/consumer-group/`)
   - 10+ files: `test_consumer_debug.py`, `test_consumer_fix.py`, `test_consumer_final.py`
   - DELETE: All except one canonical test

3. **WAL debugging** (`tests/debug_wal.py`, `tests/fast_wal_test.py`)
   - DELETE: Functionality covered by Rust tests

4. **Metadata debugging** (`tests/check_metadata.py`, `tests/analyze_*.py`)
   - DELETE: Not actual tests

**Action**: Create deletion script (Phase 3 of cleanup plan)

---

### ⚠️ Compatibility Tests

**Status**: DISORGANIZED
**Location**: `tests/compatibility/`, `tests/compat-tests/`
**Issues**: Multiple overlapping directories

**Canonical Structure** (target):
```
tests/compatibility/
├── kafka_python/       # kafka-python 2.0.2 tests
├── confluent_kafka/    # librdkafka-based client tests
└── java/               # Java client tests (KSQLDB)
```

**Current State**:
- `tests/compatibility/` - 4 Python files (what do these test?)
- `tests/compat-tests/` - Old compatibility test framework (Rust)
- Mixed with `tests/python/integration/test_kafka_python_integration.py`

**Action**: Consolidate all compatibility tests into standard structure

---

### ⚠️ Consumer Group Tests

**Status**: DUPLICATE CHAOS
**Location**: `tests/consumer-group/`
**Count**: 10+ files

**Files**:
- `test_consumer_simple.py`
- `test_consumer_debug.py`
- `test_consumer_fix.py`
- `test_consumer_final.py`
- `test_consumer_after_flush.py`
- `test_simple_consumer.py`
- `test_simple_fetch.py`
- `test_fetch_fix.py`
- `produce_first.py`

**Issues**:
- ❌ Multiple versions of same test (`simple`, `debug`, `fix`, `final`)
- ❌ Unclear which one is canonical
- ❌ No clear test names (what do they test?)

**Action**: Consolidate into ONE canonical test:
- `tests/integration/test_consumer_group_basic_kafka_python.py`

**Delete**: All others in `tests/consumer-group/`

---

### ⚠️ Cluster Tests

**Status**: PARTIAL
**Location**: `tests/cluster/`, `tests/raft/`, `tests/scripts/`
**Count**: ~10 files

**Working Tests**:
- ✅ `tests/test_node_removal.sh` - Node removal testing (NEW)
- ✅ `tests/scripts/test_cluster_manual.sh`
- ⚠️ `tests/raft/*.rs` - Raft-specific tests (need audit)

**Issues**:
- Multiple cluster test directories (consolidate)
- Some tests may be outdated (Raft v1 vs v2)

**Action**: Create `tests/cluster/` directory and consolidate

---

### ⚠️ Performance Tests

**Status**: MINIMAL
**Location**: `tests/python/performance/`
**Count**: 2 files

**Tests**:
- `test_offset_tracking.py`
- `test_offset_performance.py`

**Missing**:
- Throughput benchmarks
- Latency benchmarks (p50/p95/p99)
- Resource usage tests

**Action**: Create proper performance test suite

---

### ❌ Regression Tests

**Status**: MISSING
**Location**: N/A

**Issues**:
- ❌ No dedicated regression test directory
- ❌ Fixed bugs not captured as regression tests
- ❌ Risk of regressions (CRC bugs, WAL checksum, etc.)

**Action**: Create `tests/regression/` with subdirectories per issue:
- `v1.3.28_crc_fix/`
- `v1.3.63_wal_checksum/`
- `consumer_group_rebalance/`

---

## Known Issues

### 1. CRC-32C Validation
**Status**: ✅ FIXED (v1.3.59)
**Issue**: Kafka clients (Java) rejected batches due to CRC mismatch
**Root Cause**: Re-compression produced different bytes than original
**Fix**: Preserve original wire bytes in `compressed_records_wire_bytes`
**Test**: `tests/integration/test_crc32c_validation.py` (exists)

### 2. WAL V2 Checksum
**Status**: ✅ FIXED (v1.3.63)
**Issue**: WalRecord V2 checksum validation failed on read
**Root Cause**: Bincode serialization made checksums non-deterministic
**Fix**: Skip checksum validation for V2 records (rely on Kafka CRC)
**Test**: ❌ MISSING - needs regression test

### 3. Consumer Group Rebalance
**Status**: ✅ FIXED
**Issue**: Consumer group rebalancing had protocol bugs
**Root Cause**: Multiple issues with JoinGroup/SyncGroup
**Fix**: See `docs/CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md`
**Test**: ❌ MISSING - needs regression test

### 4. Partition Assignment
**Status**: ⚠️ ONGOING
**Issue**: Partition distribution bugs with Murmur2 partitioning
**Root Cause**: See `docs/PARTITION_DISTRIBUTION_BUG_ROOT_CAUSE.md`
**Fix**: Under investigation
**Test**: TBD

---

## Test Cleanup Status

### Phase 1: Audit (IN PROGRESS)
- [ ] Create inventory of all 292 Python tests
- [ ] Categorize: KEEP / MERGE / DELETE
- [ ] Identify canonical tests for each feature
- [ ] Document in spreadsheet or JSON

### Phase 2: Migrate (NOT STARTED)
- [ ] Move canonical tests to proper directories
- [ ] Rename to follow conventions
- [ ] Add complete docstrings
- [ ] Update imports and paths

### Phase 3: Delete (NOT STARTED)
- [ ] Delete all debug scripts
- [ ] Delete duplicate tests
- [ ] Delete experimental files
- [ ] Clean git history (if needed)

### Phase 4: Create Infrastructure (IN PROGRESS)
- [x] Create `docs/TESTING_STANDARDS.md`
- [x] Create `tests/run_all_tests.sh`
- [ ] Create `tests/run_integration_tests.py`
- [ ] Create `tests/run_compatibility_tests.sh`
- [ ] Create test templates

---

## Test Execution

### Quick Smoke Test
```bash
# Start server
cargo run --bin chronik-server start

# Run basic functionality test
python3 tests/integration/test_basic_functionality.py
```

### Full Test Suite
```bash
# Build first
cargo build --release --bin chronik-server

# Run all tests
./tests/run_all_tests.sh
```

### Specific Categories
```bash
# Unit tests only
cargo test --workspace --lib --bins

# Integration tests only (Rust)
cargo test --test integration

# Compatibility tests (manual for now)
python3 tests/compatibility/kafka_python/test_suite.py
```

---

## Before Every Release Checklist

- [ ] ✅ Run `./tests/run_all_tests.sh`
- [ ] ✅ Run compatibility tests with kafka-python
- [ ] ✅ Run compatibility tests with confluent-kafka (librdkafka)
- [ ] ✅ Test with Java clients (if Java features changed)
- [ ] ✅ Run cluster tests (if cluster features changed)
- [ ] ✅ Run regression tests (ensure no regressions)
- [ ] ✅ Test on clean environment (not just dev machine)
- [ ] ✅ Document any new known issues in this file

**CRITICAL**: See CLAUDE.md section "Testing Requirements" - NEVER release without testing with THE ACTUAL CLIENT that uses the feature.

---

## Next Actions

### Immediate (This Week)
1. ✅ Create testing standards document
2. ✅ Create master test runner
3. [ ] Audit all Python tests (create inventory)
4. [ ] Create test cleanup script (`tests/scripts/audit_tests.py`)
5. [ ] Start deleting obvious debug scripts

### Short Term (This Month)
1. [ ] Consolidate consumer group tests → 1 canonical test
2. [ ] Consolidate compatibility tests → proper directory structure
3. [ ] Create regression tests for known fixed bugs
4. [ ] Reduce 292 Python files → ~50 canonical tests

### Long Term (Next Quarter)
1. [ ] Performance test suite with benchmarks
2. [ ] Automated cluster test suite
3. [ ] CI/CD integration with test categorization
4. [ ] Test coverage metrics

---

## Notes

- See [docs/TESTING_STANDARDS.md](../docs/TESTING_STANDARDS.md) for complete standards
- Test output goes in `test-results/` (gitignored)
- Always test with native binary on macOS (NO DOCKER)
- Test with the ACTUAL client that uses the feature (Java → Java, Python → Python)
- Delete debug scripts immediately after issue resolution
- ONE canonical test per feature (no versions, no duplicates)
