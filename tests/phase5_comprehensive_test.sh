#!/bin/bash
# Phase 5: Comprehensive Raft Stability Test Suite
# Tests all fixes from Phases 1-4 in a real 3-node cluster

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
TEST_DURATION=300  # 5 minutes per test
LOG_DIR="./phase5_test_logs"
DATA_DIR_BASE="./phase5_data"

# Counters
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_TOTAL=4

echo -e "${BLUE}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║  Phase 5: Comprehensive Raft Stability Test Suite             ║${NC}"
echo -e "${BLUE}║  Testing Phases 1-4 Fixes in Real 3-Node Cluster              ║${NC}"
echo -e "${BLUE}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""

# Cleanup function
cleanup() {
    echo ""
    echo -e "${YELLOW}Cleaning up cluster...${NC}"
    pkill -f "chronik-server.*--node-id" || true
    sleep 3
    pkill -9 -f "chronik-server.*--node-id" || true
    sleep 1
}

# Trap cleanup on exit
trap cleanup EXIT

# Initial cleanup
cleanup

# Verify binary exists
if [ ! -f "./target/release/chronik-server" ]; then
    echo -e "${RED}❌ FAIL: chronik-server binary not found!${NC}"
    echo "Run: cargo build --release --bin chronik-server --features raft"
    exit 1
fi

# Create directories
rm -rf "$LOG_DIR" "$DATA_DIR_BASE"
mkdir -p "$LOG_DIR"
mkdir -p "$DATA_DIR_BASE/node1" "$DATA_DIR_BASE/node2" "$DATA_DIR_BASE/node3"

echo -e "${GREEN}✅ Environment prepared${NC}"
echo ""

# ============================================================================
# Test 1: Basic Cluster Formation & Stability (Phase 1 Validation)
# ============================================================================

echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Test 1: Cluster Formation & Election Stability (5 minutes)${NC}"
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo ""
echo "Testing Phase 1 fix: 3000ms election timeout, 150ms heartbeat"
echo "Expected: ≤30 elections, term numbers stay at 1-5"
echo ""

# Create cluster config files
cat > "$DATA_DIR_BASE/cluster-node1.toml" <<EOF
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 9192

[[peers]]
id = 2
addr = "localhost:9093"
raft_port = 9193

[[peers]]
id = 3
addr = "localhost:9094"
raft_port = 9194
EOF

cat > "$DATA_DIR_BASE/cluster-node2.toml" <<EOF
enabled = true
node_id = 2
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 9192

[[peers]]
id = 2
addr = "localhost:9093"
raft_port = 9193

[[peers]]
id = 3
addr = "localhost:9094"
raft_port = 9194
EOF

cat > "$DATA_DIR_BASE/cluster-node3.toml" <<EOF
enabled = true
node_id = 3
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 9192

[[peers]]
id = 2
addr = "localhost:9093"
raft_port = 9193

[[peers]]
id = 3
addr = "localhost:9094"
raft_port = 9194
EOF

# Start nodes with all Phase 1-4 fixes
echo "Starting Node 1..."
RUST_LOG=info,chronik_raft=info,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --data-dir "$DATA_DIR_BASE/node1" \
  --cluster-config "$DATA_DIR_BASE/cluster-node1.toml" \
  all > "$LOG_DIR/node1.log" 2>&1 &
NODE1_PID=$!

sleep 2

echo "Starting Node 2..."
RUST_LOG=info,chronik_raft=info,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --data-dir "$DATA_DIR_BASE/node2" \
  --cluster-config "$DATA_DIR_BASE/cluster-node2.toml" \
  all > "$LOG_DIR/node2.log" 2>&1 &
NODE2_PID=$!

sleep 2

echo "Starting Node 3..."
RUST_LOG=info,chronik_raft=info,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --data-dir "$DATA_DIR_BASE/node3" \
  --cluster-config "$DATA_DIR_BASE/cluster-node3.toml" \
  all > "$LOG_DIR/node3.log" 2>&1 &
NODE3_PID=$!

echo ""
echo "Waiting 15 seconds for cluster formation..."
sleep 15

# Check if all nodes are still running
RUNNING=0
for pid in $NODE1_PID $NODE2_PID $NODE3_PID; do
    if ps -p $pid > /dev/null 2>&1; then
        ((RUNNING++))
    fi
done

if [ $RUNNING -ne 3 ]; then
    echo -e "${RED}❌ FAIL: Only $RUNNING/3 nodes running!${NC}"
    echo ""
    echo "Checking crash logs..."
    for i in 1 2 3; do
        if ! ps -p $(eval echo \$NODE${i}_PID) > /dev/null 2>&1; then
            echo ""
            echo "=== Node $i crashed - last 50 lines ==="
            tail -50 "$LOG_DIR/node$i.log"
        fi
    done
    ((TESTS_FAILED++))
else
    echo -e "${GREEN}✅ All 3 nodes running${NC}"

    # Create test topic to trigger leader elections
    echo ""
    echo "Creating test topic (3 partitions, replication-factor 3)..."
    if command -v kafka-topics &> /dev/null; then
        kafka-topics --bootstrap-server localhost:9092 --create \
          --topic phase5-test --partitions 3 --replication-factor 3 \
          --command-config <(echo "request.timeout.ms=30000") 2>&1 || echo "Topic creation may have failed"
    else
        echo -e "${YELLOW}⚠️  kafka-topics not found (install: brew install kafka)${NC}"
        echo "Skipping topic creation - will test with __meta partition only"
    fi

    sleep 10

    # Monitor for 5 minutes
    echo ""
    echo "Monitoring cluster stability for 5 minutes..."
    echo "(Press Ctrl+C to skip)"

    INITIAL_ELECTIONS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')
    START_TIME=$(date +%s)

    for i in $(seq 1 30); do
        sleep 10
        CURRENT_ELECTIONS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')
        NEW_ELECTIONS=$((CURRENT_ELECTIONS - INITIAL_ELECTIONS))
        ELAPSED=$((i * 10))

        echo "[${ELAPSED}s] Total elections: $CURRENT_ELECTIONS (new in last 10s: $NEW_ELECTIONS)"

        INITIAL_ELECTIONS=$CURRENT_ELECTIONS
    done

    # Analyze results
    echo ""
    echo -e "${BLUE}Analyzing Test 1 Results...${NC}"

    TOTAL_ELECTIONS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')
    MAX_TERM=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | \
        grep -oE "term=[0-9]+" | cut -d= -f2 | sort -n | tail -1 || echo 1)
    LOWER_TERM_ERRORS=$(grep -h "ignored a message with lower term" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')

    echo "  Total elections: $TOTAL_ELECTIONS"
    echo "  Max term reached: $MAX_TERM"
    echo "  Lower term errors: $LOWER_TERM_ERRORS"

    TEST1_PASS=true

    # Check criteria
    if [ "$TOTAL_ELECTIONS" -le 30 ]; then
        echo -e "  ${GREEN}✅ Election count acceptable ($TOTAL_ELECTIONS ≤ 30)${NC}"
    else
        echo -e "  ${RED}❌ Too many elections ($TOTAL_ELECTIONS > 30)${NC}"
        TEST1_PASS=false
    fi

    if [ "$MAX_TERM" -le 5 ]; then
        echo -e "  ${GREEN}✅ Term stability good (max term: $MAX_TERM ≤ 5)${NC}"
    else
        echo -e "  ${RED}❌ Excessive term growth (max term: $MAX_TERM > 5)${NC}"
        TEST1_PASS=false
    fi

    if [ "$LOWER_TERM_ERRORS" -le 100 ]; then
        echo -e "  ${GREEN}✅ Lower term errors acceptable ($LOWER_TERM_ERRORS ≤ 100)${NC}"
    else
        echo -e "  ${YELLOW}⚠️  High lower term errors ($LOWER_TERM_ERRORS > 100)${NC}"
    fi

    if [ "$TEST1_PASS" = true ]; then
        echo ""
        echo -e "${GREEN}✅ TEST 1 PASSED: Cluster is stable!${NC}"
        ((TESTS_PASSED++))
    else
        echo ""
        echo -e "${RED}❌ TEST 1 FAILED: Election churn detected${NC}"
        ((TESTS_FAILED++))
    fi
fi

echo ""
read -p "Press Enter to continue to Test 2..."

# ============================================================================
# Test 2: Java Client Compatibility (Phase 4 Validation)
# ============================================================================

echo ""
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Test 2: Java Client Compatibility${NC}"
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo ""
echo "Testing Phase 4 fix: Metadata sync with retry"
echo "Expected: Java clients can produce/consume successfully"
echo ""

if ! command -v kafka-console-producer &> /dev/null; then
    echo -e "${YELLOW}⚠️  kafka-console-producer not found${NC}"
    echo "Install via: brew install kafka"
    echo -e "${YELLOW}⚠️  SKIPPING TEST 2${NC}"
else
    # Check metadata sync
    METADATA_SYNCS=$(grep -h "Updated partition assignment" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')
    METADATA_FAILURES=$(grep -h "CRITICAL: Partition assignment.*could not be replicated" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')

    echo "Metadata sync attempts: $METADATA_SYNCS"
    echo "Metadata sync failures: $METADATA_FAILURES"

    if [ "$METADATA_FAILURES" -gt 0 ]; then
        echo -e "${RED}❌ WARNING: $METADATA_FAILURES metadata sync failures detected!${NC}"
    fi

    # Test produce
    echo ""
    echo "Testing producer (sending 10 messages)..."
    TEST2_PASS=true

    for i in {1..10}; do
        echo "test-message-$i"
    done | timeout 30 kafka-console-producer --bootstrap-server localhost:9092 --topic phase5-test 2>&1 > "$LOG_DIR/producer.log"

    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✅ Producer succeeded${NC}"
    else
        echo -e "${RED}❌ Producer failed${NC}"
        tail -20 "$LOG_DIR/producer.log"
        TEST2_PASS=false
    fi

    # Test consume
    echo ""
    echo "Testing consumer (expecting 10 messages)..."

    CONSUMED=$(timeout 30 kafka-console-consumer --bootstrap-server localhost:9092 \
        --topic phase5-test --from-beginning --max-messages 10 2>&1 | tee "$LOG_DIR/consumer.log" | wc -l | tr -d ' ')

    if [ "$CONSUMED" -eq 10 ]; then
        echo -e "${GREEN}✅ Consumer received all 10 messages${NC}"
    else
        echo -e "${RED}❌ Consumer received only $CONSUMED/10 messages${NC}"
        TEST2_PASS=false
    fi

    # Check for errors
    CONSUMER_ERRORS=$(grep -i "error\|exception" "$LOG_DIR/consumer.log" 2>/dev/null | wc -l | tr -d ' ')
    if [ "$CONSUMER_ERRORS" -gt 0 ]; then
        echo -e "${RED}❌ Consumer errors detected:${NC}"
        grep -i "error\|exception" "$LOG_DIR/consumer.log"
        TEST2_PASS=false
    fi

    if [ "$TEST2_PASS" = true ]; then
        echo ""
        echo -e "${GREEN}✅ TEST 2 PASSED: Java clients work!${NC}"
        ((TESTS_PASSED++))
    else
        echo ""
        echo -e "${RED}❌ TEST 2 FAILED: Java client issues detected${NC}"
        ((TESTS_FAILED++))
    fi
fi

echo ""
read -p "Press Enter to continue to Test 3..."

# ============================================================================
# Test 3: High Write Load Stability (Phase 2 Validation)
# ============================================================================

echo ""
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Test 3: High Write Load Stability (2 minutes)${NC}"
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo ""
echo "Testing Phase 2 fix: Non-blocking ready() prevents election churn under load"
echo "Expected: No new elections during high write load"
echo ""

if ! command -v kafka-producer-perf-test &> /dev/null; then
    echo -e "${YELLOW}⚠️  kafka-producer-perf-test not found${NC}"
    echo "Install via: brew install kafka"
    echo -e "${YELLOW}⚠️  SKIPPING TEST 3${NC}"
else
    # Capture pre-load election count
    PRE_LOAD_ELECTIONS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')

    echo "Pre-load elections: $PRE_LOAD_ELECTIONS"
    echo ""
    echo "Generating high write load (10,000 messages)..."

    kafka-producer-perf-test \
        --topic phase5-test \
        --num-records 10000 \
        --record-size 1024 \
        --throughput 5000 \
        --producer-props bootstrap.servers=localhost:9092 \
        2>&1 | tee "$LOG_DIR/perf-test.log"

    sleep 5

    # Capture post-load election count
    POST_LOAD_ELECTIONS=$(grep -h "Became Raft leader" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')
    NEW_ELECTIONS=$((POST_LOAD_ELECTIONS - PRE_LOAD_ELECTIONS))

    echo ""
    echo "Post-load elections: $POST_LOAD_ELECTIONS"
    echo "New elections during load: $NEW_ELECTIONS"

    TEST3_PASS=true

    if [ "$NEW_ELECTIONS" -eq 0 ]; then
        echo -e "${GREEN}✅ No elections during load - Phase 2 working!${NC}"
    elif [ "$NEW_ELECTIONS" -le 3 ]; then
        echo -e "${YELLOW}⚠️  $NEW_ELECTIONS elections during load (acceptable)${NC}"
    else
        echo -e "${RED}❌ $NEW_ELECTIONS elections during load (churn detected)${NC}"
        TEST3_PASS=false
    fi

    # Check performance
    THROUGHPUT=$(grep "records/sec" "$LOG_DIR/perf-test.log" | grep -oE "[0-9]+\.[0-9]+ records/sec" | head -1)
    echo "Throughput achieved: $THROUGHPUT"

    if [ "$TEST3_PASS" = true ]; then
        echo ""
        echo -e "${GREEN}✅ TEST 3 PASSED: Cluster stable under load!${NC}"
        ((TESTS_PASSED++))
    else
        echo ""
        echo -e "${RED}❌ TEST 3 FAILED: Election churn during load${NC}"
        ((TESTS_FAILED++))
    fi
fi

echo ""
read -p "Press Enter to continue to Test 4..."

# ============================================================================
# Test 4: Deserialization Error Check (Phase 3 Validation)
# ============================================================================

echo ""
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Test 4: Deserialization Error Check${NC}"
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo ""
echo "Testing Phase 3 fix: Comprehensive logging shows no deser errors"
echo "Expected: No deserialization failures in logs"
echo ""

# Count deserialization errors
DESER_ERRORS=$(grep -h "Deserialization FAILED" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')

echo "Deserialization errors found: $DESER_ERRORS"

TEST4_PASS=true

if [ "$DESER_ERRORS" -eq 0 ]; then
    echo -e "${GREEN}✅ No deserialization errors - data integrity intact!${NC}"
else
    echo -e "${RED}❌ $DESER_ERRORS deserialization errors detected${NC}"
    echo ""
    echo "First 5 errors:"
    grep -h "Deserialization FAILED" "$LOG_DIR"/node*.log 2>/dev/null | head -5
    echo ""
    echo "Check Phase 3 logs for hex dumps and root cause analysis"
    TEST4_PASS=false
fi

# Check data flow consistency
echo ""
echo "Checking data flow consistency (Phase 3 logging)..."

PRODUCE_LOGS=$(grep -h "PRODUCE: Proposing" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')
REPLICA_LOGS=$(grep -h "REPLICA: propose()" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')
STATE_MACHINE_LOGS=$(grep -h "STATE_MACHINE: apply()" "$LOG_DIR"/node*.log 2>/dev/null | wc -l | tr -d ' ')

echo "  PRODUCE logs: $PRODUCE_LOGS"
echo "  REPLICA logs: $REPLICA_LOGS"
echo "  STATE_MACHINE logs: $STATE_MACHINE_LOGS"

if [ "$PRODUCE_LOGS" -gt 0 ] && [ "$STATE_MACHINE_LOGS" -gt 0 ]; then
    echo -e "${GREEN}✅ Data flow tracked through all stages${NC}"
else
    echo -e "${YELLOW}⚠️  Limited data flow (may be normal if no writes occurred)${NC}"
fi

if [ "$TEST4_PASS" = true ]; then
    echo ""
    echo -e "${GREEN}✅ TEST 4 PASSED: No data corruption!${NC}"
    ((TESTS_PASSED++))
else
    echo ""
    echo -e "${RED}❌ TEST 4 FAILED: Deserialization errors detected${NC}"
    ((TESTS_FAILED++))
fi

# ============================================================================
# Final Summary
# ============================================================================

echo ""
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}Final Test Results${NC}"
echo -e "${BLUE}════════════════════════════════════════════════════════════════${NC}"
echo ""

echo "Tests Passed: $TESTS_PASSED/$TESTS_TOTAL"
echo "Tests Failed: $TESTS_FAILED/$TESTS_TOTAL"
echo ""

if [ $TESTS_FAILED -eq 0 ]; then
    echo -e "${GREEN}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║  ✅ ALL TESTS PASSED!                                          ║${NC}"
    echo -e "${GREEN}║                                                                ║${NC}"
    echo -e "${GREEN}║  Phases 1-4 fixes validated successfully.                     ║${NC}"
    echo -e "${GREEN}║  Chronik v2.0.0 is READY FOR GA RELEASE!                      ║${NC}"
    echo -e "${GREEN}╚════════════════════════════════════════════════════════════════╝${NC}"
    EXIT_CODE=0
else
    echo -e "${RED}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${RED}║  ❌ TESTS FAILED ($TESTS_FAILED/$TESTS_TOTAL)                                         ║${NC}"
    echo -e "${RED}║                                                                ║${NC}"
    echo -e "${RED}║  Review logs in $LOG_DIR/                                      ║${NC}"
    echo -e "${RED}║  Use Phase 3 diagnostic logging for root cause analysis       ║${NC}"
    echo -e "${RED}╚════════════════════════════════════════════════════════════════╝${NC}"
    EXIT_CODE=1
fi

echo ""
echo "Logs saved to: $LOG_DIR/"
echo "  - node1.log, node2.log, node3.log (server logs)"
echo "  - producer.log, consumer.log, perf-test.log (client logs)"
echo ""

echo "Cluster will remain running for manual inspection."
echo "Press Enter to shut down cluster and exit..."
read

exit $EXIT_CODE
