#!/bin/bash
# Test script for chronik-bench
set -e

echo "=== Chronik Benchmark Test Script ==="
echo ""

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

# Check if chronik-server is running
check_server() {
    echo -n "Checking if Chronik server is running... "
    if lsof -i :9092 | grep -q LISTEN; then
        echo -e "${GREEN}OK${NC}"
        return 0
    else
        echo -e "${RED}NOT RUNNING${NC}"
        echo ""
        echo -e "${YELLOW}Please start Chronik server first:${NC}"
        echo "  cd /Users/lspecian/Development/chronik-stream"
        echo "  CHRONIK_ADVERTISED_ADDR=localhost ./target/release/chronik-server standalone"
        return 1
    fi
}

# Run benchmark test
run_test() {
    local name=$1
    local args=$2

    echo ""
    echo -e "${GREEN}=== Test: $name ===${NC}"
    echo "Command: chronik-bench $args"
    echo ""

    ./../../target/release/chronik-bench $args

    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✓ Test passed${NC}"
    else
        echo -e "${RED}✗ Test failed${NC}"
        exit 1
    fi
}

# Main
if ! check_server; then
    exit 1
fi

echo ""
echo "=== Running Benchmark Tests ==="

# Test 1: Quick produce test (10 seconds)
run_test "Quick Produce Test" \
    "--bootstrap-servers localhost:9092 \
     --topic bench-test-quick \
     --concurrency 8 \
     --message-size 512 \
     --duration 10s \
     --warmup-duration 2s \
     --report-interval-secs 2"

# Test 2: Produce with CSV output
run_test "Produce with CSV Output" \
    "--bootstrap-servers localhost:9092 \
     --topic bench-test-csv \
     --concurrency 16 \
     --message-size 1024 \
     --duration 10s \
     --csv-output /tmp/chronik-bench-test.csv \
     --warmup-duration 2s"

# Verify CSV was created
if [ -f /tmp/chronik-bench-test.csv ]; then
    echo -e "${GREEN}✓ CSV file created${NC}"
    echo "CSV contents:"
    cat /tmp/chronik-bench-test.csv
else
    echo -e "${RED}✗ CSV file not created${NC}"
    exit 1
fi

# Test 3: Produce with compression
run_test "Produce with Snappy Compression" \
    "--bootstrap-servers localhost:9092 \
     --topic bench-test-compressed \
     --concurrency 4 \
     --message-size 2048 \
     --duration 10s \
     --compression snappy \
     --warmup-duration 2s"

# Test 4: Produce with JSON output
run_test "Produce with JSON Output" \
    "--bootstrap-servers localhost:9092 \
     --topic bench-test-json \
     --concurrency 8 \
     --message-size 1024 \
     --duration 10s \
     --json-output /tmp/chronik-bench-test.json \
     --warmup-duration 2s"

# Verify JSON was created
if [ -f /tmp/chronik-bench-test.json ]; then
    echo -e "${GREEN}✓ JSON file created${NC}"
    echo "JSON contents:"
    cat /tmp/chronik-bench-test.json | jq '.' 2>/dev/null || cat /tmp/chronik-bench-test.json
else
    echo -e "${RED}✗ JSON file not created${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}=== All tests passed! ===${NC}"
echo ""
echo "Benchmark binary location: ./../../target/release/chronik-bench"
echo ""
echo "Example usage:"
echo "  ./../../target/release/chronik-bench --help"
echo "  ./../../target/release/chronik-bench --concurrency 64 --duration 60s --csv-output results.csv"
