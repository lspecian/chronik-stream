#!/bin/bash
# Manual 3-node Raft cluster test script
# Usage: ./test_cluster_manual.sh [start|stop|restart|status]

set -e

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="${PROJECT_DIR}/target/release/chronik-server"
DATA_DIR="${PROJECT_DIR}/test-cluster-data"

# Node configurations
NODE1_ID=1
NODE1_KAFKA_PORT=9092
NODE1_RAFT_PORT=5001
NODE1_DATA="${DATA_DIR}/node1"
NODE1_PID_FILE="${DATA_DIR}/node1.pid"

NODE2_ID=2
NODE2_KAFKA_PORT=9093
NODE2_RAFT_PORT=5002
NODE2_DATA="${DATA_DIR}/node2"
NODE2_PID_FILE="${DATA_DIR}/node2.pid"

NODE3_ID=3
NODE3_KAFKA_PORT=9094
NODE3_RAFT_PORT=5003
NODE3_DATA="${DATA_DIR}/node3"
NODE3_PID_FILE="${DATA_DIR}/node3.pid"

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

check_binary() {
    if [ ! -f "$BINARY" ]; then
        log_error "Binary not found at $BINARY"
        log_info "Building with: cargo build --release --bin chronik-server --features raft"
        cargo build --release --bin chronik-server --features raft
    fi
    log_info "Binary found: $BINARY"
}

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

start_node() {
    local node_id=$1
    local kafka_port=$2
    local raft_port=$3
    local data_dir=$4
    local pid_file=$5

    if [ -f "$pid_file" ]; then
        local pid=$(cat "$pid_file")
        if kill -0 "$pid" 2>/dev/null; then
            log_warn "Node $node_id already running with PID $pid"
            return
        fi
    fi

    log_info "Starting Node $node_id (Kafka: $kafka_port, Raft: $raft_port)..."

    # Create data directory and config
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

    # Calculate unique metrics port (avoid conflicts)
    local metrics_port=$((9100 + node_id))

    # Start node in background
    RUST_LOG=info \
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

    local pid=$!
    echo "$pid" > "$pid_file"

    log_info "Node $node_id started with PID $pid"
    log_info "  Data dir: $data_dir"
    log_info "  Log file: $data_dir/node$node_id.log"
}

stop_node() {
    local node_id=$1
    local pid_file=$2

    if [ ! -f "$pid_file" ]; then
        log_warn "Node $node_id not running (no PID file)"
        return
    fi

    local pid=$(cat "$pid_file")
    if ! kill -0 "$pid" 2>/dev/null; then
        log_warn "Node $node_id not running (stale PID $pid)"
        rm -f "$pid_file"
        return
    fi

    log_info "Stopping Node $node_id (PID $pid)..."
    kill "$pid" 2>/dev/null || true

    # Wait for graceful shutdown (max 5 seconds)
    local count=0
    while kill -0 "$pid" 2>/dev/null && [ $count -lt 50 ]; do
        sleep 0.1
        count=$((count + 1))
    done

    if kill -0 "$pid" 2>/dev/null; then
        log_warn "Force killing Node $node_id..."
        kill -9 "$pid" 2>/dev/null || true
    fi

    rm -f "$pid_file"
    log_info "Node $node_id stopped"
}

show_status() {
    log_info "Cluster Status:"
    echo ""

    for node_id in 1 2 3; do
        local pid_file="${DATA_DIR}/node${node_id}.pid"
        if [ -f "$pid_file" ]; then
            local pid=$(cat "$pid_file")
            if kill -0 "$pid" 2>/dev/null; then
                echo -e "  Node $node_id: ${GREEN}RUNNING${NC} (PID $pid)"
            else
                echo -e "  Node $node_id: ${RED}STOPPED${NC} (stale PID)"
            fi
        else
            echo -e "  Node $node_id: ${RED}STOPPED${NC}"
        fi
    done

    echo ""
    log_info "Log files:"
    for node_id in 1 2 3; do
        local log_file="${DATA_DIR}/node${node_id}/node${node_id}.log"
        if [ -f "$log_file" ]; then
            echo "  Node $node_id: $log_file"
        fi
    done
}

show_logs() {
    local node_id=$1
    local log_file="${DATA_DIR}/node${node_id}/node${node_id}.log"

    if [ ! -f "$log_file" ]; then
        log_error "Log file not found: $log_file"
        return 1
    fi

    log_info "Showing logs for Node $node_id (last 50 lines):"
    tail -50 "$log_file"
}

case "${1:-start}" in
    start)
        log_info "Starting 3-node Raft cluster..."
        check_binary
        mkdir -p "$DATA_DIR"

        start_node $NODE1_ID $NODE1_KAFKA_PORT $NODE1_RAFT_PORT "$NODE1_DATA" "$NODE1_PID_FILE"
        sleep 2
        start_node $NODE2_ID $NODE2_KAFKA_PORT $NODE2_RAFT_PORT "$NODE2_DATA" "$NODE2_PID_FILE"
        sleep 2
        start_node $NODE3_ID $NODE3_KAFKA_PORT $NODE3_RAFT_PORT "$NODE3_DATA" "$NODE3_PID_FILE"

        sleep 3
        show_status

        echo ""
        log_info "Cluster started! Waiting 5 seconds for leader election..."
        sleep 5

        echo ""
        log_info "Check logs with: ./test_cluster_manual.sh logs <node_id>"
        log_info "Stop cluster with: ./test_cluster_manual.sh stop"
        ;;

    stop)
        log_info "Stopping 3-node Raft cluster..."
        stop_node $NODE3_ID "$NODE3_PID_FILE"
        stop_node $NODE2_ID "$NODE2_PID_FILE"
        stop_node $NODE1_ID "$NODE1_PID_FILE"
        log_info "Cluster stopped"
        ;;

    restart)
        $0 stop
        sleep 2
        $0 start
        ;;

    status)
        show_status
        ;;

    logs)
        if [ -z "$2" ]; then
            log_error "Usage: $0 logs <node_id>"
            exit 1
        fi
        show_logs "$2"
        ;;

    clean)
        $0 stop
        log_info "Cleaning up cluster data..."
        rm -rf "$DATA_DIR"
        log_info "Cleanup complete"
        ;;

    *)
        echo "Usage: $0 {start|stop|restart|status|logs <node_id>|clean}"
        exit 1
        ;;
esac
