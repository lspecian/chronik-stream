#!/bin/bash
# Start Chronik Raft Cluster - Node 3
# Kafka: 9094, Raft: 9392

# Clean up old data
rm -rf ./data-node3

# Start Node 3
RUST_LOG=info,chronik_raft=debug,chronik_server=debug \
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  --data-dir ./data-node3 \
  raft-cluster \
    --raft-addr 0.0.0.0:9392 \
    --peers "1@localhost:9192,2@localhost:9292"
