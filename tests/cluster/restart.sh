#!/bin/bash
# Restart cluster WITHOUT cleaning data - for testing cold-start recovery
# This script is intentionally separate from start.sh to avoid accidents

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
echo -e "${GREEN}Chronik Cluster RESTART (preserving data)${NC}"
echo -e "${GREEN}========================================${NC}"
echo

# Check if binary exists
if [ ! -f "$BINARY" ]; then
    echo -e "${RED}ERROR: Binary not found at $BINARY${NC}"
    echo "Run: cargo build --release --bin chronik-server"
    exit 1
fi

# Stop if running
if pgrep -f "chronik-server.*start.*--config.*node[123].toml" > /dev/null; then
    echo -e "${YELLOW}Stopping existing cluster...${NC}"
    "$SCRIPT_DIR/stop.sh"
    sleep 2
fi

# Verify data directories exist
if [ ! -d "$SCRIPT_DIR/data/node1" ]; then
    echo -e "${RED}ERROR: Data directories don't exist. Run start.sh first to create initial data.${NC}"
    exit 1
fi

# Create logs directory if needed
mkdir -p "$SCRIPT_DIR"/logs

echo -e "${YELLOW}NOTE: Preserving existing data - testing cold-start recovery${NC}"
echo

# Start nodes
echo -e "${GREEN}Starting Node 1...${NC}"
RUST_LOG=info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn "$BINARY" start --config "$SCRIPT_DIR/node1.toml" \
    > "$SCRIPT_DIR/logs/node1.log" 2>&1 &
echo $! > "$SCRIPT_DIR/data/node1.pid"

sleep 2

echo -e "${GREEN}Starting Node 2...${NC}"
RUST_LOG=info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn "$BINARY" start --config "$SCRIPT_DIR/node2.toml" \
    > "$SCRIPT_DIR/logs/node2.log" 2>&1 &
echo $! > "$SCRIPT_DIR/data/node2.pid"

sleep 2

echo -e "${GREEN}Starting Node 3...${NC}"
RUST_LOG=info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn "$BINARY" start --config "$SCRIPT_DIR/node3.toml" \
    > "$SCRIPT_DIR/logs/node3.log" 2>&1 &
echo $! > "$SCRIPT_DIR/data/node3.pid"

sleep 3

# Check if nodes are running
echo
echo -e "${GREEN}Checking cluster status...${NC}"
if pgrep -f "chronik-server.*start.*--config.*node1.toml" > /dev/null; then
    echo -e "  Node 1: ${GREEN}Running${NC} (port 9092)"
else
    echo -e "  Node 1: ${RED}FAILED${NC}"
fi

if pgrep -f "chronik-server.*start.*--config.*node2.toml" > /dev/null; then
    echo -e "  Node 2: ${GREEN}Running${NC} (port 9093)"
else
    echo -e "  Node 2: ${RED}FAILED${NC}"
fi

if pgrep -f "chronik-server.*start.*--config.*node3.toml" > /dev/null; then
    echo -e "  Node 3: ${GREEN}Running${NC} (port 9094)"
else
    echo -e "  Node 3: ${RED}FAILED${NC}"
fi

echo
echo -e "${GREEN}Cluster restarted (data preserved)${NC}"
echo "Logs: $SCRIPT_DIR/logs/"
echo "Data: $SCRIPT_DIR/data/"
