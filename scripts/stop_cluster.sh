#!/bin/bash
# Gracefully shutdown 3-node Chronik Raft cluster
# Usage: ./scripts/stop_cluster.sh [--clean]

set -e

# Navigate to repo root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
cd "$REPO_ROOT"

# Parse arguments
CLEAN_DATA=false
if [[ "$1" == "--clean" ]]; then
    CLEAN_DATA=true
fi

echo "Stopping 3-node Chronik cluster..."
START_TIME=$(date +%s)

# Gracefully stop each node (SIGTERM)
STOPPED_COUNT=0
for node_id in 1 2 3; do
    PID_FILE="/tmp/chronik-node${node_id}.pid"

    if [[ -f "$PID_FILE" ]]; then
        pid=$(cat "$PID_FILE")

        if ps -p $pid > /dev/null 2>&1; then
            echo "Stopping Node $node_id (PID $pid)..."
            kill -TERM $pid

            # Wait up to 10 seconds for graceful shutdown
            for i in {1..10}; do
                if ! ps -p $pid > /dev/null 2>&1; then
                    echo "Node $node_id stopped gracefully"
                    STOPPED_COUNT=$((STOPPED_COUNT + 1))
                    break
                fi
                sleep 1
            done

            # Force kill if still running
            if ps -p $pid > /dev/null 2>&1; then
                echo "WARNING: Node $node_id did not stop gracefully, forcing shutdown..."
                kill -9 $pid
                sleep 1
                STOPPED_COUNT=$((STOPPED_COUNT + 1))
            fi
        else
            echo "Node $node_id (PID $pid) is not running"
        fi

        rm -f "$PID_FILE"
    else
        echo "Node $node_id PID file not found (not running?)"
    fi
done

# Clean data directories if requested
if $CLEAN_DATA; then
    echo ""
    echo "Cleaning data directories..."
    rm -rf data/node1 data/node2 data/node3
    echo "Data directories removed"
fi

# Calculate shutdown time
END_TIME=$(date +%s)
SHUTDOWN_TIME=$((END_TIME - START_TIME))

echo ""
echo "Cluster shutdown complete!"
echo "Stopped nodes: $STOPPED_COUNT/3"
echo "Shutdown time: ${SHUTDOWN_TIME}s"

if $CLEAN_DATA; then
    echo "Data directories cleaned"
fi
