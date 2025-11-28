#!/bin/bash
# DEFINITIVE way to start a local 3-node Chronik cluster for testing
# This is THE ONLY script to use for local cluster testing

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BINARY="$PROJECT_ROOT/target/release/chronik-server"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Chronik Local Test Cluster${NC}"
echo -e "${GREEN}========================================${NC}"
echo

# Check if binary exists
if [ ! -f "$BINARY" ]; then
    echo -e "${RED}ERROR: Binary not found at $BINARY${NC}"
    echo "Run: cargo build --release --bin chronik-server"
    exit 1
fi

# Check if cluster is already running
if pgrep -f "chronik-server.*start.*--config.*node[123].toml" > /dev/null; then
    echo -e "${YELLOW}WARNING: Cluster appears to be already running${NC}"
    echo "Run ./cluster-local/stop.sh first"
    exit 1
fi

# Clean up old data
echo -e "${YELLOW}Cleaning up old data...${NC}"
rm -rf "$SCRIPT_DIR"/data/node{1,2,3}
mkdir -p "$SCRIPT_DIR"/data/node{1,2,3}
mkdir -p "$SCRIPT_DIR"/logs

# Start nodes
# CRITICAL: Use RUST_LOG=info (not debug) to prevent log bomb with many partitions
# v2.2.9 fix #1: debug + ultra + 1000s of partitions = 180K log lines/sec → crash
# v2.2.9 fix #2: Tantivy INFO logs (commits, GC) create 18K logs/commit with 3000 partitions
# v2.2.9 fix #3: Protocol/produce handler logs create 21K logs/metadata request with 3000 partitions
# v2.2.14 fix #4: Connection closed logs at debug = 50MB/sec → disk full
# Use 'high' profile for good performance without excessive logging
echo -e "${GREEN}Starting Node 1...${NC}"
RUST_LOG=info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn CHRONIK_WAL_PROFILE=high "$BINARY" start --config "$SCRIPT_DIR/node1.toml" \
    > "$SCRIPT_DIR/logs/node1.log" 2>&1 &
echo $! > "$SCRIPT_DIR/data/node1.pid"

sleep 2

echo -e "${GREEN}Starting Node 2...${NC}"
RUST_LOG=info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn CHRONIK_WAL_PROFILE=high "$BINARY" start --config "$SCRIPT_DIR/node2.toml" \
    > "$SCRIPT_DIR/logs/node2.log" 2>&1 &
echo $! > "$SCRIPT_DIR/data/node2.pid"

sleep 2

echo -e "${GREEN}Starting Node 3...${NC}"
RUST_LOG=info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn CHRONIK_WAL_PROFILE=high "$BINARY" start --config "$SCRIPT_DIR/node3.toml" \
    > "$SCRIPT_DIR/logs/node3.log" 2>&1 &
echo $! > "$SCRIPT_DIR/data/node3.pid"

sleep 3

# Check if all nodes are running
echo
echo -e "${GREEN}Checking node status...${NC}"
for i in 1 2 3; do
    if [ -f "$SCRIPT_DIR/data/node$i.pid" ]; then
        PID=$(cat "$SCRIPT_DIR/data/node$i.pid")
        if ps -p $PID > /dev/null 2>&1; then
            PORT=$((9091 + i))
            echo -e "${GREEN}✓ Node $i: Running (PID $PID, port $PORT)${NC}"
        else
            echo -e "${RED}✗ Node $i: Failed to start${NC}"
            cat "$SCRIPT_DIR/logs/node$i.log"
        fi
    fi
done

echo
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Cluster started successfully!${NC}"
echo -e "${GREEN}========================================${NC}"
echo
echo "Bootstrap servers: localhost:9092,localhost:9093,localhost:9094"
echo
echo "Logs:"
echo "  Node 1: $SCRIPT_DIR/logs/node1.log"
echo "  Node 2: $SCRIPT_DIR/logs/node2.log"
echo "  Node 3: $SCRIPT_DIR/logs/node3.log"
echo
echo "To stop: ./cluster-local/stop.sh"
echo "To tail logs: tail -f $SCRIPT_DIR/logs/node*.log"
