#!/bin/bash
# Test script to verify Raft cluster stability after fixes

set -e

echo "=== Raft Cluster Stability Test ==="
echo ""

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Build the server
echo -e "${YELLOW}Step 1: Building chronik-server...${NC}"
cargo build --release --bin chronik-server
echo -e "${GREEN}✓ Build complete${NC}"
echo ""

# Create cluster config
echo -e "${YELLOW}Step 2: Creating cluster configuration...${NC}"
cat > chronik-cluster.toml << 'EOF'
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "127.0.0.1:9092"
raft_port = 5001

[[peers]]
id = 2
addr = "127.0.0.1:9093"
raft_port = 5002

[[peers]]
id = 3
addr = "127.0.0.1:9094"
raft_port = 5003
EOF
echo -e "${GREEN}✓ Configuration created${NC}"
echo ""

# Create data directories
echo -e "${YELLOW}Step 3: Setting up data directories...${NC}"
rm -rf ./data/node{1,2,3}
mkdir -p ./data/node{1,2,3}
echo -e "${GREEN}✓ Directories ready${NC}"
echo ""

# Start node 1
echo -e "${YELLOW}Step 4: Starting node 1 (port 9092, Raft 5001)...${NC}"
CHRONIK_NODE_ID=1 \
CHRONIK_DATA_DIR=./data/node1 \
CHRONIK_KAFKA_PORT=9092 \
CHRONIK_ADVERTISED_ADDR=127.0.0.1 \
CHRONIK_ADVERTISED_PORT=9092 \
RUST_LOG=info,chronik_raft=debug,chronik_server::raft_cluster=debug \
./target/release/chronik-server \
    --cluster-config chronik-cluster.toml \
    standalone > node1.log 2>&1 &
NODE1_PID=$!
echo -e "${GREEN}✓ Node 1 started (PID: $NODE1_PID)${NC}"

# Start node 2
echo -e "${YELLOW}Step 5: Starting node 2 (port 9093, Raft 5002)...${NC}"
# Update config for node 2
sed 's/node_id = 1/node_id = 2/' chronik-cluster.toml > chronik-cluster-node2.toml
CHRONIK_NODE_ID=2 \
CHRONIK_DATA_DIR=./data/node2 \
CHRONIK_KAFKA_PORT=9093 \
CHRONIK_ADVERTISED_ADDR=127.0.0.1 \
CHRONIK_ADVERTISED_PORT=9093 \
RUST_LOG=info,chronik_raft=debug,chronik_server::raft_cluster=debug \
./target/release/chronik-server \
    --cluster-config chronik-cluster-node2.toml \
    standalone > node2.log 2>&1 &
NODE2_PID=$!
echo -e "${GREEN}✓ Node 2 started (PID: $NODE2_PID)${NC}"

# Start node 3
echo -e "${YELLOW}Step 6: Starting node 3 (port 9094, Raft 5003)...${NC}"
# Update config for node 3
sed 's/node_id = 1/node_id = 3/' chronik-cluster.toml > chronik-cluster-node3.toml
CHRONIK_NODE_ID=3 \
CHRONIK_DATA_DIR=./data/node3 \
CHRONIK_KAFKA_PORT=9094 \
CHRONIK_ADVERTISED_ADDR=127.0.0.1 \
CHRONIK_ADVERTISED_PORT=9094 \
RUST_LOG=info,chronik_raft=debug,chronik_server::raft_cluster=debug \
./target/release/chronik-server \
    --cluster-config chronik-cluster-node3.toml \
    standalone > node3.log 2>&1 &
NODE3_PID=$!
echo -e "${GREEN}✓ Node 3 started (PID: $NODE3_PID)${NC}"
echo ""

# Cleanup function
cleanup() {
    echo ""
    echo -e "${YELLOW}Cleaning up...${NC}"
    kill $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
    wait $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
    echo -e "${GREEN}✓ All nodes stopped${NC}"
}
trap cleanup EXIT INT TERM

# Wait for cluster to stabilize
echo -e "${YELLOW}Step 7: Waiting for cluster bootstrap (15 seconds)...${NC}"
sleep 15
echo -e "${GREEN}✓ Cluster should be ready${NC}"
echo ""

# Check for connection stability
echo -e "${YELLOW}Step 8: Checking connection stability...${NC}"
echo "Monitoring logs for 30 seconds..."
echo ""

# Monitor for issues
ERRORS_FOUND=0
sleep 5

# Check for connection errors
if grep -q "tcp connect error" node*.log 2>/dev/null; then
    echo -e "${RED}✗ Connection errors detected!${NC}"
    ERRORS_FOUND=1
else
    echo -e "${GREEN}✓ No connection errors${NC}"
fi

# Check for leader election
if grep -q "Became Raft leader" node*.log 2>/dev/null; then
    echo -e "${GREEN}✓ Leader elected${NC}"
    LEADER_NODE=$(grep "Became Raft leader" node*.log | head -1 | awk -F: '{print $1}')
    echo "  Leader: $LEADER_NODE"
else
    echo -e "${RED}✗ No leader elected${NC}"
    ERRORS_FOUND=1
fi

# Check for committed entries
if grep -q "Committed entries ready" node*.log 2>/dev/null; then
    echo -e "${GREEN}✓ Entries being committed${NC}"
    COMMIT_COUNT=$(grep -c "Committed entries ready" node*.log 2>/dev/null || echo "0")
    echo "  Commit count: $COMMIT_COUNT"
else
    echo -e "${YELLOW}⚠ No committed entries yet (may be normal if no operations)${NC}"
fi

# Check for ready() processing
if grep -q "ready() EXTRACTING" node*.log 2>/dev/null; then
    echo -e "${GREEN}✓ Raft ready() processing active${NC}"
else
    echo -e "${RED}✗ Raft ready() not processing${NC}"
    ERRORS_FOUND=1
fi

# Wait and monitor for leadership stability
echo ""
echo -e "${YELLOW}Step 9: Monitoring leadership stability (30 seconds)...${NC}"
INITIAL_TERM=$(grep "current_term" node1.log | tail -1 | awk -F= '{print $NF}' || echo "0")
echo "  Initial term: $INITIAL_TERM"
sleep 30
FINAL_TERM=$(grep "current_term" node1.log | tail -1 | awk -F= '{print $NF}' || echo "0")
echo "  Final term: $FINAL_TERM"

if [ "$INITIAL_TERM" = "$FINAL_TERM" ]; then
    echo -e "${GREEN}✓ Leadership stable (no term changes)${NC}"
else
    echo -e "${RED}✗ Leadership unstable (term changed from $INITIAL_TERM to $FINAL_TERM)${NC}"
    ERRORS_FOUND=1
fi

# Summary
echo ""
echo "=== Test Summary ==="
if [ $ERRORS_FOUND -eq 0 ]; then
    echo -e "${GREEN}✓ All checks passed!${NC}"
    echo ""
    echo "Cluster is stable. Key indicators:"
    echo "  - No connection errors"
    echo "  - Leader elected successfully"
    echo "  - Raft ready() processing active"
    echo "  - Leadership stable (no unnecessary elections)"
    echo ""
    echo "Logs available in: node1.log, node2.log, node3.log"
    echo ""
    echo "To manually inspect:"
    echo "  tail -f node*.log | grep -E 'leader|commit|ready|error'"
    exit 0
else
    echo -e "${RED}✗ Some checks failed${NC}"
    echo ""
    echo "Issues detected. Check logs for details:"
    echo "  tail -100 node*.log"
    echo ""
    exit 1
fi
