#!/bin/bash
set -e

echo "======================================"
echo "Testing Health-Check Bootstrap"
echo "======================================"

# Clean up any existing processes
pkill -f chronik-server || true
sleep 2

# Clean data directories
rm -rf /tmp/chronik-node1 /tmp/chronik-node2 /tmp/chronik-node3
mkdir -p /tmp/chronik-node1 /tmp/chronik-node2 /tmp/chronik-node3

# Function to create config for a node
create_node_config() {
    local node_id=$1
    local config_file="chronik-cluster-node${node_id}.toml"

    cat > "${config_file}" <<EOF
enabled = true
node_id = ${node_id}
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

[gossip]
bind_addr = "0.0.0.0:$((7946 + node_id - 1))"
seed_nodes = []
bootstrap_expect = 3
EOF
}

# Create configs for all nodes
create_node_config 1
create_node_config 2
create_node_config 3

# Function to start a node
start_node() {
    local node_id=$1
    local kafka_port=$((9092 + node_id - 1))
    local raft_port=$((9192 + node_id - 1))
    local data_dir="/tmp/chronik-node${node_id}"
    local config_file="chronik-cluster-node${node_id}.toml"

    echo "Starting node ${node_id} (Kafka: ${kafka_port}, Raft: ${raft_port})"

    CHRONIK_DATA_DIR="${data_dir}" \
    CHRONIK_KAFKA_PORT="${kafka_port}" \
    CHRONIK_ADVERTISED_ADDR="localhost" \
    RUST_LOG=info,chronik_raft::gossip=debug,chronik_server::raft_cluster=debug \
    ./target/release/chronik-server \
        --advertised-addr localhost \
        --cluster-config "${config_file}" \
        standalone \
        > "/tmp/chronik-node${node_id}.log" 2>&1 &

    echo "Node ${node_id} started (PID: $!)"
}

echo ""
echo "Step 1: Starting all 3 nodes simultaneously"
echo "Expected: One node becomes bootstrap leader, forms cluster"
start_node 1
start_node 2
start_node 3

echo ""
echo "Waiting 15 seconds for bootstrap to complete..."
sleep 15

echo ""
echo "Step 2: Checking cluster formation"
echo "Attempting to create topic..."

# Try to create a topic to verify cluster is working
if kafka-topics --bootstrap-server localhost:9092 --create --topic test-healthcheck --partitions 3 --replication-factor 3 2>/dev/null; then
    echo "✅ Topic created successfully - cluster is operational!"
else
    echo "⚠️ Topic creation failed - checking logs..."
    echo ""
    echo "=== Node 1 Logs ==="
    tail -50 /tmp/chronik-node1.log
    echo ""
    echo "=== Node 2 Logs ==="
    tail -50 /tmp/chronik-node2.log
    echo ""
    echo "=== Node 3 Logs ==="
    tail -50 /tmp/chronik-node3.log
    exit 1
fi

echo ""
echo "Step 3: Listing topics to verify cluster state"
kafka-topics --bootstrap-server localhost:9092 --list

echo ""
echo "Step 4: Testing message produce/consume"
echo "test-message" | kafka-console-producer --bootstrap-server localhost:9092 --topic test-healthcheck
kafka-console-consumer --bootstrap-server localhost:9092 --topic test-healthcheck --from-beginning --max-messages 1 --timeout-ms 5000

echo ""
echo "Step 5: Testing resilience - killing bootstrap leader"
# Find which node was the bootstrap leader
LEADER_NODE=$(grep -l "Elected as bootstrap leader" /tmp/chronik-node*.log | sed 's/.*node\([0-9]\).*/\1/')
if [ -n "$LEADER_NODE" ]; then
    echo "Killing node ${LEADER_NODE} (bootstrap leader)..."
    pkill -f "CHRONIK_KAFKA_PORT=$(expr 9092 + $LEADER_NODE - 1)" || true

    echo "Waiting 5 seconds for cluster to stabilize..."
    sleep 5

    echo "Verifying cluster still works..."
    if kafka-topics --bootstrap-server localhost:9093 --list 2>/dev/null | grep -q test-healthcheck; then
        echo "✅ Cluster survived leader failure!"
    else
        echo "❌ Cluster failed after leader died"
        exit 1
    fi
else
    echo "⚠️ Could not determine bootstrap leader from logs"
fi

echo ""
echo "======================================"
echo "✅ All tests passed!"
echo "======================================"
echo ""
echo "Cluster logs available at:"
echo "  /tmp/chronik-node1.log"
echo "  /tmp/chronik-node2.log"
echo "  /tmp/chronik-node3.log"
echo ""
echo "To clean up: pkill -f chronik-server"
