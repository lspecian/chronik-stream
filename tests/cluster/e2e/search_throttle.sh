#!/bin/bash
# searchable.throttle.enabled: a burst faster than archiving slows down
# instead of leaving acknowledged records unsearchable.
#
# One node with a tiny hot index (soft cap 5,000, hard cap 20,000) and 8
# pipelining producers sending batches of up to 10,000, so a 300,000-record
# burst into one partition outruns archiving:
#   - throttle off (control): the hot index evicts records no archive holds
#     (chronik_hot_text_docs_total{op="evicted_unarchived"} > 0) — the test can
#     see the gap it is about;
#   - throttle on: none evicted unarchived, the coverage gauge stays 0 at every
#     check, produce waited (chronik_produce_search_throttle_ms_total > 0), and
#     every record is searchable at the end.
#
# Usage: search_throttle.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, jq.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/search-throttle-$LABEL; LOG=tests/cluster/logs/search-throttle-$LABEL
B=localhost:9092; N=300000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
tests/search_bench/gen.sh "$D/corpus" $N 5000 >/dev/null

start() {
  rm -rf "$D/data"
  CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true \
    CHRONIK_HOT_TEXT_MAX_DOCS=5000 CHRONIK_HOT_TEXT_HARD_MAX_DOCS=20000 \
    CHRONIK_SEARCH_COVERAGE_INTERVAL_SECS=2 \
    "$BIN" start --data-dir "$D/data" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL: server did not start"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }
metric() { curl -s -m 5 localhost:13092/metrics | awk -v m="$1" '$1 == m { print $2 }'; }
total() { curl -s -m 30 -X POST "localhost:6092/$1/_search" -H 'Content-Type: application/json' \
  -d '{"size":0,"track_total_hits":true}' | jq '.hits.total.value'; }
# Produce the corpus as fast as possible while sampling the coverage gauge;
# prints the worst value seen.
burst() { # topic
  ls "$D"/corpus/bulk-*.ndjson | xargs -P 8 -I{} sh -c "kcat -P -b $B -t $1 -p 0 -X linger.ms=5 < {} 2>/dev/null" &
  local producers=$! worst=0 g
  while kill -0 $producers 2>/dev/null; do
    g=$(metric chronik_search_unsearchable_offsets); [ "${g:-0}" -gt "$worst" ] && worst=$g
    sleep 1
  done
  wait $producers
  for _ in 1 2 3; do
    g=$(metric chronik_search_unsearchable_offsets); [ "${g:-0}" -gt "$worst" ] && worst=$g
    sleep 2
  done
  echo "$worst"
}

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh
trap stop EXIT

echo "throttle off (control):"
start
T=burst
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
burst $T >/dev/null
lost=$(metric 'chronik_hot_text_docs_total{op="evicted_unarchived"}')
if [ "${lost:-0}" -gt 0 ]; then echo "  ok   records evicted before archived: $lost"
else echo "  FAIL control: the burst did not outrun archiving (evicted_unarchived=${lost:-0}); nothing to test"; fail=1; fi
stop

echo "throttle on:"
start
T=throttled
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T \
  --add-config searchable.throttle.enabled=true >/dev/null
worst=$(burst $T)
expect "records evicted before archived" "$(metric 'chronik_hot_text_docs_total{op="evicted_unarchived"}')" "0"
expect "worst coverage gauge during the burst" "$worst" "0"
waited=$(metric chronik_produce_search_throttle_ms_total)
if [ "${waited:-0}" -gt 0 ]; then echo "  ok   produce waited: ${waited} ms"
else echo "  FAIL produce never waited (throttle_ms_total=${waited:-0})"; fail=1; fi
expect "batches written after the longest wait" "$(metric chronik_produce_search_throttle_timeouts_total)" "0"
expect "every record searchable" "$(total $T)" "$N"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
