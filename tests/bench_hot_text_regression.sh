#!/usr/bin/env bash
#
# HP-1.7 — Produce-path regression + RAM probe for the hot text index.
#
# Runs chronik-bench in produce mode twice — once with the hot text index
# enabled, once disabled — and reports per-message latency percentiles and
# peak RSS for each. Designed to catch any throughput or tail-latency cost
# introduced by the fire-and-forget hot-path hook.
#
# Usage (default: both runs):
#   ./tests/bench_hot_text_regression.sh
#
# Tuning knobs (env):
#   DURATION=30s   CONCURRENCY=64   MSG_SIZE=256
#   PARTITIONS=1   MSG_COUNT=0  (unlimited within DURATION)
#
set -euo pipefail

SERVER_BINARY="${SERVER_BINARY:-./target/release/chronik-server}"
BENCH_BINARY="${BENCH_BINARY:-./target/release/chronik-bench}"
KAFKA_PORT=9092
DURATION="${DURATION:-30s}"
WARMUP="${WARMUP:-5s}"
CONCURRENCY="${CONCURRENCY:-64}"
MSG_SIZE="${MSG_SIZE:-256}"
PARTITIONS="${PARTITIONS:-1}"
MSG_COUNT="${MSG_COUNT:-0}"
TOPIC="hp17-regression"

[ -x "$SERVER_BINARY" ] || { echo "missing: $SERVER_BINARY"; exit 1; }
[ -x "$BENCH_BINARY" ]  || { echo "missing: $BENCH_BINARY (run: cargo build --release --bin chronik-bench)"; exit 1; }

SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    sleep 1
    kill -9 "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

rss_kb() {
  local pid="$1"
  if [ -r "/proc/$pid/status" ]; then
    awk '/^VmRSS:/ {print $2}' "/proc/$pid/status"
  else
    ps -o rss= -p "$pid" 2>/dev/null | awk '{print $1}'
  fi
}

run_one() {
  local mode="$1"   # "hot" or "cold"
  local enabled="true"; [ "$mode" = "cold" ] && enabled="false"
  local data_dir="tests/data/hp17-reg-$mode"
  rm -rf "$data_dir"; mkdir -p "$data_dir"

  echo
  echo "=== $mode (CHRONIK_HOT_TEXT_ENABLED=$enabled) ==="

  RUST_LOG="warn" \
  CHRONIK_DEFAULT_SEARCHABLE=true \
  CHRONIK_HOT_TEXT_ENABLED="$enabled" \
  "$SERVER_BINARY" start \
    --data-dir "$data_dir" \
    --advertise localhost \
    > "$data_dir/server.log" 2>&1 &
  SERVER_PID=$!

  # Wait for Kafka port
  local start=$SECONDS
  until nc -z localhost "$KAFKA_PORT" 2>/dev/null; do
    [ $((SECONDS - start)) -lt 60 ] || { echo "timed out waiting for Kafka"; tail -40 "$data_dir/server.log"; exit 1; }
    sleep 0.2
  done
  sleep 2

  # Baseline RSS pre-bench
  local rss_before
  rss_before=$(rss_kb "$SERVER_PID")
  echo "baseline RSS: ${rss_before} KB"

  # Run bench. --mode produce, --create-topic to ensure topic exists with desired partition count.
  "$BENCH_BINARY" \
    --bootstrap-servers "localhost:$KAFKA_PORT" \
    --topic "$TOPIC" \
    --mode produce \
    --duration "$DURATION" \
    --warmup-duration "$WARMUP" \
    --concurrency "$CONCURRENCY" \
    --message-size "$MSG_SIZE" \
    --message-count "$MSG_COUNT" \
    --partitions "$PARTITIONS" \
    --create-topic 2>&1 | tee "$data_dir/bench.log"

  # Peak RSS after bench (approximate — take current reading)
  local rss_after
  rss_after=$(rss_kb "$SERVER_PID")
  local rss_delta=$((rss_after - rss_before))
  echo
  echo "RSS before : ${rss_before} KB"
  echo "RSS after  : ${rss_after} KB"
  echo "RSS delta  : ${rss_delta} KB"

  # Kill server
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=""
  sleep 2
}

run_one hot
run_one cold

echo
echo "=== done. Compare the two per-message latency summaries above."
