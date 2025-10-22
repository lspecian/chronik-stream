#!/bin/bash
# Toxiproxy Infrastructure Setup for Chronik Cluster Chaos Testing
# This script sets up Toxiproxy proxies for a 3-node Chronik cluster to enable network fault injection

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOXIPROXY_API="http://localhost:8474"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Chronik Toxiproxy Infrastructure Setup${NC}"
echo -e "${GREEN}========================================${NC}"
echo ""

# Function to check if Toxiproxy server is running
check_toxiproxy() {
    if curl -s "${TOXIPROXY_API}/proxies" > /dev/null 2>&1; then
        echo -e "${GREEN}✓ Toxiproxy server is running${NC}"
        return 0
    else
        echo -e "${YELLOW}⚠ Toxiproxy server is not running${NC}"
        return 1
    fi
}

# Function to start Toxiproxy server
start_toxiproxy() {
    echo -e "${YELLOW}Starting Toxiproxy server...${NC}"
    toxiproxy-server > /tmp/toxiproxy.log 2>&1 &
    TOXIPROXY_PID=$!
    echo $TOXIPROXY_PID > /tmp/toxiproxy.pid
    sleep 2

    if check_toxiproxy; then
        echo -e "${GREEN}✓ Toxiproxy server started (PID: $TOXIPROXY_PID)${NC}"
    else
        echo -e "${RED}✗ Failed to start Toxiproxy server${NC}"
        exit 1
    fi
}

# Function to create proxy
create_proxy() {
    local name=$1
    local listen=$2
    local upstream=$3

    echo -e "${YELLOW}Creating proxy: $name${NC}"
    echo "  Listen: $listen → Upstream: $upstream"

    curl -s -X POST "${TOXIPROXY_API}/proxies" \
        -H "Content-Type: application/json" \
        -d "{
            \"name\": \"$name\",
            \"listen\": \"$listen\",
            \"upstream\": \"$upstream\",
            \"enabled\": true
        }" > /dev/null

    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✓ Proxy created: $name${NC}"
    else
        echo -e "${RED}✗ Failed to create proxy: $name${NC}"
    fi
}

# Function to delete all proxies (cleanup)
cleanup_proxies() {
    echo -e "${YELLOW}Cleaning up existing proxies...${NC}"

    PROXIES=$(curl -s "${TOXIPROXY_API}/proxies" | grep -o '"name":"[^"]*"' | cut -d'"' -f4)

    for proxy in $PROXIES; do
        echo "  Deleting proxy: $proxy"
        curl -s -X DELETE "${TOXIPROXY_API}/proxies/$proxy" > /dev/null
    done

    echo -e "${GREEN}✓ Cleanup complete${NC}"
}

# Main setup
main() {
    echo "Step 1: Check/Start Toxiproxy server"
    if ! check_toxiproxy; then
        start_toxiproxy
    fi
    echo ""

    echo "Step 2: Clean up existing proxies"
    cleanup_proxies
    echo ""

    echo "Step 3: Create proxies for Chronik cluster"
    echo ""

    # Kafka proxies (clients connect to these)
    echo "=== Kafka Proxies (for clients) ==="
    create_proxy "chronik-node1-kafka" "127.0.0.1:19092" "127.0.0.1:9092"
    create_proxy "chronik-node2-kafka" "127.0.0.1:19093" "127.0.0.1:9093"
    create_proxy "chronik-node3-kafka" "127.0.0.1:19094" "127.0.0.1:9094"
    echo ""

    # Raft gRPC proxies (inter-node communication)
    echo "=== Raft gRPC Proxies (inter-node) ==="
    create_proxy "chronik-node1-raft" "127.0.0.1:15001" "127.0.0.1:5001"
    create_proxy "chronik-node2-raft" "127.0.0.1:15002" "127.0.0.1:5002"
    create_proxy "chronik-node3-raft" "127.0.0.1:15003" "127.0.0.1:5003"
    echo ""

    echo "Step 4: Verify proxy setup"
    echo ""
    curl -s "${TOXIPROXY_API}/proxies" | python3 -m json.tool
    echo ""

    echo -e "${GREEN}========================================${NC}"
    echo -e "${GREEN}Setup Complete!${NC}"
    echo -e "${GREEN}========================================${NC}"
    echo ""
    echo "Toxiproxy API: ${TOXIPROXY_API}"
    echo ""
    echo "Kafka Proxies (for clients):"
    echo "  Node 1: localhost:19092 → localhost:9092"
    echo "  Node 2: localhost:19093 → localhost:9093"
    echo "  Node 3: localhost:19094 → localhost:9094"
    echo ""
    echo "Raft Proxies (inter-node):"
    echo "  Node 1: localhost:15001 → localhost:5001"
    echo "  Node 2: localhost:15002 → localhost:5002"
    echo "  Node 3: localhost:15003 → localhost:5003"
    echo ""
    echo "Usage:"
    echo "  - Start Chronik cluster normally (ports 9092-9094, 5001-5003)"
    echo "  - Connect Kafka clients to ports 19092-19094 (proxied)"
    echo "  - Use toxiproxy-cli to inject faults (latency, partition, etc.)"
    echo ""
    echo "Examples:"
    echo "  # Add 100ms latency to node 1"
    echo "  toxiproxy-cli toxic add chronik-node1-kafka -t latency -a latency=100"
    echo ""
    echo "  # Partition node 2 from cluster (Raft)"
    echo "  toxiproxy-cli toxic add chronik-node2-raft -t bandwidth -a rate=0"
    echo ""
    echo "  # Remove all toxics from node 1"
    echo "  toxiproxy-cli toxic remove chronik-node1-kafka -n latency_downstream"
    echo ""
    echo "  # List all proxies and toxics"
    echo "  toxiproxy-cli list"
    echo ""
}

# Cleanup handler
cleanup() {
    echo ""
    echo -e "${YELLOW}Shutting down Toxiproxy...${NC}"
    if [ -f /tmp/toxiproxy.pid ]; then
        kill $(cat /tmp/toxiproxy.pid) 2>/dev/null || true
        rm /tmp/toxiproxy.pid
    fi
}

trap cleanup EXIT

# Parse arguments
case "${1:-}" in
    start)
        main
        echo "Press Ctrl+C to stop Toxiproxy and cleanup proxies"
        tail -f /tmp/toxiproxy.log
        ;;
    stop)
        cleanup_proxies
        if [ -f /tmp/toxiproxy.pid ]; then
            kill $(cat /tmp/toxiproxy.pid) 2>/dev/null || true
            rm /tmp/toxiproxy.pid
            echo -e "${GREEN}✓ Toxiproxy stopped${NC}"
        fi
        ;;
    *)
        main
        echo ""
        echo "Run './test_toxiproxy_setup.sh start' to keep server running"
        echo "Run './test_toxiproxy_setup.sh stop' to stop server and cleanup"
        ;;
esac
