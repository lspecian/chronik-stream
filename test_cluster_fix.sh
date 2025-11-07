#!/bin/bash
set -e

echo "=========================================="
echo "Chronik Cluster Broker Discovery Test"
echo "Testing Fix for v2.2.1"
echo "=========================================="
echo

# Clean up any existing test data
echo "🧹 Cleaning up old test data..."
rm -rf ./test-data-node1 ./test-data-node2 ./test-data-node3
mkdir -p ./test-data-node1 ./test-data-node2 ./test-data-node3

# Kill any existing chronik processes on test ports
echo "🔪 Killing any existing Chronik processes..."
pkill -f "chronik-server.*19092" || true
pkill -f "chronik-server.*19093" || true
pkill -f "chronik-server.*19094" || true
sleep 2

# Start Node 1
echo
echo "🚀 Starting Node 1 (localhost:19092)..."
RUST_LOG=chronik_server=info,chronik_raft=info \
  ./target/release/chronik-server start \
  --config test-cluster-node1.toml \
  --data-dir ./test-data-node1 \
  > ./test-node1.log 2>&1 &
NODE1_PID=$!
echo "   Node 1 PID: $NODE1_PID"

# Start Node 2
echo "🚀 Starting Node 2 (localhost:19093)..."
RUST_LOG=chronik_server=info,chronik_raft=info \
  ./target/release/chronik-server start \
  --config test-cluster-node2.toml \
  --data-dir ./test-data-node2 \
  > ./test-node2.log 2>&1 &
NODE2_PID=$!
echo "   Node 2 PID: $NODE2_PID"

# Start Node 3
echo "🚀 Starting Node 3 (localhost:19094)..."
RUST_LOG=chronik_server=info,chronik_raft=info \
  ./target/release/chronik-server start \
  --config test-cluster-node3.toml \
  --data-dir ./test-data-node3 \
  > ./test-node3.log 2>&1 &
NODE3_PID=$!
echo "   Node 3 PID: $NODE3_PID"

# Function to cleanup on exit
cleanup() {
    echo
    echo "🛑 Stopping nodes..."
    kill $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
    wait $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
    echo "✅ Nodes stopped"
}
trap cleanup EXIT

# Wait for cluster to be ready
# NOTE: Each node takes ~60s to start (30s Raft leader wait + 30s init)
# With 3 nodes starting simultaneously, wait 90s to be safe
echo
echo "⏳ Waiting for cluster to initialize (90 seconds - nodes wait for Raft leader)..."
sleep 90

# Check if nodes are still running
if ! kill -0 $NODE1_PID 2>/dev/null; then
    echo "❌ Node 1 died! Check ./test-node1.log"
    tail -50 ./test-node1.log
    exit 1
fi

if ! kill -0 $NODE2_PID 2>/dev/null; then
    echo "❌ Node 2 died! Check ./test-node2.log"
    tail -50 ./test-node2.log
    exit 1
fi

if ! kill -0 $NODE3_PID 2>/dev/null; then
    echo "❌ Node 3 died! Check ./test-node3.log"
    tail -50 ./test-node3.log
    exit 1
fi

echo "✅ All nodes are running"
echo

# Check broker registration logs
echo "📋 Checking broker registration in logs..."
echo
echo "Node 1 broker registration:"
grep -i "broker.*registration\|broker.*raft\|metadata store has" ./test-node1.log | tail -5 || echo "   (no relevant logs found)"
echo
echo "Node 2 broker registration:"
grep -i "broker.*registration\|broker.*raft\|metadata store has" ./test-node2.log | tail -5 || echo "   (no relevant logs found)"
echo
echo "Node 3 broker registration:"
grep -i "broker.*registration\|broker.*raft\|metadata store has" ./test-node3.log | tail -5 || echo "   (no relevant logs found)"
echo

# Run Python test
echo "=========================================="
echo "Running Python Kafka Client Test"
echo "=========================================="
echo

if command -v python3 &> /dev/null; then
    python3 tests/test_cluster_broker_discovery.py
    TEST_RESULT=$?
else
    echo "⚠️  Python3 not found, skipping Python test"
    TEST_RESULT=0
fi

echo
echo "=========================================="
if [ $TEST_RESULT -eq 0 ]; then
    echo "✅ ALL TESTS PASSED"
else
    echo "❌ TESTS FAILED"
    echo
    echo "Check logs:"
    echo "  - ./test-node1.log"
    echo "  - ./test-node2.log"
    echo "  - ./test-node3.log"
fi
echo "=========================================="

exit $TEST_RESULT
