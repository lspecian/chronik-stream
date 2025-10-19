#!/bin/bash

# Kill existing processes
pkill -f chronik-server
sleep 2

# Clean logs
rm -f /tmp/chronik-node*.log

# Start Node 1
echo "Starting Node 1..."
RUST_LOG=info,chronik_raft=debug ./target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9092 \
  --config node1-cluster.toml \
  standalone > /tmp/chronik-node1.log 2>&1 &
NODE1_PID=$!
echo "Node 1 PID: $NODE1_PID"

# Wait and check
sleep 5

echo ""
echo "=== Checking if Raft ports are listening ==="
lsof -i :9192 -i :9193 -i :9194 2>/dev/null | grep LISTEN || echo "NO Raft ports listening"

echo ""
echo "=== Node 1 Log (first 50 lines) ==="
head -50 /tmp/chronik-node1.log

echo ""
echo "=== Searching for Raft server startup ==="
grep -i "raft.*listen\|raft.*server\|raft.*start" /tmp/chronik-node1.log || echo "No Raft server startup messages found"

# Cleanup
kill $NODE1_PID 2>/dev/null
