#!/bin/bash
set -e

echo "🧹 Cleaning up old processes and data..."
pkill -9 chronik-server || true
sleep 2
rm -rf node1_data node2_data node3_data
mkdir -p node1_data node2_data node3_data

echo ""
echo "🔧 Using existing chronik-server binary (skipping build)..."
echo ""
echo "🚀 Starting 3-node Raft cluster..."
echo "Node 1: Kafka=9092, Raft=9093, Metrics=8093, Search=6080 (default)"
echo "Node 2: Kafka=9192, Raft=9193, Metrics=8193, Search=6180"
echo "Node 3: Kafka=9292, Raft=9293, Metrics=8293, Search=6280"
echo ""

# Start Node 1 (uses default search port 6080)
RUST_LOG=chronik_raft=debug,chronik_server::raft_integration=debug,info \
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=1 \
CHRONIK_REPLICATION_FACTOR=3 \
CHRONIK_MIN_INSYNC_REPLICAS=2 \
CHRONIK_CLUSTER_PEERS="localhost:9092:9093,localhost:9192:9193,localhost:9292:9293" \
./target/release/chronik-server \
  --kafka-port 9092 \
  --metrics-port 8093 \
  --bind-addr 0.0.0.0 \
  --advertised-addr localhost \
  --data-dir ./node1_data \
  standalone \
  > node1.log 2>&1 &
NODE1_PID=$!
echo "✅ Node 1 started (PID: $NODE1_PID)"

sleep 2

# Start Node 2
RUST_LOG=chronik_raft=debug,chronik_server::raft_integration=debug,info \
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=2 \
CHRONIK_REPLICATION_FACTOR=3 \
CHRONIK_MIN_INSYNC_REPLICAS=2 \
CHRONIK_CLUSTER_PEERS="localhost:9092:9093,localhost:9192:9193,localhost:9292:9293" \
./target/release/chronik-server \
  --kafka-port 9192 \
  --metrics-port 8193 \
  --search-port 6180 \
  --bind-addr 0.0.0.0 \
  --advertised-addr localhost \
  --data-dir ./node2_data \
  standalone \
  > node2.log 2>&1 &
NODE2_PID=$!
echo "✅ Node 2 started (PID: $NODE2_PID)"

sleep 2

# Start Node 3
RUST_LOG=chronik_raft=debug,chronik_server::raft_integration=debug,info \
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=3 \
CHRONIK_REPLICATION_FACTOR=3 \
CHRONIK_MIN_INSYNC_REPLICAS=2 \
CHRONIK_CLUSTER_PEERS="localhost:9092:9093,localhost:9192:9193,localhost:9292:9293" \
./target/release/chronik-server \
  --kafka-port 9292 \
  --metrics-port 8293 \
  --search-port 6280 \
  --bind-addr 0.0.0.0 \
  --advertised-addr localhost \
  --data-dir ./node3_data \
  standalone \
  > node3.log 2>&1 &
NODE3_PID=$!
echo "✅ Node 3 started (PID: $NODE3_PID)"

echo ""
echo "⏳ Waiting 5 seconds for cluster formation..."
sleep 5

echo ""
echo "📊 Checking process status..."
if ps -p $NODE1_PID > /dev/null; then
    echo "✅ Node 1 (PID $NODE1_PID) is running"
else
    echo "❌ Node 1 (PID $NODE1_PID) has crashed"
fi

if ps -p $NODE2_PID > /dev/null; then
    echo "✅ Node 2 (PID $NODE2_PID) is running"
else
    echo "❌ Node 2 (PID $NODE2_PID) has crashed"
fi

if ps -p $NODE3_PID > /dev/null; then
    echo "✅ Node 3 (PID $NODE3_PID) is running"
else
    echo "❌ Node 3 (PID $NODE3_PID) has crashed"
fi

echo ""
echo "📋 Node 1 logs (last 20 lines):"
tail -n 20 node1.log

echo ""
echo "📋 Node 2 logs (last 20 lines):"
tail -n 20 node2.log

echo ""
echo "📋 Node 3 logs (last 20 lines):"
tail -n 20 node3.log

echo ""
echo "🧪 Testing Kafka connections..."
echo ""

# Test Node 1 connection
echo "Testing Node 1 (localhost:9092)..."
timeout 5 nc -zv localhost 9092 && echo "✅ Node 1 Kafka port is open" || echo "❌ Node 1 Kafka port is closed"

# Test Node 2 connection
echo "Testing Node 2 (localhost:9192)..."
timeout 5 nc -zv localhost 9192 && echo "✅ Node 2 Kafka port is open" || echo "❌ Node 2 Kafka port is closed"

# Test Node 3 connection
echo "Testing Node 3 (localhost:9292)..."
timeout 5 nc -zv localhost 9292 && echo "✅ Node 3 Kafka port is open" || echo "❌ Node 3 Kafka port is closed"

echo ""
echo "🎯 Cluster is running. To stop:"
echo "  kill $NODE1_PID $NODE2_PID $NODE3_PID"
echo ""
echo "To run Python test:"
echo "  python3 test_3node_produce_consume.py"
