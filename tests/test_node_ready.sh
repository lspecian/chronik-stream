#!/usr/bin/env bash
#
# Minimal test to verify Chronik cluster mode connection.
# Replaces test_node_ready.py — no Python dependency.
#
set -euo pipefail

SERVERS=("localhost:9092" "localhost:9093" "localhost:9094")

echo "Testing Chronik cluster mode connection..."
echo "============================================================"

echo ""
echo "1. Checking Kafka ports..."
all_ok=true
for srv in "${SERVERS[@]}"; do
  host="${srv%%:*}"
  port="${srv##*:}"
  if nc -z "$host" "$port" 2>/dev/null; then
    echo "   $srv — OK"
  else
    echo "   $srv — FAILED (connection refused)"
    all_ok=false
  fi
done

echo ""
echo "2. Checking Unified API health..."
for port in 6092 6093 6094; do
  resp=$(curl -s --connect-timeout 3 "http://localhost:$port/health" 2>/dev/null || echo '{"status":"unreachable"}')
  status=$(echo "$resp" | grep -o '"status":"[^"]*"' | head -1 || echo "unknown")
  echo "   localhost:$port — $status"
done

echo ""
echo "3. Producing a test message..."
# Use kcat/kafkacat if available, otherwise curl to unified API
if command -v kcat &>/dev/null; then
  echo '{"test":"node_ready","ts":'$(date +%s)'}' | kcat -P -b localhost:9092 -t _test_node_ready 2>/dev/null
  echo "   Produced via kcat"
elif command -v kafkacat &>/dev/null; then
  echo '{"test":"node_ready","ts":'$(date +%s)'}' | kafkacat -P -b localhost:9092 -t _test_node_ready 2>/dev/null
  echo "   Produced via kafkacat"
else
  echo "   Skipped (kcat/kafkacat not installed)"
fi

echo ""
if $all_ok; then
  echo "Test PASSED — all nodes reachable"
  exit 0
else
  echo "Test FAILED — some nodes unreachable"
  exit 1
fi
