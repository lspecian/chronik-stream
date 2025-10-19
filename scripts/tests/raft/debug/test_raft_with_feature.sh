#!/bin/bash

# Kill existing processes
pkill -f chronik-server
sleep 2

# Clean logs
rm -f /tmp/chronik-node*.log

# Start Node 1 with raft-enabled binary
echo "Starting Node 1 with raft feature..."
RUST_LOG=info,chronik_raft=debug ./target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9092 \
  --cluster-config node1-cluster.toml \
  --node-id 1 \
  standalone > /tmp/chronik-node1.log 2>&1 &
NODE1_PID=$!
echo "Node 1 PID: $NODE1_PID"

# Wait for startup
sleep 5

echo ""
echo "=== Checking if Raft port 9192 is listening ==="
lsof -i :9192 2>/dev/null | grep LISTEN && echo "✓ Raft port 9192 IS listening!" || echo "✗ Raft port 9192 NOT listening"

echo ""
echo "=== Checking for Raft server startup messages ==="
grep -i "raft.*server.*listen\|raft.*grpc.*start\|starting raft" /tmp/chronik-node1.log | head -10

echo ""
echo "=== Checking for errors ==="
grep -i "error\|fatal" /tmp/chronik-node1.log | head -10

echo ""
echo "=== Last 30 lines of log ==="
tail -30 /tmp/chronik-node1.log

# Cleanup
kill $NODE1_PID 2>/dev/null
wait $NODE1_PID 2>/dev/null
