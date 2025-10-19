#!/bin/bash

pkill -f chronik-server || true
sleep 2

echo "Starting 3-node cluster..."
RUST_LOG=info,chronik_raft=debug ./target/release/chronik-server \
  --advertised-addr localhost --kafka-port 9092 \
  --cluster-config node1-cluster.toml --node-id 1 \
  standalone > node1.log 2>&1 &
N1=$!

RUST_LOG=info,chronik_raft=debug ./target/release/chronik-server \
  --advertised-addr localhost --kafka-port 9093 \
  --cluster-config node2-cluster.toml --node-id 2 \
  standalone > node2.log 2>&1 &
N2=$!

RUST_LOG=info,chronik_raft=debug ./target/release/chronik-server \
  --advertised-addr localhost --kafka-port 9094 \
  --cluster-config node3-cluster.toml --node-id 3 \
  standalone > node3.log 2>&1 &
N3=$!

echo "Nodes started: $N1, $N2, $N3"

# Monitor for 60 seconds
for i in {1..12}; do
  sleep 5
  echo ""
  echo "=== $((i*5))s: Cluster Status ==="
  
  echo "Raft ports:"
  lsof -i :9192 -i :9193 -i :9194 2>/dev/null | grep LISTEN | awk '{print $9}'
  
  echo "Kafka ports:"
  lsof -i :9092 -i :9093 -i :9094 2>/dev/null | grep LISTEN | awk '{print $9}' || echo "NONE listening"
  
  echo "Leader status:"
  grep "became leader\|leader=" node*.log | tail -3
  
  echo "Commit status:"
  grep "commit=" node*.log | tail -5 | grep -v "commit=0" || echo "Still commit=0"
  
  echo "Broker registration:"
  grep "Successfully registered broker\|Failed to register broker" node*.log | tail -3
  
  echo "Errors:"
  grep -i "error\|fatal" node*.log | tail -3
done

echo ""
echo "=== Final Status After 60s ==="
grep "commit=" node*.log | tail -10
grep "broker.*register" node*.log | tail -10

kill $N1 $N2 $N3 2>/dev/null
wait 2>/dev/null
