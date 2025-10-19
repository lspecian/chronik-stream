#!/bin/bash

# Start just one node with debug logging
RUST_LOG=chronik_raft=debug,chronik_server::raft_integration=debug,info \
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=1 \
CHRONIK_REPLICATION_FACTOR=3 \
CHRONIK_MIN_INSYNC_REPLICAS=2 \
CHRONIK_CLUSTER_PEERS="localhost:9092:9093,localhost:9192:9193,localhost:9292:9293" \
./target/release/chronik-server \
  --kafka-port 9092 \
  --metrics-port 8093 \
  --bind-addr 0.0.0.0 \
  --advertised-addr localhost \
  --data-dir ./node1_data \
  standalone
