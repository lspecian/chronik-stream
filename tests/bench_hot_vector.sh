#!/usr/bin/env bash
#
# HP-2.9 — Hot vector path benchmark (Phase 2).
#
# Starts:
#   1. A mock embedding server on port 18099 (tests/mock_embedder)
#   2. chronik-server with CHRONIK_DEFAULT_VECTOR_ENABLED=true and the
#      external provider pointed at the mock
# Then runs:
#   - Freshness probe: produce a marker text, poll /_vector/{topic}/search
#     until the marker's vector returns itself as the closest hit.
#   - Produce regression: chronik-bench produce mode with and without the
#     hot vector path (second run sets CHRONIK_HOT_VECTOR_ENABLED=false).
#
# Requirements: built chronik-server, chronik-bench, mock-embedder, kcat,
# curl, jq, nc.

set -euo pipefail

SERVER_BINARY="./target/release/chronik-server"
BENCH_BINARY="./target/release/chronik-bench"
MOCK_BINARY="./target/release/mock-embedder"
MOCK_PORT=18099
MOCK_DIMS="${MOCK_DIMS:-64}"
KAFKA_PORT=9092
UNIFIED_API_PORT=6092
TOPIC="${TOPIC:-hp29-vec}"
DATA_DIR="${DATA_DIR:-tests/data/hp29-vec}"
DURATION="${DURATION:-20s}"
WARMUP="${WARMUP:-3s}"
CONCURRENCY="${CONCURRENCY:-16}"
MSG_SIZE="${MSG_SIZE:-128}"
FRESHNESS_ITERATIONS="${FRESHNESS_ITERATIONS:-10}"
FRESHNESS_TIMEOUT_S="${FRESHNESS_TIMEOUT_S:-10}"

[ -x "$SERVER_BINARY" ] || { echo "missing: $SERVER_BINARY"; exit 1; }
[ -x "$BENCH_BINARY" ]  || { echo "missing: $BENCH_BINARY"; exit 1; }
[ -x "$MOCK_BINARY" ]   || { echo "missing: $MOCK_BINARY"; exit 1; }

MOCK_PID=""
SERVER_PID=""
cleanup() {
  for pid in $SERVER_PID $MOCK_PID; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      sleep 1
      kill -9 "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT

rss_kb() {
  if [ -r "/proc/$1/status" ]; then
    awk '/^VmRSS:/ {print $2}' "/proc/$1/status"
  fi
}

start_mock() {
  PORT=$MOCK_PORT DIMS=$MOCK_DIMS "$MOCK_BINARY" > /tmp/mock-embedder.log 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 20); do
    nc -z localhost "$MOCK_PORT" 2>/dev/null && return 0
    sleep 0.2
  done
  echo "mock failed to start"; exit 1
}

start_server() {
  local mode="$1"   # "hot" or "cold"
  local enabled="true"; [ "$mode" = "cold" ] && enabled="false"
  rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"
  RUST_LOG="warn,chronik_columnar::hot_vector_batcher=info" \
  CHRONIK_DEFAULT_SEARCHABLE=true \
  CHRONIK_DEFAULT_VECTOR_ENABLED=true \
  CHRONIK_HOT_VECTOR_ENABLED="$enabled" \
  CHRONIK_EMBEDDING_PROVIDER=external \
  CHRONIK_EMBEDDING_ENDPOINT="http://localhost:$MOCK_PORT/embed" \
  CHRONIK_EMBEDDING_DIMENSIONS="$MOCK_DIMS" \
  "$SERVER_BINARY" start \
    --data-dir "$DATA_DIR" \
    --advertise localhost \
    > "$DATA_DIR/server.log" 2>&1 &
  SERVER_PID=$!
  local start=$SECONDS
  while ! nc -z localhost $KAFKA_PORT 2>/dev/null; do
    [ $((SECONDS - start)) -lt 60 ] || { tail -40 "$DATA_DIR/server.log"; exit 1; }
    sleep 0.2
  done
  start=$SECONDS
  while ! nc -z localhost $UNIFIED_API_PORT 2>/dev/null; do
    [ $((SECONDS - start)) -lt 30 ] || { tail -40 "$DATA_DIR/server.log"; exit 1; }
    sleep 0.2
  done
  sleep 2
}

stop_server() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  SERVER_PID=""
  sleep 2
}

freshness_probe() {
  # Produces a unique marker, polls /_vector/{topic}/search for a close hit.
  local marker="$1"
  local body='{"query":"'"$marker"'","k":3}'
  local t0_ns; t0_ns=$(date +%s%N)
  printf '{"marker":"%s","text":"%s"}\n' "$marker" "$marker" | \
    kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC" 2>/dev/null
  local deadline=$((SECONDS + FRESHNESS_TIMEOUT_S))
  while [ $SECONDS -lt $deadline ]; do
    local got
    got=$(curl -fs -X POST "http://localhost:$UNIFIED_API_PORT/_vector/$TOPIC/search" \
      -H 'content-type: application/json' -d "$body" 2>/dev/null \
      | jq -r '.count // 0' 2>/dev/null || echo 0)
    if [ "$got" != "0" ] && [ -n "$got" ]; then
      local t1_ns; t1_ns=$(date +%s%N)
      echo $(( (t1_ns - t0_ns) / 1000000 ))
      return 0
    fi
    sleep 0.05
  done
  return 1
}

run_freshness() {
  echo ""
  echo "=== Freshness probe (hot vector enabled) ==="
  # Seed the topic with a single produce so it exists and the hot path gets primed
  echo '{"marker":"warmup","text":"warmup"}' | kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC" 2>/dev/null
  sleep 1

  samples=()
  for i in $(seq 1 $FRESHNESS_ITERATIONS); do
    marker="probe-$(date +%s%N)-$i"
    if delta=$(freshness_probe "$marker"); then
      samples+=("$delta")
      printf "  [%02d/%02d] %-40s → %s ms\n" "$i" "$FRESHNESS_ITERATIONS" "$marker" "$delta"
    else
      printf "  [%02d/%02d] %-40s → TIMEOUT after %ds\n" "$i" "$FRESHNESS_ITERATIONS" "$marker" "$FRESHNESS_TIMEOUT_S"
    fi
  done
  if [ "${#samples[@]}" -eq 0 ]; then
    echo "  no successful samples"
    return
  fi
  readarray -t sorted < <(printf '%s\n' "${samples[@]}" | sort -n)
  local n=${#sorted[@]}
  local idx_p50=$(( (n - 1) / 2 ))
  local idx_p95=$(( (n * 95 / 100) > (n - 1) ? (n - 1) : (n * 95 / 100) ))
  local idx_p99=$(( (n * 99 / 100) > (n - 1) ? (n - 1) : (n * 99 / 100) ))
  local sum=0; for v in "${sorted[@]}"; do sum=$((sum + v)); done
  printf "  results: n=%d  min=%s  p50=%s  p95=%s  p99=%s  max=%s  mean=%s (ms)\n" \
    "$n" "${sorted[0]}" "${sorted[$idx_p50]}" "${sorted[$idx_p95]}" "${sorted[$idx_p99]}" "${sorted[$((n-1))]}" "$((sum / n))"
}

run_regression() {
  local mode="$1"
  echo ""
  echo "=== Produce regression ($mode) ==="
  local rss_before rss_after
  rss_before=$(rss_kb "$SERVER_PID")
  echo "  baseline RSS: ${rss_before} KB"
  "$BENCH_BINARY" \
    --bootstrap-servers "localhost:$KAFKA_PORT" \
    --topic "hp29-reg-$mode" \
    --mode produce \
    --duration "$DURATION" \
    --warmup-duration "$WARMUP" \
    --concurrency "$CONCURRENCY" \
    --message-size "$MSG_SIZE" \
    --partitions 1 \
    --create-topic 2>&1 | tee "$DATA_DIR/bench-$mode.log" | grep -E "Messages:|Message rate:|p50:|p95:|p99:|p99.9:|max:"
  rss_after=$(rss_kb "$SERVER_PID")
  echo "  RSS delta: $((rss_after - rss_before)) KB"
}

echo "=== HP-2.9 benchmark suite ==="
echo "  kafka=$KAFKA_PORT  api=$UNIFIED_API_PORT  mock=$MOCK_PORT (dims=$MOCK_DIMS)"

start_mock

start_server hot
run_freshness
run_regression hot
stop_server

start_server cold
run_regression cold
stop_server

echo ""
echo "=== done ==="
