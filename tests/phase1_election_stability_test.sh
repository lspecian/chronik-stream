#!/bin/bash
# Phase 1: Election Stability Test (Quick validation)
# Tests that new Raft config (3000ms election timeout, 150ms heartbeat) stops election churn

set -e

echo "=========================================="
echo "Phase 1: Election Stability Test"
echo "=========================================="
echo ""

# Configuration
TEST_DURATION=300  # 5 minutes
LOG_DIR="./phase1_test_logs"
EXPECTED_ELECTIONS=27  # 9 partitions × 3 replicas
MAX_ACCEPTABLE_ELECTIONS=$((EXPECTED_ELECTIONS + 10))

# Cleanup function
cleanup() {
    echo ""
    echo "Cleaning up test cluster..."
    pkill -f "chronik-server.*9092" || true
    pkill -f "chronik-server.*9093" || true
    pkill -f "chronik-server.*9094" || true
    sleep 2
}

# Trap cleanup on exit
trap cleanup EXIT

# Clean previous logs
rm -rf "$LOG_DIR"
mkdir -p "$LOG_DIR"

# Clean data directories
rm -rf ./data_node1 ./data_node2 ./data_node3

echo "Starting 3-node Raft cluster with Phase 1 config..."
echo "  - Election timeout: 3000ms"
echo "  - Heartbeat interval: 150ms"
echo "  - Election ticks: 20 (production-safe)"
echo ""

# Start node 1
CHRONIK_DATA_DIR=./data_node1 \
CHRONIK_NODE_ID=1 \
CHRONIK_KAFKA_PORT=9092 \
CHRONIK_RAFT_PORT=9192 \
CHRONIK_ADVERTISED_ADDR=localhost \
./target/release/chronik-server all > "$LOG_DIR/node1.log" 2>&1 &
NODE1_PID=$!
echo "Node 1 started (PID: $NODE1_PID)"

sleep 2

# Start node 2
CHRONIK_DATA_DIR=./data_node2 \
CHRONIK_NODE_ID=2 \
CHRONIK_KAFKA_PORT=9093 \
CHRONIK_RAFT_PORT=9193 \
CHRONIK_ADVERTISED_ADDR=localhost \
./target/release/chronik-server all > "$LOG_DIR/node2.log" 2>&1 &
NODE2_PID=$!
echo "Node 2 started (PID: $NODE2_PID)"

sleep 2

# Start node 3
CHRONIK_DATA_DIR=./data_node3 \
CHRONIK_NODE_ID=3 \
CHRONIK_KAFKA_PORT=9094 \
CHRONIK_RAFT_PORT=9194 \
CHRONIK_ADVERTISED_ADDR=localhost \
./target/release/chronik-server all > "$LOG_DIR/node3.log" 2>&1 &
NODE3_PID=$!
echo "Node 3 started (PID: $NODE3_PID)"

echo ""
echo "Waiting for cluster formation (10 seconds)..."
sleep 10

# Create test topic (9 partitions, 3 replicas)
echo "Creating topic: stability-test (9 partitions, replication-factor 3)"
if command -v kafka-topics &> /dev/null; then
    kafka-topics --bootstrap-server localhost:9092 --create \
      --topic stability-test --partitions 9 --replication-factor 3 \
      2>&1 | tee -a "$LOG_DIR/kafka-commands.log" || echo "Topic may already exist"
else
    echo "WARN: kafka-topics command not found. Install via: brew install kafka"
    echo "Skipping topic creation."
fi

echo ""
echo "Waiting for leader elections (5 seconds)..."
sleep 5

echo ""
echo "Capturing initial term numbers..."
INITIAL_TERMS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | \
    grep "stability-test" | \
    awk '{for(i=1;i<=NF;i++) if($i=="term") print $(i+1)}' | \
    sed 's/term=//' | sort -u || echo "No elections yet")
echo "Initial terms: ${INITIAL_TERMS:-None}"

# Record start time
START_TIME=$(date +%s)

echo ""
echo "=========================================="
echo "Monitoring cluster for $TEST_DURATION seconds..."
echo "Time: $(date)"
echo "=========================================="
echo ""
echo "Press Ctrl+C to stop early and see results"
echo ""

# Monitor loop
PREV_COUNT=0
for i in $(seq 1 $TEST_DURATION); do
    sleep 1

    # Every 30 seconds, show status
    if [ $((i % 30)) -eq 0 ]; then
        CURRENT_COUNT=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | wc -l || echo 0)
        NEW_ELECTIONS=$((CURRENT_COUNT - PREV_COUNT))
        PREV_COUNT=$CURRENT_COUNT

        echo "[$(date +%H:%M:%S)] Progress: ${i}s/${TEST_DURATION}s | Total elections: $CURRENT_COUNT | Last 30s: $NEW_ELECTIONS"
    fi
done

# Test complete
END_TIME=$(date +%s)
ELAPSED=$((END_TIME - START_TIME))

echo ""
echo "=========================================="
echo "Test Complete - Analyzing Results"
echo "=========================================="
echo ""

# Final term numbers
FINAL_TERMS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | \
    grep "stability-test" | \
    awk '{for(i=1;i<=NF;i++) if($i=="term") print $(i+1)}' | \
    sed 's/term=//' | sort -u || echo "No elections")
echo "Final term numbers: ${FINAL_TERMS:-None}"

# Count total elections
TOTAL_ELECTIONS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | \
    grep "stability-test" | wc -l || echo 0)
echo "Total elections: $TOTAL_ELECTIONS"
echo "Expected: $EXPECTED_ELECTIONS (initial leader election)"
echo "Acceptable: ≤ $MAX_ACCEPTABLE_ELECTIONS"

# Count "lower term" errors
LOWER_TERM_ERRORS=$(grep -h "ignored a message with lower term" "$LOG_DIR"/node*.log 2>/dev/null | wc -l || echo 0)
echo "Lower term errors: $LOWER_TERM_ERRORS"

# Max term reached
MAX_TERM=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | \
    grep "stability-test" | \
    awk '{for(i=1;i<=NF;i++) if($i=="term") print $(i+1)}' | \
    sed 's/term=//' | sort -n | tail -1 || echo 0)
echo "Max term reached: $MAX_TERM"

echo ""
echo "=========================================="
echo "Pass/Fail Analysis"
echo "=========================================="

PASS=true

# Check 1: Term numbers should be low (1-10 is acceptable)
if [ "$MAX_TERM" -le 10 ]; then
    echo "✅ PASS: Term stability (max term: $MAX_TERM ≤ 10)"
else
    echo "❌ FAIL: Excessive term growth (max term: $MAX_TERM > 10)"
    PASS=false
fi

# Check 2: Total elections should be reasonable
if [ "$TOTAL_ELECTIONS" -le "$MAX_ACCEPTABLE_ELECTIONS" ]; then
    echo "✅ PASS: Election count reasonable ($TOTAL_ELECTIONS ≤ $MAX_ACCEPTABLE_ELECTIONS)"
else
    echo "❌ FAIL: Excessive elections ($TOTAL_ELECTIONS > $MAX_ACCEPTABLE_ELECTIONS)"
    PASS=false
fi

# Check 3: Lower term errors should be minimal
if [ "$LOWER_TERM_ERRORS" -le 100 ]; then
    echo "✅ PASS: Lower term errors acceptable ($LOWER_TERM_ERRORS ≤ 100)"
else
    echo "❌ FAIL: Too many lower term errors ($LOWER_TERM_ERRORS > 100)"
    PASS=false
fi

echo ""
echo "=========================================="
if [ "$PASS" = true ]; then
    echo "✅ OVERALL: PASS - Phase 1 config stabilizes elections!"
    echo ""
    echo "Phase 1 SUCCESS! The new timeout configuration (3000ms election, 150ms heartbeat)"
    echo "successfully prevents election churn. Ready to proceed to Phase 2."
    EXIT_CODE=0
else
    echo "❌ OVERALL: FAIL - Election churn still present"
    echo ""
    echo "Phase 1 INCOMPLETE. Review logs in $LOG_DIR for debugging."
    echo "Common issues:"
    echo "  - Network delays exceeding 3s (unlikely)"
    echo "  - State machine blocking (Phase 2 will fix)"
    echo "  - Raft message ordering issues"
    EXIT_CODE=1
fi
echo "=========================================="
echo ""
echo "Logs saved to: $LOG_DIR"
echo "  - node1.log, node2.log, node3.log"
echo ""

exit $EXIT_CODE
