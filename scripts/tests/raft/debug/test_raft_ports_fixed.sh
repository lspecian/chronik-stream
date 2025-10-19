#!/bin/bash

# Kill existing processes
pkill -f chronik-server
sleep 2

# Clean logs
rm -f /tmp/chronik-node*.log

# Start Node 1 with CORRECT flag
echo "Starting Node 1..."
RUST_LOG=info,chronik_raft=debug ./target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9092 \
  --cluster-config node1-cluster.toml \
  --node-id 1 \
  standalone > /tmp/chronik-node1.log 2>&1 &
NODE1_PID=$!
echo "Node 1 PID: $NODE1_PID"

# Wait and check
sleep 5

echo ""
echo "=== Checking if Raft ports are listening ==="
lsof -i :9192 2>/dev/null | grep LISTEN || echo "Raft port 9192 NOT listening"

echo ""
echo "=== Node 1 Log (last 100 lines, looking for Raft) ==="
tail -100 /tmp/chronik-node1.log | grep -A 2 -B 2 -i "raft\|grpc\|listen"

echo ""
echo "=== Full error messages ==="
grep -i "error\|fatal\|failed" /tmp/chronik-node1.log | head -20

# Cleanup
kill $NODE1_PID 2>/dev/null
