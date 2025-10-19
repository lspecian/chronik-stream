#!/bin/bash
# Start 3-node Chronik Raft cluster in background
# Usage: ./scripts/start_3node_cluster.sh [--clean]

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

# Clean data directories if requested
if $CLEAN_DATA; then
    echo "Cleaning data directories..."
    rm -rf data/node1 data/node2 data/node3
    rm -f /tmp/chronik-node*.pid
fi

# Build the server binary
echo "Building chronik-server..."
cargo build --release --bin chronik-server

# Check if config files exist
CONFIG_DIR=".conductor/lahore"
if [[ ! -f "$CONFIG_DIR/chronik-cluster-node1.toml" ]]; then
    echo "ERROR: Config files not found in $CONFIG_DIR/"
    echo "Expected: chronik-cluster-node1.toml, chronik-cluster-node2.toml, chronik-cluster-node3.toml"
    exit 1
fi

# Start Node 1 (port 9092)
echo "Starting Node 1 (port 9092)..."
CHRONIK_DATA_DIR=data/node1 \
CHRONIK_KAFKA_PORT=9092 \
CHRONIK_ADVERTISED_ADDR=localhost \
./target/release/chronik-server \
    --config "$CONFIG_DIR/chronik-cluster-node1.toml" \
    standalone \
    > /tmp/chronik-node1.log 2>&1 &
echo $! > /tmp/chronik-node1.pid
echo "Node 1 PID: $(cat /tmp/chronik-node1.pid)"

# Start Node 2 (port 9093)
echo "Starting Node 2 (port 9093)..."
CHRONIK_DATA_DIR=data/node2 \
CHRONIK_KAFKA_PORT=9093 \
CHRONIK_ADVERTISED_ADDR=localhost \
./target/release/chronik-server \
    --config "$CONFIG_DIR/chronik-cluster-node2.toml" \
    standalone \
    > /tmp/chronik-node2.log 2>&1 &
echo $! > /tmp/chronik-node2.pid
echo "Node 2 PID: $(cat /tmp/chronik-node2.pid)"

# Start Node 3 (port 9094)
echo "Starting Node 3 (port 9094)..."
CHRONIK_DATA_DIR=data/node3 \
CHRONIK_KAFKA_PORT=9094 \
CHRONIK_ADVERTISED_ADDR=localhost \
./target/release/chronik-server \
    --config "$CONFIG_DIR/chronik-cluster-node3.toml" \
    standalone \
    > /tmp/chronik-node3.log 2>&1 &
echo $! > /tmp/chronik-node3.pid
echo "Node 3 PID: $(cat /tmp/chronik-node3.pid)"

echo ""
echo "Waiting 10 seconds for cluster formation..."
sleep 10

# Check if all nodes are still running
FAILED=false
for node_id in 1 2 3; do
    if [[ -f "/tmp/chronik-node${node_id}.pid" ]]; then
        pid=$(cat /tmp/chronik-node${node_id}.pid)
        if ! ps -p $pid > /dev/null 2>&1; then
            echo "ERROR: Node $node_id (PID $pid) failed to start!"
            echo "Check logs at /tmp/chronik-node${node_id}.log"
            FAILED=true
        else
            echo "Node $node_id (PID $pid) is running"
        fi
    fi
done

if $FAILED; then
    echo ""
    echo "Cluster startup FAILED. Shutting down..."
    "$SCRIPT_DIR/stop_cluster.sh"
    exit 1
fi

echo ""
echo "3-node cluster started successfully!"
echo "Nodes:"
echo "  - Node 1: localhost:9092 (PID $(cat /tmp/chronik-node1.pid))"
echo "  - Node 2: localhost:9093 (PID $(cat /tmp/chronik-node2.pid))"
echo "  - Node 3: localhost:9094 (PID $(cat /tmp/chronik-node3.pid))"
echo ""
echo "Logs:"
echo "  - tail -f /tmp/chronik-node1.log"
echo "  - tail -f /tmp/chronik-node2.log"
echo "  - tail -f /tmp/chronik-node3.log"
echo ""
echo "Stop cluster: $SCRIPT_DIR/stop_cluster.sh"
echo "Check health: $SCRIPT_DIR/check_cluster_health.sh"
