#!/bin/bash
# Start Chronik Raft Cluster - Node 2
# Kafka: 9093, Raft: 9292

# Clean up old data
rm -rf ./data-node2

# Start Node 2
RUST_LOG=info,chronik_raft=debug,chronik_server=debug \
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  --data-dir ./data-node2 \
  raft-cluster \
    --raft-addr 0.0.0.0:9292 \
    --peers "1@localhost:9192,3@localhost:9392"
