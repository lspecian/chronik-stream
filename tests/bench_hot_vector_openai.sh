#!/usr/bin/env bash
#
# HP-3 Item 3: Real OpenAI freshness + cost probe.
#
# Requires OPENAI_API_KEY in .env (or environment). Hits the real OpenAI
# embedding API — consumes tokens. Defaults keep the run cheap: 10 probes
# of ~20 tokens each ≈ 200 tokens total ≈ $0.000004.
#
# Metrics scraped after the run:
#   chronik_hot_vector_visibility_lag_ms      — produce → visible freshness
#   chronik_hot_vector_batches_total          — successful/error embed calls
#   chronik_hot_vector_vectors_total{op=added}— total embeddings produced

set -euo pipefail

SERVER_BINARY="./target/release/chronik-server"
KAFKA_PORT=9092
UNIFIED_API_PORT=6092
METRICS_PORT=13092
TOPIC="${TOPIC:-hp3-openai-probe}"
DATA_DIR="${DATA_DIR:-tests/data/hp3-openai-probe}"
ITERATIONS="${ITERATIONS:-10}"
PROBE_TIMEOUT_S="${PROBE_TIMEOUT_S:-20}"
MODEL="${CHRONIK_EMBEDDING_MODEL:-text-embedding-3-small}"
DIMS="${CHRONIK_EMBEDDING_DIMENSIONS:-1536}"

[ -x "$SERVER_BINARY" ] || { echo "missing: $SERVER_BINARY"; exit 1; }

# Load OPENAI_API_KEY from .env if present
if [ -f .env ] && ! [ -n "${OPENAI_API_KEY:-}" ]; then
  # shellcheck disable=SC1091
  set -a; . ./.env; set +a
fi
[ -n "${OPENAI_API_KEY:-}" ] || { echo "OPENAI_API_KEY not set (env or .env)"; exit 1; }

SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    sleep 1
    kill -9 "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT

rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"

echo "=== HP-3 Item 3: real OpenAI freshness probe ==="
echo "  model            = $MODEL"
echo "  dimensions       = $DIMS"
echo "  iterations       = $ITERATIONS"
echo "  per-probe timeout= ${PROBE_TIMEOUT_S}s"
echo

RUST_LOG="warn,chronik_columnar::hot_vector_batcher=info" \
OPENAI_API_KEY="$OPENAI_API_KEY" \
CHRONIK_DEFAULT_SEARCHABLE=true \
CHRONIK_DEFAULT_VECTOR_ENABLED=true \
CHRONIK_EMBEDDING_PROVIDER=openai \
CHRONIK_EMBEDDING_MODEL="$MODEL" \
CHRONIK_EMBEDDING_DIMENSIONS="$DIMS" \
CHRONIK_HOT_VECTOR_ENABLED=true \
"$SERVER_BINARY" start \
  --data-dir "$DATA_DIR" \
  --advertise localhost \
  > "$DATA_DIR/server.log" 2>&1 &
SERVER_PID=$!

# Wait for ports
for p in "$KAFKA_PORT" "$UNIFIED_API_PORT"; do
  start=$SECONDS
  until nc -z localhost "$p" 2>/dev/null; do
    [ $((SECONDS - start)) -lt 60 ] || { tail -40 "$DATA_DIR/server.log"; exit 1; }
    sleep 0.2
  done
done
sleep 2
echo "server ready (pid=$SERVER_PID)"

# Seed topic
echo '{"marker":"warmup","text":"warmup"}' | kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC" 2>/dev/null
sleep 2

probe() {
  local marker="$1"
  local body='{"query":"'"$marker"'","k":3}'
  local t0_ns; t0_ns=$(date +%s%N)
  printf '{"marker":"%s","text":"%s unique sentinel for freshness"}\n' "$marker" "$marker" \
    | kcat -P -b "localhost:$KAFKA_PORT" -t "$TOPIC" 2>/dev/null
  local deadline=$((SECONDS + PROBE_TIMEOUT_S))
  while [ $SECONDS -lt $deadline ]; do
    local count
    count=$(curl -fs -X POST "http://localhost:$UNIFIED_API_PORT/_vector/$TOPIC/search" \
      -H 'content-type: application/json' -d "$body" 2>/dev/null \
      | jq -r '.count // 0' 2>/dev/null || echo 0)
    if [ "$count" != "0" ] && [ -n "$count" ]; then
      local t1_ns; t1_ns=$(date +%s%N)
      echo $(( (t1_ns - t0_ns) / 1000000 ))
      return 0
    fi
    sleep 0.1
  done
  return 1
}

samples=()
for i in $(seq 1 "$ITERATIONS"); do
  marker="probe-$(date +%s%N)-$i"
  if delta=$(probe "$marker"); then
    samples+=("$delta")
    printf "  [%02d/%02d] %-40s → %s ms\n" "$i" "$ITERATIONS" "$marker" "$delta"
  else
    printf "  [%02d/%02d] %-40s → TIMEOUT after %ds\n" "$i" "$ITERATIONS" "$marker" "$PROBE_TIMEOUT_S"
  fi
done

echo
if [ "${#samples[@]}" -gt 0 ]; then
  readarray -t sorted < <(printf '%s\n' "${samples[@]}" | sort -n)
  n=${#sorted[@]}
  idx_p50=$(( (n - 1) / 2 ))
  idx_p95=$(( (n * 95 / 100) > (n - 1) ? (n - 1) : (n * 95 / 100) ))
  idx_p99=$(( (n * 99 / 100) > (n - 1) ? (n - 1) : (n * 99 / 100) ))
  sum=0; for v in "${sorted[@]}"; do sum=$((sum + v)); done
  echo "--- end-to-end (produce → queryable) ---"
  printf "  n=%d  min=%s  p50=%s  p95=%s  p99=%s  max=%s  mean=%s (ms)\n" \
    "$n" "${sorted[0]}" "${sorted[$idx_p50]}" "${sorted[$idx_p95]}" "${sorted[$idx_p99]}" "${sorted[$((n-1))]}" "$((sum / n))"
fi

echo
echo "--- server-side metrics (chronik_hot_vector_*) ---"
curl -s "http://localhost:$METRICS_PORT/metrics" 2>/dev/null \
  | grep -E "^chronik_hot_vector_(visibility_lag_ms|batches_total|queue_total|vectors_total|cache_total)" \
  | sort

# Derive cost estimate (OpenAI text-embedding-3-small: $0.02 / 1M input tokens).
echo
echo "--- cost estimate (text-embedding-3-small, \$0.02 / 1M tokens) ---"
# chronik_embedding_tokens_used_total exists upstream; grep it.
tokens=$(curl -s "http://localhost:$METRICS_PORT/metrics" 2>/dev/null \
  | awk '/^chronik_embedding_tokens_total/ {print $2; exit}' || echo 0)
if [ -n "$tokens" ] && [ "$tokens" != "0" ]; then
  awk -v t="$tokens" 'BEGIN{printf "  tokens used  : %d\n  cost this run: $%.8f\n", t, t * 0.02 / 1000000}'
else
  echo "  (no token counter emitted yet — may not be exported)"
fi
