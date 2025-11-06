# Test Templates

This directory contains standardized templates for creating new tests in Chronik Stream.

**IMPORTANT**: Before creating a new test, check [tests/TEST_INVENTORY.md](../TEST_INVENTORY.md) to ensure it doesn't already exist.

---

## Available Templates

### 1. Integration Test Template
**File**: `test_template_integration.py`

**Use for**:
- End-to-end functionality tests
- Tests with real Kafka clients
- Complete workflows (produce → consume, consumer groups, etc.)

**How to use**:
```bash
# Copy template
cp tests/templates/test_template_integration.py \
   tests/integration/test_<feature>_<client>.py

# Edit and fill in:
# - Test description
# - Client library imports
# - Test topic name
# - Test methods
```

**Example**:
```bash
cp tests/templates/test_template_integration.py \
   tests/integration/test_consumer_group_basic_kafka_python.py
```

### 2. Regression Test Template
**File**: `test_template_regression.py`

**Use for**:
- Tests that prevent fixed bugs from reoccurring
- Tests for specific GitHub issues
- Version-specific bug fixes

**How to use**:
```bash
# Create regression directory
mkdir -p tests/regression/v<version>_<issue>/

# Copy template
cp tests/templates/test_template_regression.py \
   tests/regression/v<version>_<issue>/test_<feature>.py

# Create README
cat > tests/regression/v<version>_<issue>/README.md << 'EOF'
# Regression Test: <Issue Title>

**Issue**: #<issue_number>
**Version Fixed**: v<version>
**Root Cause**: <Brief description>
**Client**: <kafka-python|confluent-kafka|Java>

## Bug Description
<Detailed description of the bug>

## Fix
<What was changed to fix it>

## Test
<How to reproduce and verify fix>
EOF
```

**Example**:
```bash
mkdir -p tests/regression/v1.3.63_wal_checksum/
cp tests/templates/test_template_regression.py \
   tests/regression/v1.3.63_wal_checksum/test_wal_v2_checksum.py
```

---

## Test Naming Conventions

### Format
`test_<category>_<feature>_<client>.py`

### Examples
- ✅ `test_produce_basic_kafka_python.py` - Basic produce with kafka-python
- ✅ `test_consumer_group_rebalance_librdkafka.py` - Consumer group test with librdkafka
- ✅ `test_wal_recovery_crash_scenario.py` - WAL recovery test
- ❌ `test_fix.py` - Too vague
- ❌ `test_consumer_final.py` - What happened to v1-v999?

---

## Directory Structure

Place tests in the appropriate directory:

```
tests/
├── integration/           # General integration tests
│   └── test_<feature>_<client>.py
│
├── compatibility/         # Client-specific compatibility tests
│   ├── kafka_python/
│   ├── confluent_kafka/
│   └── java/
│
├── regression/            # Regression tests (by version/issue)
│   └── v<version>_<issue>/
│       ├── README.md
│       └── test_<feature>.py
│
├── cluster/               # Cluster-specific tests
│   └── test_<feature>.sh
│
└── performance/           # Performance benchmarks
    └── test_<metric>.py
```

---

## Template Checklist

Before creating a test from a template:

- [ ] Does this test already exist? (Check `tests/TEST_INVENTORY.md`)
- [ ] What category does this belong to?
- [ ] What client library will I use?
- [ ] Is this a regression test? (Need issue number and version)
- [ ] Does the name follow conventions?
- [ ] Is the docstring complete?
- [ ] Will this be the CANONICAL test (not debug/experimental)?
- [ ] Am I testing the EXACT scenario reported?

---

## Documentation Requirements

Every test MUST include:

### 1. Module Docstring
```python
"""
<Brief description of what this test validates>

Test Category: <integration|compatibility|regression|cluster|performance>
Client: <kafka-python 2.0.2|confluent-kafka 1.9.0|Java kafka-clients 3.4.0>
Purpose: <Detailed purpose>
Related Issue: <#issue_number if regression test>
"""
```

### 2. Test Function Docstring
```python
def test_feature_scenario():
    """
    Test that <action> results in <expected outcome> when <conditions>.

    Steps:
    1. <Setup step>
    2. <Action step>
    3. <Verification step>

    Expected: <Expected behavior>
    """
```

### 3. Regression Test README
For regression tests, create `README.md` in the test directory:
```markdown
# Regression Test: <Issue Title>

**Issue**: #<issue_number>
**Version Fixed**: v<version>
**Root Cause**: <Brief description>

## Bug Description
<What went wrong>

## Fix
<What was changed>

## Test
<How to reproduce and verify>
```

---

## Running Tests

### Single Test
```bash
python3 tests/integration/test_<feature>.py
```

### All Tests
```bash
./tests/run_all_tests.sh
```

### Specific Category
```bash
cargo test --test integration  # Rust integration tests
python3 tests/run_integration_tests.py  # Python integration tests
```

---

## Example Workflow

### Creating a New Integration Test

```bash
# 1. Check if test exists
cat tests/TEST_INVENTORY.md | grep -i "consumer group"

# 2. Copy template
cp tests/templates/test_template_integration.py \
   tests/integration/test_consumer_group_basic_kafka_python.py

# 3. Edit test
vim tests/integration/test_consumer_group_basic_kafka_python.py
# - Update docstring
# - Update TEST_TOPIC = 'test-consumer-group-basic'
# - Implement test methods

# 4. Run test
python3 tests/integration/test_consumer_group_basic_kafka_python.py

# 5. Update inventory
echo "## Consumer Group Tests" >> tests/TEST_INVENTORY.md
echo "- test_consumer_group_basic_kafka_python.py - Basic consumer group functionality" >> tests/TEST_INVENTORY.md

# 6. Commit
git add tests/integration/test_consumer_group_basic_kafka_python.py
git commit -m "test: Add canonical consumer group integration test"
```

### Creating a Regression Test

```bash
# 1. Create regression directory
mkdir -p tests/regression/v1.3.63_wal_checksum/

# 2. Copy template
cp tests/templates/test_template_regression.py \
   tests/regression/v1.3.63_wal_checksum/test_wal_v2_checksum.py

# 3. Create README
cat > tests/regression/v1.3.63_wal_checksum/README.md << 'EOF'
# Regression Test: WAL V2 Checksum Validation

**Issue**: #789
**Version Fixed**: v1.3.63
**Root Cause**: Bincode serialization made checksums non-deterministic

## Bug Description
WalRecord V2 checksum validation failed on read.

## Fix
Skip checksum validation for V2 records (rely on Kafka CRC).

## Test
Produce → restart → consume (should succeed).
EOF

# 4. Edit test
vim tests/regression/v1.3.63_wal_checksum/test_wal_v2_checksum.py
# - Update issue number
# - Update version
# - Implement reproduction steps

# 5. Run test
python3 tests/regression/v1.3.63_wal_checksum/test_wal_v2_checksum.py

# 6. Commit
git add tests/regression/v1.3.63_wal_checksum/
git commit -m "test: Add regression test for WAL V2 checksum issue #789"
```

---

## See Also

- [docs/TESTING_STANDARDS.md](../../docs/TESTING_STANDARDS.md) - Complete testing standards
- [tests/TEST_INVENTORY.md](../TEST_INVENTORY.md) - Catalog of all tests
- [tests/run_all_tests.sh](../run_all_tests.sh) - Master test runner
- [CLAUDE.md](../../CLAUDE.md) - Testing requirements and work ethic
