#!/bin/bash
set -e

echo "=== Raft Diagnostic Test ==="
echo "Starting 3-node cluster with enhanced logging..."

# Kill any existing processes
pkill -9 chronik-server || true
sleep 2

# Clean up old data
rm -rf /tmp/chronik-raft-test-*

# Start 3 nodes
echo "Starting node 1..."
RUST_LOG=chronik_raft=info,chronik_server=info ./target/release/chronik-server \
  --node-id 1 \
  --kafka-port 9092 \
  --data-dir /tmp/chronik-raft-test-1 \
  --advertised-addr localhost \
  raft-cluster \
  --raft-addr 127.0.0.1:5001 \
  --peers 2@127.0.0.1:5002,3@127.0.0.1:5003 \
  > /tmp/raft-node1.log 2>&1 &
NODE1_PID=$!

echo "Starting node 2..."
RUST_LOG=chronik_raft=info,chronik_server=info ./target/release/chronik-server \
  --node-id 2 \
  --kafka-port 9093 \
  --data-dir /tmp/chronik-raft-test-2 \
  --advertised-addr localhost \
  raft-cluster \
  --raft-addr 127.0.0.1:5002 \
  --peers 1@127.0.0.1:5001,3@127.0.0.1:5003 \
  > /tmp/raft-node2.log 2>&1 &
NODE2_PID=$!

echo "Starting node 3..."
RUST_LOG=chronik_raft=info,chronik_server=info ./target/release/chronik-server \
  --node-id 3 \
  --kafka-port 9094 \
  --data-dir /tmp/chronik-raft-test-3 \
  --advertised-addr localhost \
  raft-cluster \
  --raft-addr 127.0.0.1:5003 \
  --peers 1@127.0.0.1:5001,2@127.0.0.1:5002 \
  > /tmp/raft-node3.log 2>&1 &
NODE3_PID=$!

echo "Nodes started: PID1=$NODE1_PID, PID2=$NODE2_PID, PID3=$NODE3_PID"

# Wait for cluster to stabilize
echo "Waiting 30 seconds for cluster stabilization..."
sleep 30

# Check if processes are still running
if ! kill -0 $NODE1_PID 2>/dev/null; then
  echo "ERROR: Node 1 died"
  echo "Last 20 lines of node 1 log:"
  tail -20 /tmp/raft-node1.log
  kill $NODE2_PID $NODE3_PID 2>/dev/null || true
  exit 1
fi
if ! kill -0 $NODE2_PID 2>/dev/null; then
  echo "ERROR: Node 2 died"
  echo "Last 20 lines of node 2 log:"
  tail -20 /tmp/raft-node2.log
  kill $NODE1_PID $NODE3_PID 2>/dev/null || true
  exit 1
fi
if ! kill -0 $NODE3_PID 2>/dev/null; then
  echo "ERROR: Node 3 died"
  echo "Last 20 lines of node 3 log:"
  tail -20 /tmp/raft-node3.log
  kill $NODE1_PID $NODE2_PID 2>/dev/null || true
  exit 1
fi

echo "All nodes still running. Analyzing logs..."

# Extract diagnostic information
echo ""
echo "=== Node 1 Progress Tracker ==="
grep "Progress tracker for __meta-0" /tmp/raft-node1.log | tail -1
grep "Peer.*matched=" /tmp/raft-node1.log | tail -6

echo ""
echo "=== Node 2 Progress Tracker ==="
grep "Progress tracker for __meta-0" /tmp/raft-node2.log | tail -1
grep "Peer.*matched=" /tmp/raft-node2.log | tail -6

echo ""
echo "=== Node 3 Progress Tracker ==="
grep "Progress tracker for __meta-0" /tmp/raft-node3.log | tail -1
grep "Peer.*matched=" /tmp/raft-node3.log | tail -6

echo ""
echo "=== Outgoing Messages (last 10) ==="
grep "Outgoing messages from __meta-0" /tmp/raft-node*.log | tail -10

echo ""
echo "=== Incoming Messages (last 10) ==="
grep "Received Raft message for __meta-0" /tmp/raft-node*.log | tail -10

echo ""
echo "=== Committed Entries ==="
grep "Committed entries ready for application" /tmp/raft-node*.log

echo ""
echo "=== Leader Election ==="
grep "Became Raft leader" /tmp/raft-node*.log

# Cleanup
echo ""
echo "Stopping nodes..."
kill $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
sleep 2

echo "Test complete. Full logs in /tmp/raft-node*.log"
