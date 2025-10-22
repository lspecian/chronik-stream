#!/bin/bash
# Debug test runner for Raft cluster - captures detailed logs for diagnosing leader election timing
# This runs a quick test to see the wait_for_topic_leaders() behavior

set -e

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="${PROJECT_DIR}/target/release/chronik-server"
DATA_DIR="${PROJECT_DIR}/test-cluster-data"

# Node configurations
NODE1_ID=1
NODE1_KAFKA_PORT=9092
NODE1_RAFT_PORT=5001
NODE1_DATA="${DATA_DIR}/node1"

NODE2_ID=2
NODE2_KAFKA_PORT=9093
NODE2_RAFT_PORT=5002
NODE2_DATA="${DATA_DIR}/node2"

NODE3_ID=3
NODE3_KAFKA_PORT=9094
NODE3_RAFT_PORT=5003
NODE3_DATA="${DATA_DIR}/node3"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_test() { echo -e "${BLUE}[TEST]${NC} $1"; }

# Check if cluster is already running
if pgrep -f "chronik-server.*--node-id" > /dev/null; then
    log_error "Cluster appears to be running. Stop it first:"
    log_error "  ./test_cluster_manual.sh stop"
    exit 1
fi

create_cluster_config() {
    local node_id=$1
    local data_dir=$2

    mkdir -p "$data_dir/config"

    cat > "$data_dir/config/chronik-cluster.toml" <<EOF
enabled = true
node_id = $node_id
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "127.0.0.1:$NODE1_KAFKA_PORT"
raft_port = $NODE1_RAFT_PORT

[[peers]]
id = 2
addr = "127.0.0.1:$NODE2_KAFKA_PORT"
raft_port = $NODE2_RAFT_PORT

[[peers]]
id = 3
addr = "127.0.0.1:$NODE3_KAFKA_PORT"
raft_port = $NODE3_RAFT_PORT
EOF
}

start_node_debug() {
    local node_id=$1
    local kafka_port=$2
    local raft_port=$3
    local data_dir=$4

    log_info "Starting Node $node_id (Kafka: $kafka_port, Raft: $raft_port) with DEBUG logging..."

    mkdir -p "$data_dir"
    create_cluster_config "$node_id" "$data_dir"

    # Build peers list (exclude self)
    local peers=""
    if [ "$node_id" -ne 1 ]; then
        [ -n "$peers" ] && peers="${peers},"
        peers="${peers}1@localhost:$NODE1_RAFT_PORT"
    fi
    if [ "$node_id" -ne 2 ]; then
        [ -n "$peers" ] && peers="${peers},"
        peers="${peers}2@localhost:$NODE2_RAFT_PORT"
    fi
    if [ "$node_id" -ne 3 ]; then
        [ -n "$peers" ] && peers="${peers},"
        peers="${peers}3@localhost:$NODE3_RAFT_PORT"
    fi

    local metrics_port=$((9100 + node_id))

    # CRITICAL: Use DEBUG logging for kafka_handler to see wait_for_topic_leaders()
    RUST_LOG=chronik_server::kafka_handler=debug,chronik_server::raft_integration=debug,info \
    CHRONIK_DATA_DIR="$data_dir" \
    CHRONIK_KAFKA_PORT="$kafka_port" \
    CHRONIK_RAFT_ADDR="0.0.0.0:$raft_port" \
    nohup "$BINARY" \
        --node-id "$node_id" \
        --kafka-port "$kafka_port" \
        --metrics-port "$metrics_port" \
        --advertised-addr "localhost" \
        --advertised-port "$kafka_port" \
        raft-cluster \
        --raft-addr "0.0.0.0:$raft_port" \
        --peers "$peers" \
        --bootstrap \
        > "$data_dir/node$node_id.log" 2>&1 &

    echo $! > "$data_dir/node$node_id.pid"
    log_info "  Node $node_id started (PID $!)"
}

log_info "Starting 3-node cluster with DEBUG logging..."

# Clean old data
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

# Start nodes with debug logging
start_node_debug $NODE1_ID $NODE1_KAFKA_PORT $NODE1_RAFT_PORT "$NODE1_DATA"
sleep 2
start_node_debug $NODE2_ID $NODE2_KAFKA_PORT $NODE2_RAFT_PORT "$NODE2_DATA"
sleep 2
start_node_debug $NODE3_ID $NODE3_KAFKA_PORT $NODE3_RAFT_PORT "$NODE3_DATA"

log_info "Waiting 8 seconds for cluster to stabilize..."
sleep 8

log_test "Running Python test (100 messages, 3 partitions)..."
python3 test_cluster_kafka_python.py || log_error "Test failed!"

echo ""
log_info "Test complete! Analyzing logs for wait_for_topic_leaders behavior..."
echo ""

# Extract relevant logs from all 3 nodes
for node_id in 1 2 3; do
    log_file="${DATA_DIR}/node${node_id}/node${node_id}.log"
    if [ -f "$log_file" ]; then
        echo -e "${BLUE}=== Node $node_id: wait_for_topic_leaders Activity ===${NC}"
        grep -E "(wait_for_topic_leaders|replica exists|NO replica|partition leaders ready)" "$log_file" || echo "  (no wait_for_topic_leaders logs - topic may have been created elsewhere)"
        echo ""
    fi
done

echo ""
log_info "Checking for NotLeaderForPartitionError in produce logs..."
for node_id in 1 2 3; do
    log_file="${DATA_DIR}/node${node_id}/node${node_id}.log"
    if [ -f "$log_file" ]; then
        error_count=$(grep -c "NOT_LEADER_FOR_PARTITION" "$log_file" || echo "0")
        if [ "$error_count" -gt 0 ]; then
            echo -e "  Node $node_id: ${RED}$error_count errors${NC}"
        else
            echo -e "  Node $node_id: ${GREEN}0 errors${NC}"
        fi
    fi
done

echo ""
log_info "Full logs available at:"
for node_id in 1 2 3; do
    echo "  Node $node_id: ${DATA_DIR}/node${node_id}/node${node_id}.log"
done

echo ""
log_warn "Stopping cluster..."
pkill -f "chronik-server.*--node-id" || true
sleep 2

log_info "Done! Review logs above to understand the 24% failure pattern."
