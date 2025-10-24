#!/bin/bash
# Test script for read-your-writes consistency testing
# Requires:
#   - chronik-server built with: cargo build --release --features raft --bin chronik-server
#   - Config files in tests/cluster-configs/

# Kill any existing chronik-server processes
pkill -9 chronik-server 2>/dev/null

# Get script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Clean data directories
rm -rf "$PROJECT_ROOT/test-cluster-data-ryw"
mkdir -p "$PROJECT_ROOT/test-cluster-data-ryw"/{node1,node2,node3}

# Start node 1
RUST_LOG=info,chronik_server::fetch_handler=debug,chronik_raft::read_index=debug \
CHRONIK_CLUSTER_CONFIG="$SCRIPT_DIR/cluster-configs/test-cluster-ryw-node1.toml" \
CHRONIK_DATA_DIR="$PROJECT_ROOT/test-cluster-data-ryw/node1" \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_KAFKA_PORT=9092 \
CHRONIK_FETCH_FROM_FOLLOWERS=true \
"$PROJECT_ROOT/target/release/chronik-server" standalone > "$PROJECT_ROOT/test-cluster-data-ryw/node1.log" 2>&1 &
NODE1_PID=$!
echo "Node 1 started (PID: $NODE1_PID)"

sleep 2

# Start node 2
RUST_LOG=info,chronik_server::fetch_handler=debug,chronik_raft::read_index=debug \
CHRONIK_CLUSTER_CONFIG="$SCRIPT_DIR/cluster-configs/test-cluster-ryw-node2.toml" \
CHRONIK_DATA_DIR="$PROJECT_ROOT/test-cluster-data-ryw/node2" \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_KAFKA_PORT=9093 \
CHRONIK_FETCH_FROM_FOLLOWERS=true \
"$PROJECT_ROOT/target/release/chronik-server" standalone > "$PROJECT_ROOT/test-cluster-data-ryw/node2.log" 2>&1 &
NODE2_PID=$!
echo "Node 2 started (PID: $NODE2_PID)"

sleep 2

# Start node 3
RUST_LOG=info,chronik_server::fetch_handler=debug,chronik_raft::read_index=debug \
CHRONIK_CLUSTER_CONFIG="$SCRIPT_DIR/cluster-configs/test-cluster-ryw-node3.toml" \
CHRONIK_DATA_DIR="$PROJECT_ROOT/test-cluster-data-ryw/node3" \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_KAFKA_PORT=9094 \
CHRONIK_FETCH_FROM_FOLLOWERS=true \
"$PROJECT_ROOT/target/release/chronik-server" standalone > "$PROJECT_ROOT/test-cluster-data-ryw/node3.log" 2>&1 &
NODE3_PID=$!
echo "Node 3 started (PID: $NODE3_PID)"

sleep 10

# Check status
RUNNING=$(ps aux | grep chronik-server | grep -v grep | wc -l)
echo "Nodes running: $RUNNING/3"

if [ "$RUNNING" -eq 3 ]; then
  echo "✅ All 3 nodes started successfully"
else
  echo "❌ Some nodes failed to start - checking logs..."
  tail -20 "$PROJECT_ROOT/test-cluster-data-ryw/node1.log"
fi
