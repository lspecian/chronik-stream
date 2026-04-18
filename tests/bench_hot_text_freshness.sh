#!/usr/bin/env bash
#
# HP-1.7 — Hot text index freshness probe.
#
# Measures T0 (produce) → T1 (searchable via /_search) latency, aka produce-
# to-queryable lag for full-text search. Run once with hot path enabled
# (default) and once with CHRONIK_HOT_TEXT_ENABLED=false for comparison.
#
# Usage:
#   ./tests/bench_hot_text_freshness.sh              # hot enabled (default)
#   CHRONIK_HOT_TEXT_ENABLED=false ITERATIONS=5 \
#     PROBE_TIMEOUT_S=120 ./tests/bench_hot_text_freshness.sh
#
# Requirements: built chronik-server, kcat, curl, jq, nc.
#
set -euo pipefail

KAFKA_PORT="${KAFKA_PORT:-9092}"
UNIFIED_API_PORT="${UNIFIED_API_PORT:-6092}"
TOPIC="${TOPIC:-hp17-probe}"
DATA_DIR="${DATA_DIR:-tests/data/hp17-freshness}"
SERVER_BINARY="${SERVER_BINARY:-./target/release/chronik-server}"
ITERATIONS="${ITERATIONS:-20}"
POLL_INTERVAL_MS="${POLL_INTERVAL_MS:-50}"
PROBE_TIMEOUT_S="${PROBE_TIMEOUT_S:-10}"
HOT_ENABLED="${CHRONIK_HOT_TEXT_ENABLED:-true}"

SERVER_PID=""

cleanup() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT

require() { command -v "$1" >/dev/null || { echo "missing: $1"; exit 1; }; }
require kcat; require curl; require jq; require nc

[ -x "$SERVER_BINARY" ] || { echo "missing binary: $SERVER_BINARY (run: cargo build --release --bin chronik-server)"; exit 1; }

rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"
echo "=== HP-1.7 freshness probe ==="
echo "  hot_enabled       = $HOT_ENABLED"
echo "  iterations        = $ITERATIONS"
echo "  poll interval     = ${POLL_INTERVAL_MS}ms"
echo "  per-probe timeout = ${PROBE_TIMEOUT_S}s"
echo "  kafka port        = $KAFKA_PORT"
echo "  api port          = $UNIFIED_API_PORT"
echo

RUST_LOG="warn,chronik_server=info" \
CHRONIK_DEFAULT_SEARCHABLE=true \
CHRONIK_HOT_TEXT_ENABLED="$HOT_ENABLED" \
"$SERVER_BINARY" start \
  --data-dir "$DATA_DIR" \
  --advertise localhost \
  > "$DATA_DIR/server.log" 2>&1 &
SERVER_PID=$!

# Wait for both ports
for p in "$KAFKA_PORT" "$UNIFIED_API_PORT"; do
  start=$SECONDS
  until nc -z localhost "$p" 2>/dev/null; do
    [ $((SECONDS - start)) -lt 60 ] || { echo "timed out waiting for port $p"; cat "$DATA_DIR/server.log" | tail -40; exit 1; }
    sleep 0.2
  done
done
# Give the server a moment to finish init (metadata store, admin token, etc.)
sleep 2
echo "server ready (pid=$SERVER_PID)"

# Auto-create topic via a throwaway produce
echo '{"marker":"warmup"}' | kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC" 2>/dev/null
sleep 0.5

probe_once() {
  local marker="$1"
  local body='{"query":{"match":{"value":"'"$marker"'"}}}'
  # T0 captured just before produce
  local t0_ns
  t0_ns=$(date +%s%N)
  printf '{"marker":"%s"}\n' "$marker" | kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC" 2>/dev/null
  local deadline=$((SECONDS + PROBE_TIMEOUT_S))
  while [ $SECONDS -lt $deadline ]; do
    local hits
    hits=$(curl -fs -X POST "http://localhost:$UNIFIED_API_PORT/_search" \
      -H 'content-type: application/json' \
      -d "$body" 2>/dev/null | jq -r '.hits.total.value // 0' 2>/dev/null || echo 0)
    if [ "$hits" != "0" ] && [ -n "$hits" ]; then
      local t1_ns=$(date +%s%N)
      # milliseconds delta
      echo $(( (t1_ns - t0_ns) / 1000000 ))
      return 0
    fi
    sleep "$(awk "BEGIN{printf \"%.3f\", $POLL_INTERVAL_MS/1000}")"
  done
  return 1
}

samples=()
for i in $(seq 1 "$ITERATIONS"); do
  marker="probe-$(date +%s%N)-$i"
  if delta_ms=$(probe_once "$marker"); then
    samples+=("$delta_ms")
    printf "  [%02d/%02d] %s → %d ms\n" "$i" "$ITERATIONS" "$marker" "$delta_ms"
  else
    printf "  [%02d/%02d] %s → TIMEOUT after %ds\n" "$i" "$ITERATIONS" "$marker" "$PROBE_TIMEOUT_S"
  fi
done

echo
if [ "${#samples[@]}" -eq 0 ]; then
  echo "no successful samples"
  exit 1
fi

# Percentiles via sort + index
readarray -t sorted < <(printf '%s\n' "${samples[@]}" | sort -n)
n=${#sorted[@]}
idx_p50=$(( (n - 1) / 2 ))
idx_p95=$(( (n * 95 / 100) > (n - 1) ? (n - 1) : (n * 95 / 100) ))
idx_p99=$(( (n * 99 / 100) > (n - 1) ? (n - 1) : (n * 99 / 100) ))
sum=0; for v in "${sorted[@]}"; do sum=$((sum + v)); done
avg=$((sum / n))

echo "--- results (hot_enabled=$HOT_ENABLED, n=$n) ---"
printf "  min  : %s ms\n" "${sorted[0]}"
printf "  p50  : %s ms\n" "${sorted[$idx_p50]}"
printf "  p95  : %s ms\n" "${sorted[$idx_p95]}"
printf "  p99  : %s ms\n" "${sorted[$idx_p99]}"
printf "  max  : %s ms\n" "${sorted[$((n - 1))]}"
printf "  mean : %s ms\n" "$avg"
