#!/bin/bash
# DEFINITIVE way to stop the local test cluster

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${YELLOW}Stopping Chronik test cluster...${NC}"

# Stop nodes by PID
for i in 1 2 3; do
    if [ -f "$SCRIPT_DIR/data/node$i.pid" ]; then
        PID=$(cat "$SCRIPT_DIR/data/node$i.pid")
        if ps -p $PID > /dev/null 2>&1; then
            echo -e "${YELLOW}Stopping Node $i (PID $PID)...${NC}"
            kill $PID
            sleep 1
            # Force kill if still running
            if ps -p $PID > /dev/null 2>&1; then
                kill -9 $PID 2>/dev/null || true
            fi
            echo -e "${GREEN}✓ Node $i stopped${NC}"
        fi
        rm -f "$SCRIPT_DIR/data/node$i.pid"
    fi
done

# Cleanup any remaining processes
pkill -f "chronik-server.*start.*--config.*node[123].toml" 2>/dev/null || true

echo -e "${GREEN}Cluster stopped${NC}"
