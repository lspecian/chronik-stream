#!/bin/bash
set -e

echo "=============================================="
echo " Testing Broker Registration in Cluster Mode"
echo "=============================================="
echo ""

# Kill any existing servers
echo "1. Cleaning up old processes..."
pkill -9 chronik-server 2>/dev/null || true
sleep 2

# Clean data directories
echo "2. Cleaning data directories..."
rm -rf data/ node1-data/ node2-data/ node3-data/ 2>/dev/null || true

# Start 3-node cluster
echo "3. Starting 3-node cluster..."
echo "   Starting Node 1 (port 9092, Raft port 5001)..."
./target/release/chronik-server \
  --advertised-addr localhost \
  --advertised-port 9092 \
  --node-id 1 \
  --data-dir node1-data \
  raft-cluster \
  --raft-addr 0.0.0.0:5001 \
  --peers "2@localhost:5002,3@localhost:5003" \
  --bootstrap > /tmp/node1.log 2>&1 &
NODE1_PID=$!
echo "   Node 1 PID: $NODE1_PID"

sleep 3

echo "   Starting Node 2 (port 9093, Raft port 5002)..."
./target/release/chronik-server \
  --advertised-addr localhost \
  --advertised-port 9093 \
  --kafka-port 9093 \
  --node-id 2 \
  --data-dir node2-data \
  raft-cluster \
  --raft-addr 0.0.0.0:5002 \
  --peers "1@localhost:5001,3@localhost:5003" > /tmp/node2.log 2>&1 &
NODE2_PID=$!
echo "   Node 2 PID: $NODE2_PID"

sleep 3

echo "   Starting Node 3 (port 9094, Raft port 5003)..."
./target/release/chronik-server \
  --advertised-addr localhost \
  --advertised-port 9094 \
  --kafka-port 9094 \
  --node-id 3 \
  --data-dir node3-data \
  raft-cluster \
  --raft-addr 0.0.0.0:5003 \
  --peers "1@localhost:5001,2@localhost:5002" > /tmp/node3.log 2>&1 &
NODE3_PID=$!
echo "   Node 3 PID: $NODE3_PID"

echo ""
echo "4. Waiting for cluster to form and brokers to register (20 seconds)..."
sleep 20

echo ""
echo "5. Checking broker registration in logs..."
echo ""
echo "Node 1 broker registration:"
grep -E "(Registering broker|✓ Successfully registered|✓ Broker.*confirmed visible)" /tmp/node1.log | tail -5 || echo "  No registration logs found"
echo ""
echo "Node 2 broker registration:"
grep -E "(Registering broker|✓ Successfully registered|✓ Broker.*confirmed visible)" /tmp/node2.log | tail -5 || echo "  No registration logs found"
echo ""
echo "Node 3 broker registration:"
grep -E "(Registering broker|✓ Successfully registered|✓ Broker.*confirmed visible)" /tmp/node3.log | tail -5 || echo "  No registration logs found"

echo ""
echo "6. Checking for errors..."
echo "Node 1 errors:"
grep -i "error\|critical\|fatal" /tmp/node1.log | grep -v "ERROR_ACCUMULATOR\|error_handler" | tail -3 || echo "  No errors"
echo ""
echo "Node 2 errors:"
grep -i "error\|critical\|fatal" /tmp/node2.log | grep -v "ERROR_ACCUMULATOR\|error_handler" | tail -3 || echo "  No errors"
echo ""
echo "Node 3 errors:"
grep -i "error\|critical\|fatal" /tmp/node3.log | grep -v "ERROR_ACCUMULATOR\|error_handler" | tail -3 || echo "  No errors"

echo ""
echo "7. Testing metadata query with Python..."
cat > /tmp/test_cluster_metadata.py <<'EOF'
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient
import time

# Connect to Node 1
try:
    # Simple producer to force metadata fetch
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        request_timeout_ms=10000,
        max_block_ms=10000
    )

    # Get cluster metadata
    metadata = producer._metadata

    # Wait for metadata to load
    time.sleep(1)

    # Get available brokers
    brokers = metadata.brokers()

    print(f"✓ Connected to cluster")
    print(f"  Brokers ({len(brokers)} total):")
    for broker_id in brokers:
        broker = metadata.broker_metadata(broker_id)
        if broker:
            print(f"    - Broker {broker.nodeId}: {broker.host}:{broker.port}")

    if len(brokers) == 0:
        print("❌ ERROR: No brokers found in metadata!")
        exit(1)
    elif len(brokers) < 3:
        print(f"⚠ WARNING: Expected 3 brokers, but found {len(brokers)}")
        exit(1)
    else:
        print("✅ SUCCESS: All 3 brokers registered and visible!")
        exit(0)

except Exception as e:
    print(f"❌ ERROR: Failed to connect: {e}")
    import traceback
    traceback.print_exc()
    exit(1)
EOF

python3 /tmp/test_cluster_metadata.py

echo ""
echo "8. Cleanup..."
echo "Stopping servers..."
kill $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
sleep 2
pkill -9 chronik-server 2>/dev/null || true

echo ""
echo "Test complete!"
echo "Log files:"
echo "  Node 1: /tmp/node1.log"
echo "  Node 2: /tmp/node2.log"
echo "  Node 3: /tmp/node3.log"
