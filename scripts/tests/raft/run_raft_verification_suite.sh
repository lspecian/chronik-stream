#!/bin/bash

# Raft Verification & Hardening Test Suite
# Runs all critical tests for Raft cluster validation

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m' # No Color

echo -e "${BOLD}========================================${NC}"
echo -e "${BOLD}RAFT VERIFICATION & HARDENING SUITE${NC}"
echo -e "${BOLD}========================================${NC}\n"

# Check if cargo is available
if ! command -v cargo &> /dev/null; then
    echo -e "${RED}ERROR: cargo not found. Please install Rust.${NC}"
    exit 1
fi

# Check if Python 3 is available
if ! command -v python3 &> /dev/null; then
    echo -e "${RED}ERROR: python3 not found.${NC}"
    exit 1
fi

# Check if kafka-python is installed
if ! python3 -c "import kafka" 2>/dev/null; then
    echo -e "${YELLOW}WARNING: kafka-python not found. Installing...${NC}"
    pip3 install kafka-python psutil
fi

# Build the server first
echo -e "${BLUE}Building chronik-server with Raft support...${NC}"
cargo build --release --bin chronik-server --features raft

if [ $? -ne 0 ]; then
    echo -e "${RED}BUILD FAILED${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Build successful${NC}\n"

# Test results
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_TOTAL=3

# Test 1: End-to-End Failure Recovery
echo -e "\n${BOLD}========================================${NC}"
echo -e "${BOLD}TEST 1/3: End-to-End Failure Recovery${NC}"
echo -e "${BOLD}========================================${NC}\n"

if python3 ./test_raft_e2e_failures.py; then
    echo -e "\n${GREEN}✓ Test 1 PASSED${NC}"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo -e "\n${RED}✗ Test 1 FAILED${NC}"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

sleep 5

# Test 2: Leader Failover
echo -e "\n${BOLD}========================================${NC}"
echo -e "${BOLD}TEST 2/3: Leader Failover${NC}"
echo -e "${BOLD}========================================${NC}\n"

if python3 ./test_raft_leader_failover.py; then
    echo -e "\n${GREEN}✓ Test 2 PASSED${NC}"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo -e "\n${RED}✗ Test 2 FAILED${NC}"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

sleep 5

# Test 3: Performance Benchmark
echo -e "\n${BOLD}========================================${NC}"
echo -e "${BOLD}TEST 3/3: Performance Benchmark${NC}"
echo -e "${BOLD}========================================${NC}\n"

if python3 ./benchmark_raft_vs_standalone.py; then
    echo -e "\n${GREEN}✓ Test 3 PASSED${NC}"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo -e "\n${RED}✗ Test 3 FAILED${NC}"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Final summary
echo -e "\n${BOLD}========================================${NC}"
echo -e "${BOLD}FINAL RESULTS${NC}"
echo -e "${BOLD}========================================${NC}\n"

echo -e "Total Tests:  ${TESTS_TOTAL}"
echo -e "${GREEN}Passed:       ${TESTS_PASSED}${NC}"
echo -e "${RED}Failed:       ${TESTS_FAILED}${NC}"

if [ $TESTS_FAILED -eq 0 ]; then
    echo -e "\n${GREEN}${BOLD}🎉 ALL TESTS PASSED!${NC}"
    echo -e "${GREEN}Raft cluster is production-ready.${NC}"
    exit 0
else
    echo -e "\n${RED}${BOLD}❌ SOME TESTS FAILED${NC}"
    echo -e "${YELLOW}Review logs for details:${NC}"
    echo -e "  - node*_e2e.log (failure recovery test)"
    echo -e "  - node*_failover.log (leader failover test)"
    echo -e "  - node*_perf.log (performance benchmark)"
    exit 1
fi
