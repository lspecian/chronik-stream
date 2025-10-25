#!/bin/bash
# Phase 5: Simple Raft Stability Test - ACTUALLY WORKS
# Uses correct raft-cluster subcommand

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║  Phase 5: Raft Stability Test (2 minutes)                     ║${NC}"
echo -e "${BLUE}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""

cleanup() {
    echo ""
    echo -e "${YELLOW}Cleaning up...${NC}"
    pkill -f "chronik-server.*raft-cluster" || true
    sleep 2
}

trap cleanup EXIT

# Clean up any existing processes
cleanup

# Clean old data
rm -rf ./test_data ./test_logs
mkdir -p ./test_logs

echo "Starting 3-node Raft cluster..."
echo ""

# Node 1
echo "Starting Node 1 (port 9092, Raft 9192)..."
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --data-dir ./test_data/node1 \
  --node-id 1 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap \
  > ./test_logs/node1.log 2>&1 &
NODE1_PID=$!
echo "  Node 1 PID: $NODE1_PID"

sleep 3

# Node 2
echo "Starting Node 2 (port 9093, Raft 9193)..."
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --data-dir ./test_data/node2 \
  --node-id 2 \
  raft-cluster \
  --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" \
  > ./test_logs/node2.log 2>&1 &
NODE2_PID=$!
echo "  Node 2 PID: $NODE2_PID"

sleep 3

# Node 3
echo "Starting Node 3 (port 9094, Raft 9194)..."
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --data-dir ./test_data/node3 \
  --node-id 3 \
  raft-cluster \
  --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" \
  > ./test_logs/node3.log 2>&1 &
NODE3_PID=$!
echo "  Node 3 PID: $NODE3_PID"

echo ""
echo "Waiting 15 seconds for cluster formation..."
sleep 15

# Check if all nodes are running
RUNNING=0
for pid in $NODE1_PID $NODE2_PID $NODE3_PID; do
    if ps -p $pid > /dev/null 2>&1; then
        ((RUNNING++))
    fi
done

if [ $RUNNING -ne 3 ]; then
    echo -e "${RED}❌ FAIL: Only $RUNNING/3 nodes running${NC}"
    echo ""
    for i in 1 2 3; do
        pid_var="NODE${i}_PID"
        if ! ps -p ${!pid_var} > /dev/null 2>&1; then
            echo "=== Node $i crashed - last 30 lines ==="
            tail -30 ./test_logs/node$i.log
            echo ""
        fi
    done
    exit 1
fi

echo -e "${GREEN}✅ All 3 nodes running${NC}"
echo ""

# Monitor for 2 minutes
echo "Monitoring cluster for 2 minutes..."
INITIAL_ELECTIONS=$(grep -h "Became Raft leader" ./test_logs/node*.log 2>/dev/null | wc -l | tr -d ' ')
echo "Initial elections: $INITIAL_ELECTIONS"
echo ""

for i in {1..12}; do
    sleep 10
    CURRENT=$(grep -h "Became Raft leader" ./test_logs/node*.log 2>/dev/null | wc -l | tr -d ' ')
    NEW=$((CURRENT - INITIAL_ELECTIONS))
    MAX_TERM=$(grep -h "Became Raft leader" ./test_logs/node*.log 2>/dev/null | \
        grep -oE "term=[0-9]+" | cut -d= -f2 | sort -n | tail -1 || echo 1)

    echo "[${i}0s] Elections: $CURRENT (new: $NEW) | Max term: $MAX_TERM"
    INITIAL_ELECTIONS=$CURRENT
done

echo ""
echo -e "${BLUE}Results:${NC}"

TOTAL=$(grep -h "Became Raft leader" ./test_logs/node*.log 2>/dev/null | wc -l | tr -d ' ')
MAX_TERM=$(grep -h "Became Raft leader" ./test_logs/node*.log 2>/dev/null | \
    grep -oE "term=[0-9]+" | cut -d= -f2 | sort -n | tail -1 || echo 1)
LOWER_TERM=$(grep -h "ignored a message with lower term" ./test_logs/node*.log 2>/dev/null | wc -l | tr -d ' ')

echo "  Total elections: $TOTAL"
echo "  Max term: $MAX_TERM"
echo "  Lower term errors: $LOWER_TERM"
echo ""

PASS=true

if [ "$TOTAL" -le 30 ]; then
    echo -e "  ${GREEN}✅ Election count OK ($TOTAL ≤ 30)${NC}"
else
    echo -e "  ${RED}❌ Too many elections ($TOTAL > 30)${NC}"
    PASS=false
fi

if [ "$MAX_TERM" -le 5 ]; then
    echo -e "  ${GREEN}✅ Term stability OK (max: $MAX_TERM ≤ 5)${NC}"
else
    echo -e "  ${RED}❌ Term growth detected (max: $MAX_TERM > 5)${NC}"
    PASS=false
fi

if [ "$LOWER_TERM" -le 100 ]; then
    echo -e "  ${GREEN}✅ Lower term errors OK ($LOWER_TERM ≤ 100)${NC}"
else
    echo -e "  ${YELLOW}⚠️  High lower term errors ($LOWER_TERM > 100)${NC}"
fi

echo ""
if [ "$PASS" = true ]; then
    echo -e "${GREEN}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║  ✅ TEST PASSED - Phases 1-4 fixes working!                    ║${NC}"
    echo -e "${GREEN}╚════════════════════════════════════════════════════════════════╝${NC}"
    EXIT_CODE=0
else
    echo -e "${RED}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${RED}║  ❌ TEST FAILED - Review logs                                   ║${NC}"
    echo -e "${RED}╚════════════════════════════════════════════════════════════════╝${NC}"
    EXIT_CODE=1
fi

echo ""
echo "Logs: ./test_logs/node{1,2,3}.log"
echo ""
echo "Cluster will remain running. Press Enter to shut down..."
read

exit $EXIT_CODE
