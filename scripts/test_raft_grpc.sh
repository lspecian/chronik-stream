#!/bin/bash
# Test 3-node Raft cluster with gRPC transport
# This script verifies that nodes can elect a leader via gRPC networking

set -e

echo "======================================="
echo " Raft gRPC Transport Test"
echo "======================================="
echo

# Build server first
echo "[1/4] Building chronik-server..."
/home/ubuntu/.cargo/bin/cargo build --release --bin chronik-server
echo "✓ Build complete"
echo

# Clean up data directories
echo "[2/4] Cleaning data directories..."
rm -rf /tmp/chronik-node1 /tmp/chronik-node2 /tmp/chronik-node3
echo "✓ Clean complete"
echo

# Start nodes
echo "[3/4] Starting 3-node Raft cluster..."

# Node 1
echo "Starting Node 1 (port 9092, Raft 5001)..."
RUST_LOG=info ./target/release/chronik-server \
  --node-id 1 \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --data-dir /tmp/chronik-node1 \
  raft-cluster \
  --raft-addr 0.0.0.0:5001 \
  --peers "2@http://localhost:5002,3@http://localhost:5003" \
  > /tmp/chronik-node1.log 2>&1 &
NODE1_PID=$!
echo "✓ Node 1 started (PID: $NODE1_PID)"

# Node 2
echo "Starting Node 2 (port 9093, Raft 5002)..."
RUST_LOG=info ./target/release/chronik-server \
  --node-id 2 \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --data-dir /tmp/chronik-node2 \
  raft-cluster \
  --raft-addr 0.0.0.0:5002 \
  --peers "1@http://localhost:5001,3@http://localhost:5003" \
  > /tmp/chronik-node2.log 2>&1 &
NODE2_PID=$!
echo "✓ Node 2 started (PID: $NODE2_PID)"

# Node 3
echo "Starting Node 3 (port 9094, Raft 5003)..."
RUST_LOG=info ./target/release/chronik-server \
  --node-id 3 \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --data-dir /tmp/chronik-node3 \
  raft-cluster \
  --raft-addr 0.0.0.0:5003 \
  --peers "1@http://localhost:5001,2@http://localhost:5002" \
  > /tmp/chronik-node3.log 2>&1 &
NODE3_PID=$!
echo "✓ Node 3 started (PID: $NODE3_PID)"

echo
echo "Waiting 10 seconds for leader election..."
sleep 10
echo

# Check for leader election
echo "[4/4] Checking leader election..."
echo

LEADER_FOUND=0

for node in 1 2 3; do
    LOG_FILE="/tmp/chronik-node$node.log"

    if grep -q "became leader" "$LOG_FILE"; then
        echo "✅ Node $node: ELECTED AS LEADER"
        LEADER_FOUND=1

        # Show evidence
        echo "Evidence:"
        grep -E "(became leader|became candidate)" "$LOG_FILE" | tail -3 | sed 's/^/  /'
    elif grep -q "became follower" "$LOG_FILE"; then
        echo "✅ Node $node: Follower"
    elif grep -q "became candidate" "$LOG_FILE"; then
        echo "⏳ Node $node: Candidate (election in progress)"
        grep "became candidate" "$LOG_FILE" | tail -1 | sed 's/^/  /'
    else
        echo "❌ Node $node: Unknown state"
    fi
    echo
done

# Show gRPC activity
echo "gRPC Activity:"
for node in 1 2 3; do
    LOG_FILE="/tmp/chronik-node$node.log"
    GRPC_MSGS=$(grep -c "Sent Raft message to peer.*via gRPC" "$LOG_FILE" || echo "0")
    echo "  Node $node: $GRPC_MSGS gRPC messages sent"
done
echo

# Cleanup
echo "======================================="
echo " Cleanup"
echo "======================================="
echo "Stopping nodes..."
kill $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
echo "✓ Nodes stopped"
echo

if [ $LEADER_FOUND -eq 1 ]; then
    echo "🎉 SUCCESS: Leader elected via gRPC!"
    echo "Logs available in:"
    echo "  - /tmp/chronik-node1.log"
    echo "  - /tmp/chronik-node2.log"
    echo "  - /tmp/chronik-node3.log"
    exit 0
else
    echo "❌ FAILURE: No leader elected"
    echo "Check logs for errors:"
    echo "  tail /tmp/chronik-node1.log"
    echo "  tail /tmp/chronik-node2.log"
    echo "  tail /tmp/chronik-node3.log"
    exit 1
fi
