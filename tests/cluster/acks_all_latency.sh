#!/usr/bin/env bash
# #36: acks=all must not be hundreds of times slower than acks=1.
#
# The reported symptom was duplicate records — 300 produced coming back as 585.
# The cause is latency, not a double append: an `acks=all` produce could take
# seconds, clients time out at request.timeout.ms, retry, and a non-idempotent
# producer's retry appends the batch a second time. Fix the latency and the
# duplication goes with it. (Reproduced exactly: 300 records at acks=all with
# socket.timeout.ms=5000 landed 600 records on the broker, three times out of
# three. Same command after the fix: 300, every time.)
#
# Three separate faults each put a wait on the critical path of every acks=all
# write, and this measures the two shapes that expose them:
#
#   STEADY STATE — a follower could not SEE the records it was being asked to
#   acknowledge. The leader advertised its high watermark as its log end, and
#   under acks=all that watermark cannot advance until the followers acknowledge.
#   Separately, the fetch wait was taken per-partition in request order, so one
#   idle partition ahead of an active one in the same request delayed every
#   record on it by the full max_wait_ms (a flat 505ms per produce, measured).
#
#   FIRST WRITE TO A NEW TOPIC — the follower only re-read partition assignments
#   every 10s, so a partition created at t=0 was not replicated until t=10s, and
#   the producer waited on a follower that had not been told the partition
#   existed. Measured at ~7s; this is the one that actually times clients out.
#
# Budgets are generous on purpose: the point is to catch a return to "waiting on
# a background timer", which is one to two orders of magnitude out, not to
# police tens of milliseconds.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/chronik-server"
DIR="$ROOT/tests/cluster"
LOGS="$DIR/logs"
BOOT="localhost:9392,localhost:9393,localhost:9394"
# Sequential single-record produces: each one pays a full round trip, so the
# per-request cost is what is measured rather than batching throughput.
ROUNDS="${ACKS_ROUNDS:-10}"
# Steady-state per-request budget. Pre-fix this was a flat 505ms; a correct path
# is a few tens of milliseconds.
BUDGET_MS="${ACKS_BUDGET_MS:-250}"
# First write to a brand-new topic, which includes auto-creation, the assignment
# reaching the followers, and the first replication round trip. Pre-fix ~7000ms.
NEW_TOPIC_BUDGET_MS="${ACKS_NEW_TOPIC_BUDGET_MS:-2000}"
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

[ -x "$BIN" ] || { say "SKIP: build first (cargo build --release --bin chronik-server)"; exit 0; }
command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }

start_node() {
  mkdir -p "$LOGS" "$DIR/data/alt-node$1"
  CHRONIK_UNIFIED_API_PORT=$((6391 + $1)) RUST_LOG=info \
    "$BIN" start --config "$DIR/altport-node$1.toml" > "$LOGS/alt-node$1.log" 2>&1 &
  echo $! > "$LOGS/alt-node$1.pid"
}
stop_all() {
  for n in 1 2 3; do
    p=$(cat "$LOGS/alt-node$n.pid" 2>/dev/null)
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
    rm -f "$LOGS/alt-node$n.pid"
  done
}
trap stop_all EXIT

wait_for_port() {
  for _ in $(seq 1 60); do
    (exec 3<>/dev/tcp/127.0.0.1/$1) 2>/dev/null && { exec 3<&- ; return 0; }
    sleep 1
  done
  return 1
}

now_ms() { date +%s%3N; }

# One record, one request, one round trip.
produce_one() { # $1=topic $2=acks $3=payload
  printf '%s\n' "$3" | timeout 40 kcat -P -b "$BOOT" -t "$1" -p 0 \
    -X request.required.acks="$2" 2>/dev/null
}

# Median is the honest summary for the steady state: the first request to a new
# topic pays for creation and propagation, which the new-topic case measures on
# its own terms.
median_ms() { # $@ = samples
  printf '%s\n' "$@" | sort -n | awk '{v[NR]=$1} END {print (NR%2) ? v[(NR+1)/2] : int((v[NR/2]+v[NR/2+1])/2)}'
}

steady_state_ms() { # $1=acks value → echoes median ms
  local topic="ackslat-$1-$$" samples=()
  # Warm-up: create the topic and let the assignment settle, so the measured
  # requests are steady-state.
  produce_one "$topic" "$1" warmup
  sleep 6
  for _ in $(seq 1 "$ROUNDS"); do
    local t0 t1
    t0=$(now_ms)
    produce_one "$topic" "$1" "rec-$RANDOM"
    t1=$(now_ms)
    samples+=("$((t1 - t0))")
  done
  median_ms "${samples[@]}"
}

say "== acks=all latency on a 3-node cluster (RF=3, min_insync_replicas=2) =="
rm -rf "$DIR/data/alt-node"{1,2,3}
for n in 1 2 3; do start_node "$n"; done
for p in 9392 9393 9394; do
  wait_for_port "$p" || { fail "node on $p never came up"; exit 1; }
done
sleep 12
say "-- three nodes up"
say ""

# 1. First write to a topic that does not exist yet.
FRESH="acksnew-$$"
t0=$(now_ms)
produce_one "$FRESH" -1 "first-record"
t1=$(now_ms)
NEW=$((t1 - t0))

# 2. Steady state, both acks levels, same shape.
ONE=$(steady_state_ms 1)
ALL=$(steady_state_ms -1)

say "   acks=all  first write to a NEW topic   ${NEW}ms  (budget ${NEW_TOPIC_BUDGET_MS}ms)"
say "   acks=1    steady state, per request    ${ONE}ms"
say "   acks=all  steady state, per request    ${ALL}ms  (budget ${BUDGET_MS}ms)"
say ""

if [ "${NEW:-999999}" -gt "$NEW_TOPIC_BUDGET_MS" ]; then
  fail "first acks=all write to a new topic took ${NEW}ms — the followers have not been told the partition exists"
fi
if [ "${ALL:-999999}" -gt "$BUDGET_MS" ]; then
  fail "steady-state acks=all takes ${ALL}ms per request — a follower cannot see what it is asked to acknowledge"
fi

if [ "$FAIL" -eq 0 ]; then
  say "== PASS: acks=all completes on the replication round trip, not on a background timer =="
else
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
