#!/bin/bash
# Chronik vs Kafka Performance Comparison
# Tests identical workloads against both systems

set -e

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

RESULTS_DIR="/tmp/chronik-kafka-comparison-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RESULTS_DIR"

echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}    Chronik vs Kafka Performance Comparison${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo ""
echo "Results directory: $RESULTS_DIR"
echo ""

# Test parameters
TEST_DURATION="30s"
WARMUP="5s"
CONCURRENCY=32
MESSAGE_SIZE=1024

# Check if Kafka is available
KAFKA_AVAILABLE=false
if lsof -i :9093 | grep -q LISTEN 2>/dev/null; then
    echo -e "${GREEN}✓ Kafka detected on port 9093${NC}"
    KAFKA_AVAILABLE=true
    KAFKA_SERVER="localhost:9093"
else
    echo -e "${YELLOW}⚠ Kafka not detected on port 9093${NC}"
    echo -e "${YELLOW}  To enable comparison:${NC}"
    echo -e "${YELLOW}  1. Download Kafka from https://kafka.apache.org/downloads${NC}"
    echo -e "${YELLOW}  2. Start Kafka on port 9093${NC}"
    echo -e "${YELLOW}  3. Re-run this script${NC}"
    echo ""
    echo -e "${BLUE}Proceeding with Chronik-only tests...${NC}"
fi

# ═══════════════════════════════════════════════════════════
# Test Chronik
# ═══════════════════════════════════════════════════════════

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Testing Chronik${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"

# Start Chronik
echo "Starting Chronik server..."
rm -rf /tmp/chronik-comparison-test
CHRONIK_PRODUCE_PROFILE=balanced \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_DATA_DIR=/tmp/chronik-comparison-test \
CHRONIK_METRICS_PORT=9995 \
./target/release/chronik-server standalone > "$RESULTS_DIR/chronik-server.log" 2>&1 &
CHRONIK_PID=$!
sleep 5

if ! lsof -i :9092 | grep -q LISTEN; then
    echo -e "${RED}✗ Chronik failed to start${NC}"
    cat "$RESULTS_DIR/chronik-server.log"
    exit 1
fi
echo -e "${GREEN}✓ Chronik started (PID: $CHRONIK_PID)${NC}"

# Cleanup function
cleanup() {
    echo ""
    echo "Cleaning up..."
    kill -9 $CHRONIK_PID 2>/dev/null || true
    pkill -9 chronik-server 2>/dev/null || true
}
trap cleanup EXIT

# Test 1: Baseline - No compression
echo ""
echo -e "${YELLOW}Test 1: Baseline (no compression)${NC}"
./target/release/chronik-bench \
    --bootstrap-servers localhost:9092 \
    --topic chronik-test-baseline \
    --concurrency $CONCURRENCY \
    --message-size $MESSAGE_SIZE \
    --duration $TEST_DURATION \
    --warmup-duration $WARMUP \
    --compression none \
    --csv-output "$RESULTS_DIR/chronik-baseline.csv" \
    --json-output "$RESULTS_DIR/chronik-baseline.json" \
    2>&1 | tail -25

# Test 2: Snappy compression
echo ""
echo -e "${YELLOW}Test 2: Snappy compression${NC}"
./target/release/chronik-bench \
    --bootstrap-servers localhost:9092 \
    --topic chronik-test-snappy \
    --concurrency $CONCURRENCY \
    --message-size $MESSAGE_SIZE \
    --duration $TEST_DURATION \
    --warmup-duration $WARMUP \
    --compression snappy \
    --batch-size 65536 \
    --linger-ms 10 \
    --csv-output "$RESULTS_DIR/chronik-snappy.csv" \
    --json-output "$RESULTS_DIR/chronik-snappy.json" \
    2>&1 | tail -25

# Test 3: High concurrency
echo ""
echo -e "${YELLOW}Test 3: High concurrency (64 producers)${NC}"
./target/release/chronik-bench \
    --bootstrap-servers localhost:9092 \
    --topic chronik-test-highconc \
    --concurrency 64 \
    --message-size $MESSAGE_SIZE \
    --duration $TEST_DURATION \
    --warmup-duration $WARMUP \
    --compression snappy \
    --csv-output "$RESULTS_DIR/chronik-highconc.csv" \
    --json-output "$RESULTS_DIR/chronik-highconc.json" \
    2>&1 | tail -25

# ═══════════════════════════════════════════════════════════
# Test Kafka (if available)
# ═══════════════════════════════════════════════════════════

if [ "$KAFKA_AVAILABLE" = true ]; then
    echo ""
    echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
    echo -e "${BLUE}Testing Kafka${NC}"
    echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"

    # Test 1: Baseline - No compression
    echo ""
    echo -e "${YELLOW}Test 1: Baseline (no compression)${NC}"
    ./target/release/chronik-bench \
        --bootstrap-servers $KAFKA_SERVER \
        --topic kafka-test-baseline \
        --concurrency $CONCURRENCY \
        --message-size $MESSAGE_SIZE \
        --duration $TEST_DURATION \
        --warmup-duration $WARMUP \
        --compression none \
        --csv-output "$RESULTS_DIR/kafka-baseline.csv" \
        --json-output "$RESULTS_DIR/kafka-baseline.json" \
        2>&1 | tail -25

    # Test 2: Snappy compression
    echo ""
    echo -e "${YELLOW}Test 2: Snappy compression${NC}"
    ./target/release/chronik-bench \
        --bootstrap-servers $KAFKA_SERVER \
        --topic kafka-test-snappy \
        --concurrency $CONCURRENCY \
        --message-size $MESSAGE_SIZE \
        --duration $TEST_DURATION \
        --warmup-duration $WARMUP \
        --compression snappy \
        --batch-size 65536 \
        --linger-ms 10 \
        --csv-output "$RESULTS_DIR/kafka-snappy.csv" \
        --json-output "$RESULTS_DIR/kafka-snappy.json" \
        2>&1 | tail -25

    # Test 3: High concurrency
    echo ""
    echo -e "${YELLOW}Test 3: High concurrency (64 producers)${NC}"
    ./target/release/chronik-bench \
        --bootstrap-servers $KAFKA_SERVER \
        --topic kafka-test-highconc \
        --concurrency 64 \
        --message-size $MESSAGE_SIZE \
        --duration $TEST_DURATION \
        --warmup-duration $WARMUP \
        --compression snappy \
        --csv-output "$RESULTS_DIR/kafka-highconc.csv" \
        --json-output "$RESULTS_DIR/kafka-highconc.json" \
        2>&1 | tail -25
fi

# ═══════════════════════════════════════════════════════════
# Generate Comparison Report
# ═══════════════════════════════════════════════════════════

echo ""
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Generating Comparison Report${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════════════════${NC}"

REPORT="$RESULTS_DIR/COMPARISON_REPORT.md"

cat > "$REPORT" << EOF
# Chronik vs Kafka Performance Comparison

## Test Environment

**Date**: $(date)
**Platform**: $(uname -s) $(uname -r)
**Chronik Version**: $(./target/release/chronik-server --version 2>&1 | head -1 || echo "Unknown")
**Test Duration**: $TEST_DURATION per test
**Warmup**: $WARMUP
**Concurrency**: $CONCURRENCY producers (except high-conc test: 64)
**Message Size**: $MESSAGE_SIZE bytes

## Results Summary

### Chronik Performance

| Test | Throughput (msg/s) | Bandwidth (MB/s) | p50 Latency (ms) | p99 Latency (ms) | p99.9 Latency (ms) | Success Rate |
|------|-------------------|------------------|------------------|------------------|-------------------|--------------|
EOF

# Add Chronik results
for test in baseline snappy highconc; do
    csv="$RESULTS_DIR/chronik-$test.csv"
    if [ -f "$csv" ]; then
        tail -1 "$csv" | awk -F',' -v t="$test" '{
            printf "| %s | %.0f | %.2f | %.2f | %.2f | %.2f | %.1f%% |\n",
                t, $7, $8, $16, $19, $20, $9
        }' >> "$REPORT"
    fi
done

if [ "$KAFKA_AVAILABLE" = true ]; then
    cat >> "$REPORT" << EOF

### Kafka Performance

| Test | Throughput (msg/s) | Bandwidth (MB/s) | p50 Latency (ms) | p99 Latency (ms) | p99.9 Latency (ms) | Success Rate |
|------|-------------------|------------------|------------------|------------------|-------------------|--------------|
EOF

    # Add Kafka results
    for test in baseline snappy highconc; do
        csv="$RESULTS_DIR/kafka-$test.csv"
        if [ -f "$csv" ]; then
            tail -1 "$csv" | awk -F',' -v t="$test" '{
                printf "| %s | %.0f | %.2f | %.2f | %.2f | %.2f | %.1f%% |\n",
                    t, $7, $8, $16, $19, $20, $9
            }' >> "$REPORT"
        fi
    done

    cat >> "$REPORT" << EOF

### Head-to-Head Comparison

| Metric | Test | Chronik | Kafka | Winner | Improvement |
|--------|------|---------|-------|--------|-------------|
EOF

    # Generate comparison
    for test in baseline snappy highconc; do
        chronik_csv="$RESULTS_DIR/chronik-$test.csv"
        kafka_csv="$RESULTS_DIR/kafka-$test.csv"

        if [ -f "$chronik_csv" ] && [ -f "$kafka_csv" ]; then
            # Throughput comparison
            paste <(tail -1 "$chronik_csv") <(tail -1 "$kafka_csv") | \
            awk -F',' -v t="$test" '{
                chronik_tput = $7
                kafka_tput = $14
                if (chronik_tput > kafka_tput) {
                    winner = "Chronik"
                    improvement = sprintf("+%.1f%%", (chronik_tput/kafka_tput - 1) * 100)
                } else {
                    winner = "Kafka"
                    improvement = sprintf("+%.1f%%", (kafka_tput/chronik_tput - 1) * 100)
                }
                printf "| Throughput | %s | %.0f msg/s | %.0f msg/s | %s | %s |\n",
                    t, chronik_tput, kafka_tput, winner, improvement
            }' >> "$REPORT"

            # Latency p99 comparison
            paste <(tail -1 "$chronik_csv") <(tail -1 "$kafka_csv") | \
            awk -F',' -v t="$test" '{
                chronik_p99 = $19
                kafka_p99 = $26
                if (chronik_p99 < kafka_p99) {
                    winner = "Chronik"
                    improvement = sprintf("-%.1f%%", (1 - chronik_p99/kafka_p99) * 100)
                } else {
                    winner = "Kafka"
                    improvement = sprintf("-%.1f%%", (1 - kafka_p99/chronik_p99) * 100)
                }
                printf "| p99 Latency | %s | %.2f ms | %.2f ms | %s | %s |\n",
                    t, chronik_p99, kafka_p99, winner, improvement
            }' >> "$REPORT"
        fi
    done
else
    cat >> "$REPORT" << EOF

### Kafka Comparison

Kafka was not available during testing. To compare with Kafka:
1. Start Kafka on port 9093
2. Re-run this script

EOF
fi

cat >> "$REPORT" << EOF

## Analysis

### Chronik Observations

EOF

# Add Chronik-specific analysis
chronik_baseline="$RESULTS_DIR/chronik-baseline.csv"
if [ -f "$chronik_baseline" ]; then
    tail -1 "$chronik_baseline" | awk -F',' '{
        if ($7 > 10000) {
            print "- **High throughput**: Achieved " int($7) " msg/s in baseline test"
        } else if ($7 > 1000) {
            print "- **Moderate throughput**: Achieved " int($7) " msg/s in baseline test"
        } else {
            print "- **Low throughput**: Achieved " int($7) " msg/s - may need tuning"
        }

        if ($19 < 20) {
            print "- **Excellent latency**: p99 latency of " $19 " ms"
        } else if ($19 < 100) {
            print "- **Good latency**: p99 latency of " $19 " ms"
        } else {
            print "- **High latency**: p99 latency of " $19 " ms - investigate"
        }

        if ($9 == 100) {
            print "- **Perfect reliability**: 100% message success rate"
        } else {
            print "- **Reliability issue**: " $9 "% success rate - investigate failures"
        }
    }' >> "$REPORT"
fi

cat >> "$REPORT" << EOF

## Raw Data

All CSV and JSON files are available in:
\`$RESULTS_DIR\`

## Files Generated

EOF

ls -lh "$RESULTS_DIR" | tail -n +2 | awk '{print "- " $9 " (" $5 ")"}' >> "$REPORT"

echo ""
echo -e "${GREEN}═══════════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}Comparison Complete!${NC}"
echo -e "${GREEN}═══════════════════════════════════════════════════════════${NC}"
echo ""
echo "Report: $REPORT"
echo ""
echo "View report:"
echo "  cat $REPORT"
echo ""
echo "Results directory:"
echo "  ls -lh $RESULTS_DIR"
