#!/bin/bash
# Test script to debug broker registration issues in Raft cluster

set -e

# Clean up old data
echo "Cleaning up old data..."
rm -rf /Users/lspecian/Development/chronik-stream/.conductor/lahore/data/node*
rm -f /Users/lspecian/Development/chronik-stream/.conductor/lahore/*.log

# Start 3-node cluster with detailed logging
echo "Starting 3-node Raft cluster with TRACE logging..."

# Node 1 (port 9092, Raft port 9192)
RUST_LOG=chronik_raft=trace,chronik_server=debug,chronik_common::metadata=debug \
  /Users/lspecian/Development/chronik-stream/.conductor/lahore/target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9092 \
  --cluster-config /Users/lspecian/Development/chronik-stream/.conductor/lahore/node1-cluster.toml \
  standalone \
  > /Users/lspecian/Development/chronik-stream/.conductor/lahore/node1.log 2>&1 &
NODE1_PID=$!
echo "Started node 1 (PID: $NODE1_PID, Kafka: 9092, Raft: 9192)"

# Node 2 (port 9093, Raft port 9193)
RUST_LOG=chronik_raft=trace,chronik_server=debug,chronik_common::metadata=debug \
  /Users/lspecian/Development/chronik-stream/.conductor/lahore/target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9093 \
  --cluster-config /Users/lspecian/Development/chronik-stream/.conductor/lahore/node2-cluster.toml \
  standalone \
  > /Users/lspecian/Development/chronik-stream/.conductor/lahore/node2.log 2>&1 &
NODE2_PID=$!
echo "Started node 2 (PID: $NODE2_PID, Kafka: 9093, Raft: 9193)"

# Node 3 (port 9094, Raft port 9194)
RUST_LOG=chronik_raft=trace,chronik_server=debug,chronik_common::metadata=debug \
  /Users/lspecian/Development/chronik-stream/.conductor/lahore/target/release/chronik-server \
  --advertised-addr localhost \
  --kafka-port 9094 \
  --cluster-config /Users/lspecian/Development/chronik-stream/.conductor/lahore/node3-cluster.toml \
  standalone \
  > /Users/lspecian/Development/chronik-stream/.conductor/lahore/node3.log 2>&1 &
NODE3_PID=$!
echo "Started node 3 (PID: $NODE3_PID, Kafka: 9094, Raft: 9194)"

# Wait for cluster to initialize
echo "Waiting 10 seconds for cluster to initialize..."
sleep 10

# Check logs for broker registration
echo ""
echo "=== Checking broker registration in logs ==="
echo ""
echo "Node 1 broker registration:"
grep -i "register.*broker\|broker.*register" /Users/lspecian/Development/chronik-stream/.conductor/lahore/node1.log || echo "  No broker registration logs found"

echo ""
echo "Node 2 broker registration:"
grep -i "register.*broker\|broker.*register" /Users/lspecian/Development/chronik-stream/.conductor/lahore/node2.log || echo "  No broker registration logs found"

echo ""
echo "Node 3 broker registration:"
grep -i "register.*broker\|broker.*register" /Users/lspecian/Development/chronik-stream/.conductor/lahore/node3.log || echo "  No broker registration logs found"

# Check for metadata operations
echo ""
echo "=== Checking metadata operations in logs ==="
echo ""
echo "Node 1 metadata ops:"
grep -i "MetadataOp::RegisterBroker\|Proposing metadata op" /Users/lspecian/Development/chronik-stream/.conductor/lahore/node1.log | head -20 || echo "  No metadata ops found"

echo ""
echo "Node 2 metadata ops:"
grep -i "MetadataOp::RegisterBroker\|Proposing metadata op" /Users/lspecian/Development/chronik-stream/.conductor/lahore/node2.log | head -20 || echo "  No metadata ops found"

echo ""
echo "Node 3 metadata ops:"
grep -i "MetadataOp::RegisterBroker\|Proposing metadata op" /Users/lspecian/Development/chronik-stream/.conductor/lahore/node3.log | head -20 || echo "  No metadata ops found"

# Check for Raft leader election
echo ""
echo "=== Checking Raft leader election ==="
echo ""
grep -i "became leader\|leader.*elected" /Users/lspecian/Development/chronik-stream/.conductor/lahore/*.log | head -10 || echo "  No leader election logs found"

# Check for applied entries
echo ""
echo "=== Checking Raft applied entries ==="
echo ""
grep -i "applied.*entry\|applying.*entry" /Users/lspecian/Development/chronik-stream/.conductor/lahore/*.log | head -20 || echo "  No applied entry logs found"

# Cleanup
echo ""
echo "Test complete. Stopping nodes..."
kill $NODE1_PID $NODE2_PID $NODE3_PID || true
wait

echo ""
echo "Logs saved to:"
echo "  - node1.log"
echo "  - node2.log"
echo "  - node3.log"
