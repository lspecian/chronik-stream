#!/bin/bash
# Start Chronik Raft Cluster - Node 1
# Kafka: 9092, Raft: 9192

# Clean up old data
rm -rf ./data-node1

# Start Node 1
RUST_LOG=info,chronik_raft=debug,chronik_server=debug \
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  --data-dir ./data-node1 \
  raft-cluster \
    --raft-addr 0.0.0.0:9192 \
    --peers "2@localhost:9292,3@localhost:9392" \
    --bootstrap
