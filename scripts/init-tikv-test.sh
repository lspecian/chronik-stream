#!/bin/bash
# Script to initialize TiKV test cluster

set -e

echo "Checking if TiKV test cluster is running..."
if ! docker-compose -f docker-compose.test.yml ps | grep -q "Up"; then
    echo "Starting TiKV test cluster..."
    docker-compose -f docker-compose.test.yml up -d
fi

echo "Waiting for PD to be ready..."
for i in {1..30}; do
    if curl -s http://localhost:2379/pd/api/v1/config/cluster-version > /dev/null 2>&1; then
        echo "PD is ready!"
        break
    fi
    echo -n "."
    sleep 2
done
echo

# Bootstrap the cluster if needed
echo "Checking cluster bootstrap status..."
BOOTSTRAP_STATUS=$(curl -s http://localhost:2379/pd/api/v1/config/cluster-version | grep -o '"cluster-version"' || echo "not-found")

if [[ "$BOOTSTRAP_STATUS" == "not-found" ]]; then
    echo "Cluster needs bootstrapping..."
    # TiKV should auto-bootstrap, just wait more
    sleep 10
fi

echo "TiKV test cluster is ready!"

# Show cluster status
echo "Cluster status:"
curl -s http://localhost:2379/pd/api/v1/cluster/status | jq . || echo "Status check failed"