#!/bin/bash
# Phase 1: Quick Election Stability Test (1 minute)
# Validates that new Raft config stops election churn

set -e

echo "=========================================="
echo "Phase 1: Quick Election Stability Test (60s)"
echo "=========================================="
echo ""

# Cleanup function
cleanup() {
    echo ""
    echo "Cleaning up..."
    pkill -f chronik-server || true
    sleep 2
}

trap cleanup EXIT

# Kill any existing processes
cleanup

# Clean up old data
rm -rf ./data-node* node*.log
rm -rf ./test-cluster-configs
echo "Cleaned up old data"

# Create temporary cluster configs
echo ""
echo "Creating cluster configuration files..."
mkdir -p ./test-cluster-configs

# Node 1 config
cat > ./test-cluster-configs/node1.toml <<EOF
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 9192

[[peers]]
id = 2
addr = "localhost:9093"
raft_port = 9193

[[peers]]
id = 3
addr = "localhost:9094"
raft_port = 9194
EOF

# Node 2 config
cat > ./test-cluster-configs/node2.toml <<EOF
enabled = true
node_id = 2
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 9192

[[peers]]
id = 2
addr = "localhost:9093"
raft_port = 9193

[[peers]]
id = 3
addr = "localhost:9094"
raft_port = 9194
EOF

# Node 3 config
cat > ./test-cluster-configs/node3.toml <<EOF
enabled = true
node_id = 3
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 9192

[[peers]]
id = 2
addr = "localhost:9093"
raft_port = 9193

[[peers]]
id = 3
addr = "localhost:9094"
raft_port = 9194
EOF

echo "✅ Created cluster configs"

echo ""
echo "Starting 3-node Raft cluster with Phase 1 config..."
echo "  - Election timeout: 3000ms (was 500ms)"
echo "  - Heartbeat interval: 150ms (was 100ms)"
echo "  - Election ticks: 20 (was 5) ✅ Production-safe"
echo ""

# Start all nodes simultaneously using 'all' mode with cluster config
RUST_LOG=info,chronik_raft=debug,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --data-dir ./data-node1 \
  --cluster-config ./test-cluster-configs/node1.toml \
  all > node1.log 2>&1 &
PID1=$!

RUST_LOG=info,chronik_raft=debug,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --data-dir ./data-node2 \
  --cluster-config ./test-cluster-configs/node2.toml \
  all > node2.log 2>&1 &
PID2=$!

RUST_LOG=info,chronik_raft=debug,chronik_server=info \
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --data-dir ./data-node3 \
  --cluster-config ./test-cluster-configs/node3.toml \
  all > node3.log 2>&1 &
PID3=$!

echo "✅ Node 1 started (PID: $PID1, Kafka: 9092, Raft: 9192)"
echo "✅ Node 2 started (PID: $PID2, Kafka: 9093, Raft: 9193)"
echo "✅ Node 3 started (PID: $PID3, Kafka: 9094, Raft: 9194)"

echo ""
echo "Waiting 10 seconds for cluster formation..."
sleep 10

# Check if nodes are running
RUNNING=$(ps aux | grep chronik-server | grep -v grep | wc -l | tr -d ' ')
if [ "$RUNNING" -ne 3 ]; then
    echo "❌ FAIL: Only $RUNNING/3 nodes running!"
    echo ""
    echo "Checking crash logs..."
    for i in 1 2 3; do
        if ! ps -p $(eval echo \$PID$i) > /dev/null 2>&1; then
            echo ""
            echo "=== Node $i crashed - last 30 lines ==="
            tail -30 node$i.log
        fi
    done
    exit 1
fi

echo "✅ All 3 nodes running"

# Create test topic
echo ""
echo "Creating test topic (3 partitions, replication-factor 3)..."
if command -v kafka-topics &> /dev/null; then
    kafka-topics --bootstrap-server localhost:9092 --create \
      --topic phase1-test --partitions 3 --replication-factor 3 2>&1 || echo "Topic may already exist"
else
    echo "WARN: kafka-topics not found (install via: brew install kafka)"
fi

sleep 5

# Capture initial state
INITIAL_ELECTIONS=$(grep -h "Became Raft leader" node*.log 2>/dev/null | wc -l || echo 0)
echo ""
echo "Initial elections: $INITIAL_ELECTIONS"

# Monitor for 60 seconds
echo ""
echo "Monitoring cluster for 60 seconds..."
echo "(Checking for election churn every 10 seconds)"
echo ""

for i in {1..6}; do
    sleep 10

    CURRENT_ELECTIONS=$(grep -h "Became Raft leader" node*.log 2>/dev/null | wc -l || echo 0)
    NEW_ELECTIONS=$((CURRENT_ELECTIONS - INITIAL_ELECTIONS))

    # Check max term
    MAX_TERM=$(grep -h "Became Raft leader" node*.log 2>/dev/null | \
        awk '{for(j=1;j<=NF;j++) if($j~"term") print $j}' | \
        sed 's/term=//' | sed 's/,$//' | sort -n | tail -1 || echo 1)

    echo "[${i}0s] Elections: $CURRENT_ELECTIONS (new: $NEW_ELECTIONS) | Max term: $MAX_TERM"

    INITIAL_ELECTIONS=$CURRENT_ELECTIONS
done

echo ""
echo "=========================================="
echo "Test Complete - Results"
echo "=========================================="

# Final metrics
TOTAL_ELECTIONS=$(grep -h "Became Raft leader" node*.log 2>/dev/null | wc -l || echo 0)
MAX_TERM=$(grep -h "Became Raft leader" node*.log 2>/dev/null | \
    awk '{for(j=1;j<=NF;j++) if($j~"term") print $j}' | \
    sed 's/term=//' | sed 's/,$//' | sort -n | tail -1 || echo 1)
LOWER_TERM=$(grep -h "ignored a message with lower term" node*.log 2>/dev/null | wc -l || echo 0)

echo "Total elections: $TOTAL_ELECTIONS"
echo "Max term reached: $MAX_TERM"
echo "Lower term errors: $LOWER_TERM"

echo ""
echo "Pass/Fail:"

PASS=true

# Expected: ~12 elections for 3 partitions × 3 replicas + __meta
if [ "$TOTAL_ELECTIONS" -le 25 ]; then
    echo "✅ Election count: $TOTAL_ELECTIONS ≤ 25 (acceptable)"
else
    echo "❌ Too many elections: $TOTAL_ELECTIONS > 25"
    PASS=false
fi

if [ "$MAX_TERM" -le 5 ]; then
    echo "✅ Term stability: max term $MAX_TERM ≤ 5"
else
    echo "❌ Term growth: max term $MAX_TERM > 5 (churn detected)"
    PASS=false
fi

if [ "$LOWER_TERM" -le 50 ]; then
    echo "✅ Lower term errors: $LOWER_TERM ≤ 50"
else
    echo "⚠️ High lower term errors: $LOWER_TERM > 50"
    # Don't fail on this alone
fi

echo ""
if [ "$PASS" = true ]; then
    echo "=========================================="
    echo "✅ PHASE 1 SUCCESS!"
    echo "=========================================="
    echo ""
    echo "The new Raft configuration successfully stabilizes elections."
    echo "Election churn has been eliminated."
    echo ""
    echo "Ready to proceed to Phase 2 (non-blocking ready())."
    EXIT_CODE=0
else
    echo "=========================================="
    echo "❌ PHASE 1 INCOMPLETE"
    echo "=========================================="
    echo ""
    echo "Election churn still present. Review logs:"
    echo "  - node1.log, node2.log, node3.log"
    echo ""
    echo "Common issues:"
    echo "  - State machine blocking (Phase 2 will address)"
    echo "  - Network/gRPC delays"
    EXIT_CODE=1
fi

exit $EXIT_CODE
