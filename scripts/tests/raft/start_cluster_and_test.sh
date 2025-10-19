#!/bin/bash
# Start 3-node cluster and run Kafka client test

set -e

# Clean up old data and processes
echo "Cleaning up..."
pkill -f chronik-server || true
rm -rf /Users/lspecian/Development/chronik-stream/.conductor/lahore/data/node*
rm -f /Users/lspecian/Development/chronik-stream/.conductor/lahore/*.log

# Start 3-node cluster
echo "Starting 3-node Raft cluster..."

# Node 1 (port 9092, Raft port 9192)
RUST_LOG=info \
  /Users/lspecian/Development/chronik-stream/.conductor/lahore/target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9092 \
  --cluster-config /Users/lspecian/Development/chronik-stream/.conductor/lahore/node1-cluster.toml \
  --node-id 1 \
  standalone \
  > /Users/lspecian/Development/chronik-stream/.conductor/lahore/node1.log 2>&1 &
NODE1_PID=$!
echo "Started node 1 (PID: $NODE1_PID, Kafka: 9092, Raft: 9192)"

# Node 2 (port 9093, Raft port 9193)
RUST_LOG=info \
  /Users/lspecian/Development/chronik-stream/.conductor/lahore/target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9093 \
  --cluster-config /Users/lspecian/Development/chronik-stream/.conductor/lahore/node2-cluster.toml \
  --node-id 2 \
  standalone \
  > /Users/lspecian/Development/chronik-stream/.conductor/lahore/node2.log 2>&1 &
NODE2_PID=$!
echo "Started node 2 (PID: $NODE2_PID, Kafka: 9093, Raft: 9193)"

# Node 3 (port 9094, Raft port 9194)
RUST_LOG=info \
  /Users/lspecian/Development/chronik-stream/.conductor/lahore/target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9094 \
  --cluster-config /Users/lspecian/Development/chronik-stream/.conductor/lahore/node3-cluster.toml \
  --node-id 3 \
  standalone \
  > /Users/lspecian/Development/chronik-stream/.conductor/lahore/node3.log 2>&1 &
NODE3_PID=$!
echo "Started node 3 (PID: $NODE3_PID, Kafka: 9094, Raft: 9194)"

# Wait for cluster to initialize and brokers to register
echo "Waiting 15 seconds for cluster to initialize and brokers to register..."
sleep 15

# Run Kafka client test
echo ""
echo "Running Kafka client test..."
python3 /Users/lspecian/Development/chronik-stream/.conductor/lahore/test_cluster_kafka_client.py
TEST_RESULT=$?

# Cleanup
echo ""
echo "Stopping cluster..."
kill $NODE1_PID $NODE2_PID $NODE3_PID || true
wait

echo ""
if [ $TEST_RESULT -eq 0 ]; then
    echo "✓ TEST PASSED"
    exit 0
else
    echo "✗ TEST FAILED"
    echo "Check logs: node1.log, node2.log, node3.log"
    exit 1
fi
