#!/bin/bash
# Simple Raft cluster test - just start 3 nodes and verify they form a cluster

set -e

echo "Building server with Raft support..."
cargo build --release --bin chronik-server --features raft

echo "Cleaning up old data..."
rm -rf ./data/node1 ./data/node2 ./data/node3 ./data/wal/__meta

echo "Starting Node 1..."
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=1 \
CHRONIK_KAFKA_PORT=9092 \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_ADVERTISED_PORT=9092 \
CHRONIK_RAFT_PORT=9192 \
CHRONIK_DATA_DIR=./data/node1 \
CHRONIK_REPLICATION_FACTOR=3 \
CHRONIK_MIN_INSYNC_REPLICAS=2 \
CHRONIK_CLUSTER_PEERS="localhost:9092:9192,localhost:9093:9193,localhost:9094:9194" \
RUST_LOG=info,chronik_server::raft_integration=debug \
cargo run --release --bin chronik-server --features raft -- standalone > node1_simple.log 2>&1 &
NODE1_PID=$!

sleep 5

echo "Starting Node 2..."
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=2 \
CHRONIK_KAFKA_PORT=9093 \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_ADVERTISED_PORT=9093 \
CHRONIK_RAFT_PORT=9193 \
CHRONIK_DATA_DIR=./data/node2 \
CHRONIK_REPLICATION_FACTOR=3 \
CHRONIK_MIN_INSYNC_REPLICAS=2 \
CHRONIK_CLUSTER_PEERS="localhost:9092:9192,localhost:9093:9193,localhost:9094:9194" \
RUST_LOG=info,chronik_server::raft_integration=debug \
cargo run --release --bin chronik-server --features raft -- standalone > node2_simple.log 2>&1 &
NODE2_PID=$!

sleep 5

echo "Starting Node 3..."
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=3 \
CHRONIK_KAFKA_PORT=9094 \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_ADVERTISED_PORT=9094 \
CHRONIK_RAFT_PORT=9194 \
CHRONIK_DATA_DIR=./data/node3 \
CHRONIK_REPLICATION_FACTOR=3 \
CHRONIK_MIN_INSYNC_REPLICAS=2 \
CHRONIK_CLUSTER_PEERS="localhost:9092:9192,localhost:9093:9193,localhost:9094:9194" \
RUST_LOG=info,chronik_server::raft_integration=debug \
cargo run --release --bin chronik-server --features raft -- standalone > node3_simple.log 2>&1 &
NODE3_PID=$!

echo "Waiting for cluster to start..."
sleep 15

echo ""
echo "Checking logs for Raft initialization..."
echo "================================================"
echo "Node 1 Raft logs:"
grep -i "raft" node1_simple.log | head -20 || echo "No Raft logs found in node1"
echo ""
echo "Node 2 Raft logs:"
grep -i "raft" node2_simple.log | head -20 || echo "No Raft logs found in node2"
echo ""
echo "Node 3 Raft logs:"
grep -i "raft" node3_simple.log | head -20 || echo "No Raft logs found in node3"

echo ""
echo "Killing all nodes..."
kill $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
wait

echo "Done! Check node*_simple.log for full details."
