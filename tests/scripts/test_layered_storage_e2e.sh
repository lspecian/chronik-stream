#!/bin/bash
# End-to-End test for layered storage with Raft clustering
# This test verifies WAL → S3 upload works correctly in cluster mode

set -e

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
DATA_DIR="${PROJECT_DIR}/test-cluster-data-layered"
BINARY="${PROJECT_DIR}/target/release/chronik-server"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

# Clean up any existing test data
cleanup() {
    log_info "Cleaning up test cluster..."
    pkill -f "chronik-server.*909[2-4]" 2>/dev/null || true
    sleep 2
    rm -rf "$DATA_DIR"
    mkdir -p "$DATA_DIR"
    log_success "Cleanup complete"
}

# Start 3-node cluster with SMALL WAL rotation threshold
start_cluster() {
    log_info "Starting 3-node cluster with 1MB WAL rotation threshold..."

    # Load Hetzner S3 credentials
    if [ -f "${PROJECT_DIR}/.env.hetzner" ]; then
        log_info "Loading Hetzner S3 credentials from .env.hetzner"
        source "${PROJECT_DIR}/.env.hetzner"

        # Add timestamp to prefix to keep test runs separate
        export S3_PREFIX="${S3_PREFIX}run-$(date +%Y%m%d-%H%M%S)/"

        log_info "S3 Configuration:"
        log_info "  Backend: $OBJECT_STORE_BACKEND"
        log_info "  Endpoint: $S3_ENDPOINT"
        log_info "  Region: $S3_REGION"
        log_info "  Bucket: $S3_BUCKET"
        log_info "  Prefix: $S3_PREFIX"
    else
        log_warn "No Hetzner credentials found (.env.hetzner), using local storage"
        export OBJECT_STORE_BACKEND=local
    fi

    # CRITICAL: Small WAL rotation to trigger sealing quickly
    export CHRONIK_WAL_ROTATION_SIZE=1MB

    for i in 1 2 3; do
        node_data_dir="${DATA_DIR}/node${i}"
        mkdir -p "$node_data_dir"

        kafka_port=$((9091 + i))
        raft_port=$((5000 + i))
        metrics_port=$((9100 + i))

        # Build peer list (exclude self)
        peers=""
        for j in 1 2 3; do
            if [ $j -ne $i ]; then
                peer_raft_port=$((5000 + j))
                [ -n "$peers" ] && peers="${peers},"
                peers="${peers}${j}@localhost:${peer_raft_port}"
            fi
        done

        log_info "Starting node $i (Kafka: $kafka_port, Raft: $raft_port)..."

        RUST_LOG=info,chronik_storage::wal_indexer=debug \
        CHRONIK_DATA_DIR="$node_data_dir" \
        CHRONIK_KAFKA_PORT="$kafka_port" \
        CHRONIK_RAFT_ADDR="0.0.0.0:$raft_port" \
        OBJECT_STORE_BACKEND="$OBJECT_STORE_BACKEND" \
        S3_ENDPOINT="$S3_ENDPOINT" \
        S3_REGION="$S3_REGION" \
        S3_BUCKET="$S3_BUCKET" \
        S3_ACCESS_KEY="$S3_ACCESS_KEY" \
        S3_SECRET_KEY="$S3_SECRET_KEY" \
        S3_PATH_STYLE="$S3_PATH_STYLE" \
        S3_DISABLE_SSL="$S3_DISABLE_SSL" \
        S3_PREFIX="$S3_PREFIX" \
        nohup "$BINARY" \
            --node-id "$i" \
            --kafka-port "$kafka_port" \
            --metrics-port "$metrics_port" \
            --advertised-addr "localhost" \
            --advertised-port "$kafka_port" \
            raft-cluster \
            --raft-addr "0.0.0.0:$raft_port" \
            --peers "$peers" \
            --bootstrap \
            > "$node_data_dir/node${i}.log" 2>&1 &

        echo $! > "$node_data_dir/node${i}.pid"
        log_success "Node $i started (PID: $(cat $node_data_dir/node${i}.pid))"
    done

    log_info "Waiting 15 seconds for cluster to stabilize..."
    sleep 15
    log_success "Cluster ready"
}

# Produce enough data to seal WAL segments (> 1MB)
produce_data() {
    log_info "Producing data to trigger WAL sealing (target: > 1MB)..."

    python3 <<EOF
from kafka import KafkaProducer, KafkaAdminClient
from kafka.admin import NewTopic
import time

# Create topic
admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic(name='layered-test', num_partitions=3, replication_factor=1)
try:
    admin.create_topics([topic])
    print('✓ Topic created')
except Exception as e:
    print(f'Topic creation: {e}')
admin.close()

time.sleep(3)  # Wait for replicas

# Produce 2MB of data (2000 messages * 1KB each)
producer = KafkaProducer(bootstrap_servers='localhost:9092')
message_size = 1024  # 1KB
total_messages = 6000

print(f'Producing {total_messages} messages of {message_size} bytes each...')

for i in range(total_messages):
    payload = ('x' * (message_size - 50)) + f' offset={i}'
    producer.send('layered-test', value=payload.encode())
    if (i + 1) % 200 == 0:
        print(f'  Sent {i + 1}/{total_messages} messages...')

producer.flush()
producer.close()
print(f'✓ All {total_messages} messages sent (total: ~{total_messages * message_size / 1024 / 1024:.1f} MB)')
EOF

    log_success "Data production complete"
}

# Wait for WalIndexer to run and upload
wait_for_indexing() {
    log_info "Waiting 45 seconds for WalIndexer to seal and upload segments..."
    log_info "  (WalIndexer runs every 30 seconds)"

    for i in {45..1}; do
        printf "\r  ${BLUE}[INFO]${NC} Waiting... %2d seconds remaining" $i
        sleep 1
    done
    echo

    log_success "WalIndexer should have completed at least one cycle"
}

# Check for sealed segments and S3 uploads
verify_uploads() {
    log_info "Verifying segment uploads..."

    # Check each node
    for i in 1 2 3; do
        node_data_dir="${DATA_DIR}/node${i}"

        echo
        log_info "Node $i:"

        # Check WAL directory
        wal_count=$(find "$node_data_dir/wal" -name "*.log" 2>/dev/null | wc -l)
        log_info "  WAL files: $wal_count"

        # Check segments directory
        if [ -d "$node_data_dir/segments" ]; then
            segment_count=$(find "$node_data_dir/segments" -type f 2>/dev/null | wc -l)
            log_info "  Local segments: $segment_count"
        fi

        # Check object store
        if [ -d "$node_data_dir/object_store" ]; then
            object_count=$(find "$node_data_dir/object_store" -type f 2>/dev/null | wc -l)
            object_size=$(du -sh "$node_data_dir/object_store" 2>/dev/null | cut -f1)
            log_info "  Object store files: $object_count (size: ${object_size:-0})"

            if [ $object_count -gt 0 ]; then
                log_success "  ✓ Node $i has uploaded segments to object store!"
                # Show some files
                find "$node_data_dir/object_store" -type f 2>/dev/null | head -5 | while read file; do
                    log_info "    - $(basename $file)"
                done
            fi
        fi
    done
}

# Check WalIndexer logs
check_logs() {
    echo
    log_info "Checking WalIndexer activity in logs..."

    for i in 1 2 3; do
        node_log="${DATA_DIR}/node${i}/node${i}.log"
        if [ -f "$node_log" ]; then
            indexer_logs=$(grep -i "walindexer\|indexed\|sealed" "$node_log" 2>/dev/null || true)
            if [ -n "$indexer_logs" ]; then
                echo
                log_info "Node $i WalIndexer activity:"
                echo "$indexer_logs" | tail -10
            fi
        fi
    done
}

# Main test flow
main() {
    echo
    echo "================================================================"
    echo "Layered Storage End-to-End Test with Raft Clustering"
    echo "================================================================"
    echo

    cleanup
    start_cluster
    produce_data
    wait_for_indexing
    verify_uploads
    check_logs

    echo
    echo "================================================================"
    echo "Test Complete"
    echo "================================================================"
    echo
    log_info "Cluster is still running. To stop:"
    log_info "  pkill -f 'chronik-server.*909[2-4]'"
    echo
    log_info "To check logs:"
    log_info "  tail -f ${DATA_DIR}/node1/node1.log"
    echo
}

main "$@"
