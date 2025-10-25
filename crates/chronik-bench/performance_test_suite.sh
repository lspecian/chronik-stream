#!/bin/bash
# Comprehensive Performance Test Suite for Chronik
# Compares different configurations and produces detailed reports

set -e

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m' # No Color

# Configuration
RESULTS_DIR="/tmp/chronik-perf-results-$(date +%Y%m%d-%H%M%S)"
CHRONIK_BIN="./target/release/chronik-server"
BENCH_BIN="./target/release/chronik-bench"
DATA_DIR="/tmp/chronik-perf-test"

# Test parameters
BOOTSTRAP_SERVER="localhost:9092"
TEST_DURATION="30s"
WARMUP_DURATION="5s"
REPORT_INTERVAL="5"

echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}    Chronik Performance Test Suite${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo ""
echo "Results directory: $RESULTS_DIR"
mkdir -p "$RESULTS_DIR"

# Function to start Chronik server with specific profile
start_chronik() {
    local profile=$1
    local server_log="$RESULTS_DIR/chronik-server-$profile.log"

    echo -e "${YELLOW}Starting Chronik with profile: $profile${NC}"

    # Clean up old data
    rm -rf "$DATA_DIR"

    # Start server
    CHRONIK_PRODUCE_PROFILE=$profile \
    CHRONIK_ADVERTISED_ADDR=localhost \
    CHRONIK_DATA_DIR="$DATA_DIR" \
    CHRONIK_METRICS_PORT=9995 \
    $CHRONIK_BIN standalone > "$server_log" 2>&1 &

    SERVER_PID=$!
    echo "Server PID: $SERVER_PID"

    # Wait for server to be ready
    sleep 5

    # Verify server is running
    if lsof -i :9092 | grep -q LISTEN; then
        echo -e "${GREEN}✓ Server started successfully${NC}"
        return 0
    else
        echo -e "${RED}✗ Server failed to start${NC}"
        cat "$server_log"
        return 1
    fi
}

# Function to stop Chronik server
stop_chronik() {
    echo -e "${YELLOW}Stopping Chronik server...${NC}"
    pkill -9 chronik-server 2>/dev/null || true
    sleep 2
    echo -e "${GREEN}✓ Server stopped${NC}"
}

# Function to run benchmark
run_benchmark() {
    local test_name=$1
    local concurrency=$2
    local message_size=$3
    local compression=$4
    local batch_size=$5
    local linger_ms=$6

    local csv_output="$RESULTS_DIR/${test_name}.csv"
    local json_output="$RESULTS_DIR/${test_name}.json"
    local log_output="$RESULTS_DIR/${test_name}.log"

    echo ""
    echo -e "${BLUE}Running: $test_name${NC}"
    echo "  Concurrency: $concurrency"
    echo "  Message size: $message_size bytes"
    echo "  Compression: $compression"
    echo "  Batch size: $batch_size bytes"
    echo "  Linger: ${linger_ms}ms"

    $BENCH_BIN \
        --bootstrap-servers "$BOOTSTRAP_SERVER" \
        --topic "perf-test-$test_name" \
        --concurrency "$concurrency" \
        --message-size "$message_size" \
        --duration "$TEST_DURATION" \
        --warmup-duration "$WARMUP_DURATION" \
        --compression "$compression" \
        --batch-size "$batch_size" \
        --linger-ms "$linger_ms" \
        --report-interval-secs "$REPORT_INTERVAL" \
        --csv-output "$csv_output" \
        --json-output "$json_output" \
        2>&1 | tee "$log_output"

    if [ ${PIPESTATUS[0]} -eq 0 ]; then
        echo -e "${GREEN}✓ Test completed${NC}"
        return 0
    else
        echo -e "${RED}✗ Test failed${NC}"
        return 1
    fi
}

# Cleanup on exit
cleanup() {
    echo ""
    echo -e "${YELLOW}Cleaning up...${NC}"
    stop_chronik
}
trap cleanup EXIT

# ═══════════════════════════════════════════════════════════
# TEST SUITE 1: Low-Latency Profile
# ═══════════════════════════════════════════════════════════

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}TEST SUITE 1: Low-Latency Profile${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"

start_chronik "low-latency"

# Test 1.1: Minimal latency (single producer, tiny messages)
run_benchmark "lowlat-1-minimal" 1 128 "none" 1 0

# Test 1.2: Low latency with moderate load
run_benchmark "lowlat-2-moderate" 8 512 "none" 1024 0

# Test 1.3: Low latency with higher concurrency
run_benchmark "lowlat-3-concurrent" 32 1024 "none" 16384 1

stop_chronik
sleep 3

# ═══════════════════════════════════════════════════════════
# TEST SUITE 2: Balanced Profile
# ═══════════════════════════════════════════════════════════

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}TEST SUITE 2: Balanced Profile${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"

start_chronik "balanced"

# Test 2.1: Standard workload
run_benchmark "balanced-1-standard" 16 1024 "none" 16384 0

# Test 2.2: High concurrency
run_benchmark "balanced-2-highconc" 64 1024 "none" 32768 5

# Test 2.3: Large messages
run_benchmark "balanced-3-large" 32 4096 "none" 65536 10

# Test 2.4: With compression
run_benchmark "balanced-4-snappy" 32 2048 "snappy" 65536 10

stop_chronik
sleep 3

# ═══════════════════════════════════════════════════════════
# TEST SUITE 3: High-Throughput Profile
# ═══════════════════════════════════════════════════════════

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}TEST SUITE 3: High-Throughput Profile${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"

start_chronik "high-throughput"

# Test 3.1: Maximum throughput attempt
run_benchmark "hightput-1-maxconc" 128 1024 "snappy" 65536 10

# Test 3.2: Large batches
run_benchmark "hightput-2-largebatch" 64 2048 "snappy" 131072 20

# Test 3.3: Compression comparison - gzip
run_benchmark "hightput-3-gzip" 64 2048 "gzip" 65536 10

# Test 3.4: Compression comparison - lz4
run_benchmark "hightput-4-lz4" 64 2048 "lz4" 65536 10

stop_chronik

# ═══════════════════════════════════════════════════════════
# GENERATE SUMMARY REPORT
# ═══════════════════════════════════════════════════════════

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Generating Summary Report${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"

SUMMARY_FILE="$RESULTS_DIR/SUMMARY.md"

cat > "$SUMMARY_FILE" << 'EOF'
# Chronik Performance Test Results

## Test Environment

**Date**: $(date)
**Platform**: $(uname -s) $(uname -r)
**Chronik Version**: $(./target/release/chronik-server --version 2>&1 | head -1)
**CPU**: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "Unknown")
**RAM**: $(sysctl -n hw.memsize 2>/dev/null | awk '{print $1/1024/1024/1024 " GB"}' || echo "Unknown")

## Test Configuration

- **Test Duration**: 30 seconds per test
- **Warmup**: 5 seconds
- **Bootstrap Server**: localhost:9092

## Summary Table

| Test Name | Profile | Concurrency | Msg Size | Throughput (msg/s) | Throughput (MB/s) | p50 (ms) | p99 (ms) | p99.9 (ms) |
|-----------|---------|-------------|----------|-------------------|-------------------|----------|----------|------------|
EOF

# Parse CSV files and add to summary
for csv in "$RESULTS_DIR"/*.csv; do
    if [ -f "$csv" ]; then
        test_name=$(basename "$csv" .csv)

        # Extract key metrics from CSV (skip header)
        tail -n 1 "$csv" | awk -F',' -v name="$test_name" '{
            printf "| %s | - | %s | %s | %.0f | %.2f | %.2f | %.2f | %.2f |\n",
                name, $23, $22, $7, $8, $16, $19, $20
        }' >> "$SUMMARY_FILE"
    fi
done

cat >> "$SUMMARY_FILE" << 'EOF'

## Detailed Results

See individual CSV files in the results directory for complete data.

## Files Generated

EOF

# List all files
ls -lh "$RESULTS_DIR" | tail -n +2 | awk '{print "- " $9 " (" $5 ")"}' >> "$SUMMARY_FILE"

echo ""
echo -e "${GREEN}═══════════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}All tests completed!${NC}"
echo -e "${GREEN}═══════════════════════════════════════════════════════════${NC}"
echo ""
echo "Results directory: $RESULTS_DIR"
echo "Summary report: $SUMMARY_FILE"
echo ""
echo "View summary:"
echo "  cat $SUMMARY_FILE"
echo ""
echo "View individual results:"
echo "  ls -lh $RESULTS_DIR"
