#!/bin/bash
# Check health of 3-node Chronik Raft cluster
# Usage: ./scripts/check_cluster_health.sh

set -e

# Navigate to repo root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
cd "$REPO_ROOT"

echo "Checking Chronik Raft Cluster Health"
echo "====================================="

HEALTHY=true

# Check each node
for node_id in 1 2 3; do
    port=$((9092 + node_id - 1))
    pid_file="/tmp/chronik-node${node_id}.pid"

    echo ""
    echo "Node ${node_id} (localhost:${port}):"

    # Check if process is running
    if [[ ! -f "$pid_file" ]]; then
        echo "  Status: NOT RUNNING (PID file missing)"
        HEALTHY=false
        continue
    fi

    pid=$(cat "$pid_file")
    if ! ps -p $pid > /dev/null 2>&1; then
        echo "  Status: NOT RUNNING (process $pid not found)"
        HEALTHY=false
        continue
    fi

    echo "  Status: RUNNING (PID $pid)"

    # Try to query metrics endpoint (if available)
    # Note: Chronik may not expose Prometheus metrics by default
    # This is a placeholder for future metrics integration

    # Check if node is listening on Kafka port
    if nc -z localhost $port 2>/dev/null; then
        echo "  Kafka Port: LISTENING on $port"
    else
        echo "  Kafka Port: NOT LISTENING on $port"
        HEALTHY=false
    fi

    # Check Raft port (Kafka port + 100)
    raft_port=$((port + 100))
    if nc -z localhost $raft_port 2>/dev/null; then
        echo "  Raft Port: LISTENING on $raft_port"
    else
        echo "  Raft Port: NOT LISTENING on $raft_port"
        HEALTHY=false
    fi

    # Show recent log entries (last 5 lines)
    log_file="/tmp/chronik-node${node_id}.log"
    if [[ -f "$log_file" ]]; then
        echo "  Recent Logs:"
        tail -n 3 "$log_file" | sed 's/^/    /'
    fi
done

echo ""
echo "====================================="

# Try to query cluster using kafka-python (if available)
if command -v python3 &> /dev/null; then
    echo ""
    echo "Querying cluster metadata..."

    python3 -c "
import sys
try:
    from kafka import KafkaAdminClient
    admin = KafkaAdminClient(bootstrap_servers='localhost:9092', request_timeout_ms=5000)
    metadata = admin._client.cluster
    print(f'  Cluster ID: {metadata.cluster_id()}')
    print(f'  Controller: Node {metadata.controller().id}')
    print(f'  Brokers: {len(metadata.brokers())}')
    for broker in metadata.brokers():
        print(f'    - Node {broker.nodeId}: {broker.host}:{broker.port}')
    admin.close()
except ImportError:
    print('  (kafka-python not installed, skipping metadata query)')
except Exception as e:
    print(f'  ERROR: Failed to query cluster: {e}')
    sys.exit(1)
" || HEALTHY=false
fi

echo ""
echo "====================================="

if $HEALTHY; then
    echo "Cluster Status: HEALTHY ✓"
    exit 0
else
    echo "Cluster Status: UNHEALTHY ✗"
    echo ""
    echo "Troubleshooting:"
    echo "  - Check logs: tail -f /tmp/chronik-node*.log"
    echo "  - Restart cluster: ./scripts/stop_cluster.sh && ./scripts/start_3node_cluster.sh --clean"
    exit 1
fi
