#!/bin/bash
# Phase 5 Debug Test - Full trace logging to debug vote response issue

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║  Phase 5 Debug Test - Full Trace Logging (30 seconds)         ║${NC}"
echo -e "${BLUE}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""

cleanup() {
    echo ""
    echo -e "${YELLOW}Cleaning up...${NC}"
    pkill -f "chronik-server.*raft-cluster" || true
    sleep 2
}

trap cleanup EXIT

cleanup
rm -rf ./test_data ./test_logs
mkdir -p ./test_logs

echo "Starting 3-node Raft cluster with FULL DEBUG LOGGING..."
echo ""

# Node 1 with full trace logging
RUST_LOG=debug,chronik_raft=trace,chronik_server::raft_integration=trace \
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
echo "Node 1 started (PID: $NODE1_PID)"

sleep 3

# Node 2 with full trace logging
RUST_LOG=debug,chronik_raft=trace,chronik_server::raft_integration=trace \
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --data-dir ./test_data/node2 \
  --node-id 2 \
  raft-cluster \
  --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" \
  --bootstrap \
  > ./test_logs/node2.log 2>&1 &
NODE2_PID=$!
echo "Node 2 started (PID: $NODE2_PID)"

sleep 3

# Node 3 with full trace logging
RUST_LOG=debug,chronik_raft=trace,chronik_server::raft_integration=trace \
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --data-dir ./test_data/node3 \
  --node-id 3 \
  raft-cluster \
  --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" \
  --bootstrap \
  > ./test_logs/node3.log 2>&1 &
NODE3_PID=$!
echo "Node 3 started (PID: $NODE3_PID)"

echo ""
echo "Waiting 10 seconds for cluster formation..."
sleep 10

# Check if all nodes are running
RUNNING=0
ps -p $NODE1_PID > /dev/null 2>&1 && RUNNING=$((RUNNING+1))
ps -p $NODE2_PID > /dev/null 2>&1 && RUNNING=$((RUNNING+1))
ps -p $NODE3_PID > /dev/null 2>&1 && RUNNING=$((RUNNING+1))

if [ "$RUNNING" -ne 3 ]; then
    echo -e "${RED}❌ Only $RUNNING/3 nodes running!${NC}"
    echo ""
    echo "Node 1 last 30 lines:"
    tail -30 ./test_logs/node1.log
    echo ""
    echo "Node 2 last 30 lines:"
    tail -30 ./test_logs/node2.log
    echo ""
    echo "Node 3 last 30 lines:"
    tail -30 ./test_logs/node3.log
    exit 1
fi

echo -e "${GREEN}✅ All 3 nodes running${NC}"
echo ""

# Monitor for 30 seconds and check for leader election
echo "Monitoring for 30 seconds..."
sleep 30

echo ""
echo -e "${BLUE}Analysis:${NC}"
echo ""

# Check for leader election
LEADERS=$(grep -i "became leader\|Became leader" ./test_logs/*.log 2>/dev/null | wc -l | tr -d ' ')
echo "Leaders elected: $LEADERS"

# Check for vote requests
VOTE_REQUESTS=$(grep -i "RequestVote" ./test_logs/node1.log 2>/dev/null | wc -l | tr -d ' ')
echo "Vote requests from node1: $VOTE_REQUESTS"

# Check for vote responses
VOTE_RESPONSES_SENT=$(grep -i "cast vote for" ./test_logs/node2.log 2>/dev/null | wc -l | tr -d ' ')
echo "Vote responses from node2: $VOTE_RESPONSES_SENT"

# Check if node1 received responses from node2
RESPONSES_RECEIVED=$(grep -i "from=2, to=1" ./test_logs/node1.log 2>/dev/null | wc -l | tr -d ' ')
echo "Responses received by node1 from node2: $RESPONSES_RECEIVED"

# Check ready_non_blocking message extraction
echo ""
echo "Checking message extraction in node2:"
grep "ready_non_blocking() EXTRACTING\|Adding.*persisted messages\|Sending.*messages to peers" ./test_logs/node2.log 2>/dev/null | head -20

echo ""
if [ "$LEADERS" -gt 0 ]; then
    echo -e "${GREEN}✅ Leader elected successfully!${NC}"
else
    echo -e "${RED}❌ No leader elected${NC}"
    echo ""
    echo "Vote response problem details:"
    echo "- Node 2 cast $VOTE_RESPONSES_SENT votes"
    echo "- Node 1 received $RESPONSES_RECEIVED responses from node 2"
    echo ""
    echo "Checking node 2's message sending (last 30 matches):"
    grep -i "Sending.*to peer\|send_message.*to=\|Failed to send" ./test_logs/node2.log 2>/dev/null | tail -30
fi

echo ""
echo "Full logs available at:"
echo "  - ./test_logs/node1.log"
echo "  - ./test_logs/node2.log"
echo "  - ./test_logs/node3.log"
echo ""
echo "Press Enter to shut down cluster..."
read

cleanup
