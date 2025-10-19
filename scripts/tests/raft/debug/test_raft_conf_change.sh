#!/bin/bash
#
# Test Raft configuration changes and message generation
#

set -e

echo "🧹 Cleaning up old processes and data..."
pkill -9 chronik-server 2>/dev/null || true
rm -rf node1_data node2_data node3_data
rm -f node1.log node2.log node3.log

echo ""
echo "🚀 Starting 3-node Raft cluster..."

# Start Node 1 (will become leader of single-node cluster)
RUST_LOG=chronik_raft=debug,chronik_server::raft_integration=debug \
./target/release/chronik-server \
    --advertised-addr localhost \
    --kafka-port 9092 \
    --raft-port 9093 \
    --metrics-port 8093 \
    --search-port 6080 \
    --data-dir ./node1_data \
    standalone \
    > node1.log 2>&1 &
NODE1_PID=$!
echo "✅ Node 1 started (PID: $NODE1_PID)"

sleep 2

# Start Node 2 (will become leader of its own single-node cluster)
RUST_LOG=chronik_raft=debug,chronik_server::raft_integration=debug \
./target/release/chronik-server \
    --advertised-addr localhost \
    --kafka-port 9192 \
    --raft-port 9193 \
    --metrics-port 8193 \
    --search-port 6180 \
    --data-dir ./node2_data \
    standalone \
    > node2.log 2>&1 &
NODE2_PID=$!
echo "✅ Node 2 started (PID: $NODE2_PID)"

sleep 2

# Start Node 3 (will become leader of its own single-node cluster)
RUST_LOG=chronik_raft=debug,chronik_server::raft_integration=debug \
./target/release/chronik-server \
    --advertised-addr localhost \
    --kafka-port 9292 \
    --raft-port 9293 \
    --metrics-port 8293 \
    --search-port 6280 \
    --data-dir ./node3_data \
    standalone \
    > node3.log 2>&1 &
NODE3_PID=$!
echo "✅ Node 3 started (PID: $NODE3_PID)"

echo ""
echo "⏳ Waiting 5 seconds for nodes to initialize..."
sleep 5

echo ""
echo "📊 Checking Node 1 logs for ready() messages..."
echo "Looking for 'ready() EXTRACTING' with messages > 0..."
grep "ready() EXTRACTING" node1.log | tail -5 || echo "No ready() extractions yet"

echo ""
echo "📊 Checking Node 1 logs for conf changes..."
grep "conf change" node1.log | tail -5 || echo "No conf changes yet"

echo ""
echo "📊 Checking Node 1 logs for leader election..."
grep -E "Campaign|leader|role=" node1.log | tail -10 || echo "No election logs yet"

echo ""
echo "🧪 Testing basic Kafka connectivity..."
echo "test-message" | timeout 2 kafka-console-producer \
    --bootstrap-server localhost:9092 \
    --topic test-topic 2>&1 || echo "❌ Producer failed (expected for now)"

echo ""
echo "📋 Process status:"
ps aux | grep chronik-server | grep -v grep || echo "No processes found"

echo ""
echo "🔍 Node 1 final log (last 30 lines):"
tail -30 node1.log

echo ""
echo "⚠️  Keeping cluster running for manual inspection..."
echo "To stop: pkill -9 chronik-server"
echo "Logs: node1.log, node2.log, node3.log"
