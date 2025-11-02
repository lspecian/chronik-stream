#!/bin/bash
set -e

# Clean up
pkill chronik-server || true
sleep 2
rm -rf ./data-leader ./data-follower
mkdir -p ./data-leader ./data-follower

# Start follower (port 9093, WAL receiver on 15432)
echo "Starting follower node..."
RUST_LOG=info \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_WAL_RECEIVER_ADDR=0.0.0.0:15432 \
./target/release/chronik-server \
  --kafka-port 9093 \
  --metrics-port 9095 \
  --data-dir ./data-follower \
  --disable-search \
  standalone > follower.log 2>&1 &

FOLLOWER_PID=$!
echo "Follower PID: $FOLLOWER_PID"
sleep 3

# Check follower started
if ! ps -p $FOLLOWER_PID > /dev/null; then
    echo "ERROR: Follower failed to start"
    cat follower.log
    exit 1
fi

echo "✅ Follower running"
strings follower.log | grep -E "WAL receiver listening|Integrated Kafka server listening" | tail -2

# Start leader (port 9092, replicates to localhost:15432)
echo ""
echo "Starting leader node..."
RUST_LOG=info \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_REPLICATION_FOLLOWERS=localhost:15432 \
./target/release/chronik-server \
  --kafka-port 9092 \
  --metrics-port 9094 \
  --data-dir ./data-leader \
  --disable-search \
  standalone > leader.log 2>&1 &

LEADER_PID=$!
echo "Leader PID: $LEADER_PID"
sleep 3

# Check leader started
if ! ps -p $LEADER_PID > /dev/null; then
    echo "ERROR: Leader failed to start"
    cat leader.log
    exit 1
fi

echo "✅ Leader running"
strings leader.log | grep -E "Connected to follower|Integrated Kafka server listening" | tail -2

echo ""
echo "✅ 2-node cluster started successfully!"
echo "   Leader: localhost:9092 (replicating to localhost:15432)"
echo "   Follower: localhost:9093 (receiving on port 15432)"
echo ""
echo "Logs: leader.log, follower.log"
