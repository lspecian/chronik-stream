#!/bin/bash
set -e

echo "=========================================="
echo "Phase 5: Manual Leader Election Test"
echo "=========================================="

# Clean up old data
rm -rf ./data-node* ./node*.log 2>/dev/null || true

# Start Node 1 (will be initial leader)
echo ""
echo "Starting Node 1 (localhost:9092)..."
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  --data-dir ./data-node1 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap \
  > node1.log 2>&1 &

NODE1_PID=$!
echo "Node 1 started (PID: $NODE1_PID)"
sleep 3

# Start Node 2
echo ""
echo "Starting Node 2 (localhost:9093)..."
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  --data-dir ./data-node2 \
  raft-cluster \
  --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" \
  --bootstrap \
  > node2.log 2>&1 &

NODE2_PID=$!
echo "Node 2 started (PID: $NODE2_PID)"
sleep 3

# Start Node 3
echo ""
echo "Starting Node 3 (localhost:9094)..."
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  --data-dir ./data-node3 \
  raft-cluster \
  --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" \
  --bootstrap \
  > node3.log 2>&1 &

NODE3_PID=$!
echo "Node 3 started (PID: $NODE3_PID)"

echo ""
echo "Waiting 60 seconds for cluster to stabilize..."
sleep 60

echo ""
echo "=========================================="
echo "Verification 1: LeaderElector Started"
echo "=========================================="
grep -i "LeaderElector monitoring started" node*.log && echo "✅ PASS" || echo "❌ FAIL"

echo ""
echo "=========================================="
echo "Verification 2: Produce Messages"
echo "=========================================="
echo "Producing 10 messages to node 1..."
for i in {1..10}; do
  echo "message-$i" | kafkacat -P -b localhost:9092 -t test-leader -p 0 2>/dev/null || true
  sleep 0.1
done

echo "Waiting for messages to be processed..."
sleep 5

echo ""
echo "=========================================="
echo "Verification 3: Heartbeat Recording"
echo "=========================================="
echo "Checking logs for heartbeat evidence..."
grep -i "heartbeat" node1.log | tail -5 || echo "No heartbeat logs (may need trace level)"

echo ""
echo "=========================================="
echo "Verification 4: Consume Messages"
echo "=========================================="
echo "Consuming messages from node 1..."
timeout 5 kafkacat -C -b localhost:9092 -t test-leader -p 0 -o beginning -c 10 2>/dev/null || echo "Done"

echo ""
echo "=========================================="
echo "Verification 5: Kill Leader (Node 1)"
echo "=========================================="
echo "Killing node 1 (PID: $NODE1_PID)..."
kill -9 $NODE1_PID 2>/dev/null || true
echo "Node 1 killed at $(date)"

echo ""
echo "Waiting 15 seconds for leader election..."
sleep 15

echo ""
echo "=========================================="
echo "Verification 6: Leader Election Logs"
echo "=========================================="
echo "Checking node 2 for election activity..."
grep -E "Leader timeout|Elected new leader|election" node2.log | tail -10 || echo "No election logs found"

echo ""
echo "Checking node 3 for election activity..."
grep -E "Leader timeout|Elected new leader|election" node3.log | tail -10 || echo "No election logs found"

echo ""
echo "=========================================="
echo "Verification 7: Produce to New Leader"
echo "=========================================="
echo "Producing 10 more messages to node 2 (should be new leader)..."
for i in {11..20}; do
  echo "message-$i" | kafkacat -P -b localhost:9093 -t test-leader -p 0 2>/dev/null || true
  sleep 0.1
done

sleep 5

echo ""
echo "=========================================="
echo "Verification 8: Consume All Messages"
echo "=========================================="
echo "Consuming all messages from node 2..."
timeout 10 kafkacat -C -b localhost:9093 -t test-leader -p 0 -o beginning -e 2>/dev/null | wc -l || echo "Done"

echo ""
echo "=========================================="
echo "Cleanup"
echo "=========================================="
kill -9 $NODE2_PID $NODE3_PID 2>/dev/null || true
echo "All nodes stopped"

echo ""
echo "=========================================="
echo "Test Complete!"
echo "=========================================="
echo "Check node*.log for detailed output"
