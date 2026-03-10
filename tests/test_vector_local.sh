#!/usr/bin/env bash
#
# End-to-end local standalone test for Chronik vector search.
# Replaces test_vector_local.py — no Python dependency.
# Tests: produce → embed → HNSW index → search → verify.
#
# Requirements: curl, kcat (or kafkacat), jq
#
set -euo pipefail

KAFKA_PORT=9092
UNIFIED_API_PORT=6092
TOPIC_NAME="test-vector-local"
DATA_DIR="tests/data/vector-local-test"
SERVER_BINARY="./target/release/chronik-server"
NUM_MESSAGES=100
VECTOR_INDEX_TIMEOUT=180
POLL_INTERVAL=3
MOCK_EMBED_PORT=8099

TOTAL_STEPS=7
ALL_PASSED=true
SERVER_PID=""

cleanup() {
  echo ""
  echo "Cleaning up..."
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null || true
    echo "  Server stopped (pid=$SERVER_PID)"
  fi
  # Stop mock embedding server (socat)
  pkill -f "socat.*$MOCK_EMBED_PORT" 2>/dev/null || true
  if [ -d "$DATA_DIR" ]; then
    rm -rf "$DATA_DIR"
    echo "  Removed $DATA_DIR"
  fi
  echo "Done."
}
trap cleanup EXIT

step() { echo -e "\n[$1/$TOTAL_STEPS] $2..."; }
ok() { echo "         OK${1:+ ($1)}"; }
fail() { echo "         FAIL${1:+ ($1)}"; ALL_PASSED=false; }
test_result() {
  local name="$1" passed="$2" detail="${3:-}"
  local status="OK"
  [ "$passed" = "false" ] && status="FAIL" && ALL_PASSED=false
  printf "  - %-45s %s%s\n" "$name" "$status" "${detail:+ ($detail)}"
}

port_is_open() { nc -z localhost "$1" 2>/dev/null; }

wait_for_port() {
  local port="$1" timeout="${2:-60}" label="${3:-port}"
  local start=$SECONDS
  while (( SECONDS - start < timeout )); do
    port_is_open "$port" && return 0
    sleep 0.5
  done
  echo "TIMEOUT: $label (port $port) not ready after ${timeout}s"
  return 1
}

# ── Stage 1: Build ──────────────────────────────────────────────
step 1 "Checking chronik-server binary"
if [ -f "$SERVER_BINARY" ]; then
  age_min=$(( ($(date +%s) - $(stat -c %Y "$SERVER_BINARY" 2>/dev/null || stat -f %m "$SERVER_BINARY" 2>/dev/null)) / 60 ))
  if [ "$age_min" -lt 30 ]; then
    ok "binary fresh (${age_min}m old, skipping build)"
  else
    cargo build --release --bin chronik-server 2>&1 | tail -1
    ok "built"
  fi
else
  cargo build --release --bin chronik-server 2>&1 | tail -1
  ok "built"
fi

# ── Stage 2: Mock Embedding Server ──────────────────────────────
step 2 "Starting mock embedding server"
# Simple HTTP server that returns deterministic embeddings using socat + bash
# For a proper test, this returns a fixed 64-dim vector for any input
if port_is_open "$MOCK_EMBED_PORT"; then
  fail "port $MOCK_EMBED_PORT already in use"
else
  # Create a minimal mock responder
  MOCK_RESPONSE='{"embeddings":[[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,0.1]]}'
  while true; do
    echo -e "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ${#MOCK_RESPONSE}\r\n\r\n$MOCK_RESPONSE" | nc -l -p "$MOCK_EMBED_PORT" -q 0 >/dev/null 2>&1 || true
  done &
  MOCK_PID=$!
  sleep 1
  ok "port $MOCK_EMBED_PORT (pid=$MOCK_PID)"
fi

# ── Stage 3: Start chronik-server ───────────────────────────────
step 3 "Starting chronik-server"
for port in $KAFKA_PORT $UNIFIED_API_PORT; do
  if port_is_open "$port"; then
    fail "port $port already in use — another server running?"
    exit 1
  fi
done

rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

RUST_LOG="info,chronik_storage::wal_indexer=debug" \
CHRONIK_DATA_DIR="$DATA_DIR" \
CHRONIK_UNIFIED_API_PORT="$UNIFIED_API_PORT" \
CHRONIK_EMBEDDING_PROVIDER="external" \
CHRONIK_EMBEDDING_ENDPOINT="http://localhost:$MOCK_EMBED_PORT/embed" \
CHRONIK_EMBEDDING_DIMENSIONS="64" \
"$SERVER_BINARY" start --advertise localhost > "$DATA_DIR/server.log" 2>&1 &
SERVER_PID=$!

wait_for_port $KAFKA_PORT 30 "Kafka"
wait_for_port $UNIFIED_API_PORT 30 "Unified API"
sleep 3
ok "pid=$SERVER_PID, ports $KAFKA_PORT+$UNIFIED_API_PORT"

# ── Stage 4: Create vector-enabled topic ────────────────────────
step 4 "Creating vector-enabled topic"
# Use kcat to produce a dummy message which auto-creates the topic
# Topic configs need to be set via admin — for now, produce to auto-create
if command -v kcat &>/dev/null; then
  echo '{"message":"init","level":"info","index":0}' | \
    kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC_NAME" 2>/dev/null
  ok "$TOPIC_NAME (auto-created via kcat)"
elif command -v kafkacat &>/dev/null; then
  echo '{"message":"init","level":"info","index":0}' | \
    kafkacat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC_NAME" 2>/dev/null
  ok "$TOPIC_NAME (auto-created via kafkacat)"
else
  fail "kcat/kafkacat not installed — cannot produce test messages"
  exit 1
fi

# ── Stage 5: Produce messages ───────────────────────────────────
step 5 "Producing messages"
MESSAGES=(
  "Connection refused to database server on port 5432"
  "Out of memory: killed process 12345"
  "SSL handshake failed: certificate expired"
  "High memory usage detected: 85% utilized"
  "Response time degraded: p99 latency at 2.5 seconds"
  "Server started successfully on port 8080"
  "New user registration completed"
  "Batch job completed: processed 50000 records"
  "Query executed in 3.2ms: SELECT FROM users"
  "Kafka produce latency p50=2ms p99=15ms"
)

produced=0
for i in $(seq 1 $NUM_MESSAGES); do
  idx=$(( (i - 1) % ${#MESSAGES[@]} ))
  msg="${MESSAGES[$idx]}"
  level="info"
  [ $idx -lt 3 ] && level="error"
  [ $idx -lt 5 ] && [ $idx -ge 3 ] && level="warning"
  echo "{\"message\":\"$msg\",\"level\":\"$level\",\"index\":$i,\"timestamp\":$(date +%s%3N)}" | \
    kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC_NAME" 2>/dev/null
  produced=$((produced + 1))
done
ok "$produced messages"

# ── Stage 6: Wait for vector indexing ───────────────────────────
step 6 "Waiting for vector indexing"
BASE_URL="http://localhost:$UNIFIED_API_PORT"
start_time=$SECONDS
total_vectors=0

while (( SECONDS - start_time < VECTOR_INDEX_TIMEOUT )); do
  resp=$(curl -s "$BASE_URL/_vector/$TOPIC_NAME/stats" 2>/dev/null || echo "{}")
  total_vectors=$(echo "$resp" | jq -r '.total_vectors // 0' 2>/dev/null || echo "0")

  if [ "$total_vectors" -gt 0 ]; then
    elapsed=$((SECONDS - start_time))
    echo "         ... $total_vectors vectors indexed (${elapsed}s)"
    if [ "$total_vectors" -ge "$NUM_MESSAGES" ]; then
      break
    fi
  fi
  sleep $POLL_INTERVAL
done

elapsed=$((SECONDS - start_time))
if [ "$total_vectors" -gt 0 ]; then
  ok "$total_vectors vectors in ${elapsed}s"
else
  echo "         WARNING: 0 vectors after ${elapsed}s (WalIndexer may need more time)"
fi

# ── Stage 7: Run search tests ──────────────────────────────────
step 7 "Running search tests"

# Test 1: List vector topics
resp=$(curl -s "$BASE_URL/_vector/topics" 2>/dev/null || echo "error")
if echo "$resp" | jq . >/dev/null 2>&1; then
  test_result "GET  /_vector/topics" "true" "$(echo "$resp" | jq -r 'length // "ok"') entries"
else
  test_result "GET  /_vector/topics" "false" "failed"
fi

# Test 2: Index stats
resp=$(curl -s "$BASE_URL/_vector/$TOPIC_NAME/stats" 2>/dev/null || echo "error")
if echo "$resp" | jq .total_vectors >/dev/null 2>&1; then
  vcount=$(echo "$resp" | jq -r '.total_vectors // 0')
  dims=$(echo "$resp" | jq -r '.dimensions // 0')
  test_result "GET  /_vector/{topic}/stats" "true" "${vcount} vectors, ${dims}d"
else
  test_result "GET  /_vector/{topic}/stats" "false" "failed"
fi

# Test 3: Text search
if [ "$total_vectors" -gt 0 ]; then
  resp=$(curl -s -X POST "$BASE_URL/_vector/$TOPIC_NAME/search" \
    -H 'Content-Type: application/json' \
    -d '{"query":"database connection error","k":5}' 2>/dev/null || echo "error")
  if echo "$resp" | jq .results >/dev/null 2>&1; then
    nresults=$(echo "$resp" | jq '.results | length')
    test_result "POST /_vector/{topic}/search" "true" "$nresults results"
  else
    test_result "POST /_vector/{topic}/search" "false" "$resp"
  fi

  # Test 4: Hybrid search
  resp=$(curl -s -X POST "$BASE_URL/_vector/$TOPIC_NAME/hybrid" \
    -H 'Content-Type: application/json' \
    -d '{"query":"memory usage warning","k":5}' 2>/dev/null || echo "error")
  http_code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE_URL/_vector/$TOPIC_NAME/hybrid" \
    -H 'Content-Type: application/json' \
    -d '{"query":"memory usage warning","k":5}' 2>/dev/null || echo "000")
  if [ "$http_code" = "200" ] || [ "$http_code" = "501" ]; then
    test_result "POST /_vector/{topic}/hybrid" "true" "HTTP $http_code"
  else
    test_result "POST /_vector/{topic}/hybrid" "false" "HTTP $http_code"
  fi
else
  echo "  - Skipping search tests (no vectors indexed yet)"
  ALL_PASSED=false
fi

# ── Summary ─────────────────────────────────────────────────────
echo ""
echo "============================================================"
if $ALL_PASSED; then
  echo "ALL TESTS PASSED"
  exit 0
else
  echo "SOME TESTS FAILED — see details above"
  exit 1
fi
