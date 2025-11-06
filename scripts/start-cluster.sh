#!/bin/bash
# Start a 3-node Chronik cluster for testing

set -e

killall -9 chronik-server 2>/dev/null || true
sleep 2
rm -rf ./data-cluster-test

./target/release/chronik-server start --config data-cluster-test-node1.toml > node1.log 2>&1 &
sleep 3
./target/release/chronik-server start --config data-cluster-test-node2.toml > node2.log 2>&1 &
sleep 3
./target/release/chronik-server start --config data-cluster-test-node3.toml > node3.log 2>&1 &
sleep 3

NODES=$(ps aux | grep "[c]hronik-server" | wc -l)
echo "3-node cluster started: $NODES nodes running"

if [ "$NODES" -ne 3 ]; then
  echo "ERROR: Expected 3 nodes, got $NODES"
  exit 1
fi

echo "Cluster ready on ports 9092, 9093, 9094"
