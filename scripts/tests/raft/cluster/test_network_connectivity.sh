#!/bin/bash

# Kill existing
pkill -f chronik-server || true
sleep 2

# Start all 3 nodes
echo "Starting cluster..."
RUST_LOG=info ./target/release/chronik-server \
  --advertised-addr localhost --kafka-port 9092 \
  --cluster-config node1-cluster.toml --node-id 1 \
  standalone > node1.log 2>&1 &
N1=$!

RUST_LOG=info ./target/release/chronik-server \
  --advertised-addr localhost --kafka-port 9093 \
  --cluster-config node2-cluster.toml --node-id 2 \
  standalone > node2.log 2>&1 &
N2=$!

RUST_LOG=info ./target/release/chronik-server \
  --advertised-addr localhost --kafka-port 9094 \
  --cluster-config node3-cluster.toml --node-id 3 \
  standalone > node3.log 2>&1 &
N3=$!

echo "Waiting for Raft servers to start..."
sleep 5

echo ""
echo "=== Testing network connectivity ==="
for port in 9192 9193 9194; do
  echo -n "Port $port: "
  nc -z localhost $port && echo "✓ REACHABLE" || echo "✗ UNREACHABLE"
done

echo ""
echo "=== Checking actual listening ports ==="
lsof -i :9192 -i :9193 -i :9194 2>/dev/null | grep LISTEN

echo ""
echo "=== Checking peer connection errors ==="
grep -i "tcp connect error\|transport error\|failed to connect" node*.log | head -10

# Keep running for observation
echo ""
echo "Cluster running. Press Ctrl+C to stop."
wait
