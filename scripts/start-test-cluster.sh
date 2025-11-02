#!/bin/bash

# Start 3-node Raft cluster for testing
# Phase 2: Wire RaftCluster to ProduceHandler

set -e

# Clean environment
echo "Cleaning test environment..."
pkill -9 -f "chronik-server" 2>/dev/null || true
rm -rf /tmp/chronik-test-cluster
mkdir -p /tmp/chronik-test-cluster

# Build path
CHRONIK_BIN="./target/release/chronik-server"

echo "Starting Node 1 (Kafka: 9092, Raft: 9192)..."
$CHRONIK_BIN \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  --data-dir /tmp/chronik-test-cluster/node1 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap > /tmp/chronik-test-cluster/node1.log 2>&1 &

echo "Starting Node 2 (Kafka: 9093, Raft: 9193)..."
$CHRONIK_BIN \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  --data-dir /tmp/chronik-test-cluster/node2 \
  raft-cluster \
  --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" \
  --bootstrap > /tmp/chronik-test-cluster/node2.log 2>&1 &

echo "Starting Node 3 (Kafka: 9094, Raft: 9194)..."
$CHRONIK_BIN \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  --data-dir /tmp/chronik-test-cluster/node3 \
  raft-cluster \
  --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" \
  --bootstrap > /tmp/chronik-test-cluster/node3.log 2>&1 &

echo "Waiting for cluster to stabilize (15 seconds)..."
sleep 15

# Check if nodes are still running
RUNNING=$(ps aux | grep chronik-server | grep -v grep | wc -l)
echo "Nodes running: $RUNNING/3"

if [ "$RUNNING" -eq 3 ]; then
    echo "✅ All 3 nodes are running!"
    echo "Logs:"
    echo "  - Node 1: /tmp/chronik-test-cluster/node1.log"
    echo "  - Node 2: /tmp/chronik-test-cluster/node2.log"
    echo "  - Node 3: /tmp/chronik-test-cluster/node3.log"
else
    echo "❌ Some nodes crashed! Check logs for details."
    exit 1
fi
