#!/bin/bash
# Start 3-node Raft cluster simultaneously
# This avoids the sequential startup timeout issue

set -e

echo "======================================"
echo "Starting 3-Node Raft Cluster (Simultaneous)"
echo "======================================"

# Kill any existing processes
pkill -f chronik-server || true
sleep 2

# Clean up old data
rm -rf ./data-node* node*.log
echo "Cleaned up old data"

# Build binary if needed
if [ ! -f ./target/release/chronik-server ]; then
    echo "Building chronik-server..."
    cargo build --release --package chronik-server --all-features
fi

echo ""
echo "Starting all 3 nodes simultaneously..."
echo ""

# Start all nodes in background (within 1 second of each other)
RUST_LOG=info,chronik_raft=info,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9092 --advertised-addr localhost --node-id 1 --data-dir ./data-node1 \
  raft-cluster --raft-addr 0.0.0.0:9192 --peers "2@localhost:9292,3@localhost:9392" \
  > node1.log 2>&1 &
PID1=$!

RUST_LOG=info,chronik_raft=info,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9093 --advertised-addr localhost --node-id 2 --data-dir ./data-node2 \
  raft-cluster --raft-addr 0.0.0.0:9292 --peers "1@localhost:9192,3@localhost:9392" \
  > node2.log 2>&1 &
PID2=$!

RUST_LOG=info,chronik_raft=info,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9094 --advertised-addr localhost --node-id 3 --data-dir ./data-node3 \
  raft-cluster --raft-addr 0.0.0.0:9392 --peers "1@localhost:9192,2@localhost:9292" \
  > node3.log 2>&1 &
PID3=$!

echo "✅ Node 1 started (PID: $PID1, Kafka: 9092, Raft: 9192)"
echo "✅ Node 2 started (PID: $PID2, Kafka: 9093, Raft: 9292)"
echo "✅ Node 3 started (PID: $PID3, Kafka: 9094, Raft: 9392)"

echo ""
echo "Waiting 10 seconds for cluster formation..."
sleep 10

echo ""
echo "======================================"
echo "Cluster Status"
echo "======================================"

# Check if all 3 processes are still running
RUNNING=$(ps aux | grep chronik-server | grep -v grep | wc -l | tr -d ' ')
echo "Nodes running: $RUNNING/3"

if [ "$RUNNING" -eq 3 ]; then
    echo "✅ All 3 nodes are running!"
else
    echo "⚠️ Only $RUNNING nodes are running. Expected 3."
    echo ""
    echo "Checking logs for errors..."
    for i in 1 2 3; do
        if ! ps -p $(eval echo \$PID$i) > /dev/null 2>&1; then
            echo ""
            echo "=== Node $i crashed - last 30 lines of log ==="
            tail -30 node$i.log
        fi
    done
    exit 1
fi

echo ""
echo "Checking for leader election..."
sleep 2

# Check logs for leader election
LEADER_COUNT=$(grep -l "raft_state=Leader" node*.log | wc -l | tr -d ' ')
echo "Leaders elected: $LEADER_COUNT"

if [ "$LEADER_COUNT" -gt 0 ]; then
    echo "✅ Leader election successful!"
    grep "raft_state=Leader" node*.log | head -3
else
    echo "⚠️ No leader elected yet. Cluster may still be forming..."
fi

echo ""
echo "======================================"
echo "Next Steps:"
echo "======================================"
echo "1. Check logs: tail -f node1.log node2.log node3.log"
echo "2. Test topic creation:"
echo "   kafka-topics --bootstrap-server localhost:9092 --create --topic test --partitions 3 --replication-factor 3"
echo "3. Test produce/consume:"
echo "   echo 'test message' | kafka-console-producer --bootstrap-server localhost:9092 --topic test"
echo "   kafka-console-consumer --bootstrap-server localhost:9092 --topic test --from-beginning --max-messages 1"
echo "4. Stop cluster: pkill -f chronik-server"
echo ""
