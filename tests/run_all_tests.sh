#!/usr/bin/env bash
# Master test runner for Chronik Stream
# Runs comprehensive test suite following TESTING_STANDARDS.md

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Test results directory
TEST_RESULTS_DIR="test-results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOG_DIR="${TEST_RESULTS_DIR}/${TIMESTAMP}"

mkdir -p "${LOG_DIR}"

echo -e "${BLUE}======================================${NC}"
echo -e "${BLUE}Chronik Stream Test Suite${NC}"
echo -e "${BLUE}======================================${NC}"
echo ""
echo "Test results: ${LOG_DIR}"
echo ""

FAILED_TESTS=()
PASSED_TESTS=()

run_test_suite() {
    local name=$1
    local command=$2
    local log_file="${LOG_DIR}/${name}.log"

    echo -e "${YELLOW}[RUNNING]${NC} ${name}..."

    if eval "${command}" > "${log_file}" 2>&1; then
        echo -e "${GREEN}[PASSED]${NC} ${name}"
        PASSED_TESTS+=("${name}")
    else
        echo -e "${RED}[FAILED]${NC} ${name}"
        echo "         Log: ${log_file}"
        FAILED_TESTS+=("${name}")
    fi
}

# ============================================
# 1. Unit Tests (Rust)
# ============================================
echo -e "${BLUE}=== Unit Tests (Rust) ===${NC}"
run_test_suite "unit-tests-rust" "cargo test --workspace --lib --bins --quiet"
echo ""

# ============================================
# 2. Integration Tests (Rust)
# ============================================
echo -e "${BLUE}=== Integration Tests (Rust) ===${NC}"

# Check if integration tests exist
if cargo test --test integration --no-run &> /dev/null; then
    run_test_suite "integration-tests-rust" "cargo test --test integration --quiet"
else
    echo -e "${YELLOW}[SKIPPED]${NC} integration-tests-rust (not found)"
fi
echo ""

# ============================================
# 3. Integration Tests (Python)
# ============================================
echo -e "${BLUE}=== Integration Tests (Python) ===${NC}"

# Check if Python integration tests exist
if [ -d "tests/integration" ] && ls tests/integration/*.py 1> /dev/null 2>&1; then
    # Run basic functionality test as a smoke test
    if [ -f "tests/integration/test_basic_functionality.py" ]; then
        run_test_suite "integration-basic-functionality" \
            "python3 tests/integration/test_basic_functionality.py"
    fi

    # Run Kafka compatibility tests
    if [ -f "tests/integration/test_kafka_compatibility.py" ]; then
        run_test_suite "integration-kafka-compatibility" \
            "python3 tests/integration/test_kafka_compatibility.py"
    fi
else
    echo -e "${YELLOW}[SKIPPED]${NC} Python integration tests (not found)"
fi
echo ""

# ============================================
# 4. Compatibility Tests
# ============================================
echo -e "${BLUE}=== Compatibility Tests ===${NC}"

# kafka-python compatibility
if [ -d "tests/compatibility/kafka_python" ]; then
    echo -e "${YELLOW}[INFO]${NC} kafka-python compatibility tests found (manual run required)"
else
    echo -e "${YELLOW}[SKIPPED]${NC} kafka-python compatibility (not found)"
fi

# confluent-kafka compatibility
if [ -d "tests/compatibility/confluent_kafka" ]; then
    echo -e "${YELLOW}[INFO]${NC} confluent-kafka compatibility tests found (manual run required)"
else
    echo -e "${YELLOW}[SKIPPED]${NC} confluent-kafka compatibility (not found)"
fi
echo ""

# ============================================
# 5. Cluster Tests (if available)
# ============================================
echo -e "${BLUE}=== Cluster Tests ===${NC}"

if [ -f "./tests/run_cluster_tests.sh" ]; then
    run_test_suite "cluster-tests" "./tests/run_cluster_tests.sh"
elif [ -f "./tests/test_node_removal.sh" ]; then
    echo -e "${YELLOW}[INFO]${NC} Node removal test available (requires manual cluster setup)"
else
    echo -e "${YELLOW}[SKIPPED]${NC} Cluster tests (not found)"
fi
echo ""

# ============================================
# 6. Regression Tests
# ============================================
echo -e "${BLUE}=== Regression Tests ===${NC}"

if [ -d "tests/regression" ]; then
    regression_count=$(find tests/regression -name "test_*.py" -o -name "test_*.sh" | wc -l)
    if [ "$regression_count" -gt 0 ]; then
        echo -e "${YELLOW}[INFO]${NC} ${regression_count} regression tests found (manual run required)"
    else
        echo -e "${YELLOW}[SKIPPED]${NC} No regression tests found"
    fi
else
    echo -e "${YELLOW}[SKIPPED]${NC} Regression tests (directory not found)"
fi
echo ""

# ============================================
# Summary
# ============================================
echo -e "${BLUE}======================================${NC}"
echo -e "${BLUE}Test Summary${NC}"
echo -e "${BLUE}======================================${NC}"
echo ""
echo -e "${GREEN}Passed:${NC} ${#PASSED_TESTS[@]}"
for test in "${PASSED_TESTS[@]}"; do
    echo "  ✓ ${test}"
done
echo ""

if [ ${#FAILED_TESTS[@]} -gt 0 ]; then
    echo -e "${RED}Failed:${NC} ${#FAILED_TESTS[@]}"
    for test in "${FAILED_TESTS[@]}"; do
        echo "  ✗ ${test}"
    done
    echo ""
    echo -e "${RED}Some tests failed. Check logs in ${LOG_DIR}${NC}"
    exit 1
else
    echo -e "${GREEN}All tests passed!${NC}"
    echo ""
    echo "Full logs: ${LOG_DIR}"
fi
