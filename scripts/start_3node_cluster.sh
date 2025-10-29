#!/bin/bash
set -e

echo "=== Starting Chronik 3-Node Raft Cluster ==="
echo ""

# Clean up
echo "1. Cleaning up old processes and data..."
killall -9 chronik-server 2>/dev/null || true
rm -rf ./data-node1 ./data-node2 ./data-node3
mkdir -p ./data-node1 ./data-node2 ./data-node3
sleep 2

# Start Node 1
echo ""
echo "2. Starting Node 1 (port 9092, raft 9192)..."
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  --data-dir ./data-node1 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap \
  > /tmp/node1.log 2>&1 &
NODE1_PID=$!
echo "   Node 1 PID: $NODE1_PID"

# Wait for Node 1 gRPC server to be ready
echo "   Waiting for Node 1 gRPC server..."
for i in {1..30}; do
  if grep -q "Raft gRPC server started successfully" /tmp/node1.log 2>/dev/null; then
    echo "   ✅ Node 1 gRPC server ready"
    break
  fi
  sleep 1
  if [ $i -eq 30 ]; then
    echo "   ❌ Node 1 gRPC server failed to start"
    exit 1
  fi
done

# Start Node 2
echo ""
echo "3. Starting Node 2 (port 9093, raft 9193)..."
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  --data-dir ./data-node2 \
  raft-cluster \
  --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" \
  --bootstrap \
  > /tmp/node2.log 2>&1 &
NODE2_PID=$!
echo "   Node 2 PID: $NODE2_PID"

# Wait for Node 2 gRPC server to be ready
echo "   Waiting for Node 2 gRPC server..."
for i in {1..30}; do
  if grep -q "Raft gRPC server started successfully" /tmp/node2.log 2>/dev/null; then
    echo "   ✅ Node 2 gRPC server ready"
    break
  fi
  sleep 1
  if [ $i -eq 30 ]; then
    echo "   ❌ Node 2 gRPC server failed to start"
    exit 1
  fi
done

# Start Node 3
echo ""
echo "4. Starting Node 3 (port 9094, raft 9194)..."
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  --data-dir ./data-node3 \
  raft-cluster \
  --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" \
  --bootstrap \
  > /tmp/node3.log 2>&1 &
NODE3_PID=$!
echo "   Node 3 PID: $NODE3_PID"

# Wait for Node 3 gRPC server to be ready
echo "   Waiting for Node 3 gRPC server..."
for i in {1..30}; do
  if grep -q "Raft gRPC server started successfully" /tmp/node3.log 2>/dev/null; then
    echo "   ✅ Node 3 gRPC server ready"
    break
  fi
  sleep 1
  if [ $i -eq 30 ]; then
    echo "   ❌ Node 3 gRPC server failed to start"
    exit 1
  fi
done

# Wait for leader election
echo ""
echo "5. Waiting for leader election (30s max)..."
for i in {1..30}; do
  if grep -q "became leader at term" /tmp/node*.log 2>/dev/null; then
    LEADER=$(grep "became leader at term" /tmp/node*.log | head -1 | grep -oP "raft_id: \K\d+")
    echo "   ✅ Leader elected: Node $LEADER"
    break
  fi
  sleep 1
  if [ $i -eq 30 ]; then
    echo "   ⚠️  No leader elected yet (this may be normal, check logs)"
  fi
done

echo ""
echo "=== Cluster Status ==="
echo "Node 1 (leader candidate): PID $NODE1_PID, Kafka port 9092"
echo "Node 2 (follower):         PID $NODE2_PID, Kafka port 9093"
echo "Node 3 (follower):         PID $NODE3_PID, Kafka port 9094"
echo ""
echo "Logs: /tmp/node1.log, /tmp/node2.log, /tmp/node3.log"
echo ""
echo "To test: ./target/release/chronik-bench --bootstrap-servers localhost:9092 --topic test --mode produce --concurrency 64 --duration 20s"
