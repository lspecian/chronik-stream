#!/bin/bash
# The coverage check reports acknowledged records that no search can find
# (chronik_search_unsearchable_offsets), and only those.
#
# One node, check every 3 s:
#   - hot index on: records produced in bursts are searchable at once; the
#     gauge stays 0 across several checks, and once archived.
#   - hot index off: records wait for their archive (sealed after the WAL is
#     idle); meanwhile the gauge reports exactly them (N, one partition), and
#     drops to 0 once the archive covers them; the total search agrees.
#
# Usage: search_coverage_metric.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, jq.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/coverage-metric-$LABEL; LOG=tests/cluster/logs/coverage-metric-$LABEL
B=localhost:9092; N=5000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
tests/search_bench/gen.sh "$D/corpus" $N $N >/dev/null
CORPUS="$D/corpus/bulk-00000.ndjson"

start() { # hot enabled?
  rm -rf "$D/data"
  CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_HOT_TEXT_ENABLED=$1 \
    CHRONIK_SEARCH_COVERAGE_INTERVAL_SECS=3 \
    "$BIN" start --data-dir "$D/data" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL: server did not start"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }
gauge() { curl -s -m 5 localhost:13092/metrics | awk -v m="$1" '$1 == m { print $2 }'; }
total() { curl -s -m 30 -X POST "localhost:6092/$1/_search" -H 'Content-Type: application/json' -d '{"size":0}' | jq '.hits.total.value'; }
archived() { find "$D/data" -path "*/tantivy_indexes/$1/*" -name '*.tar' | wc -l; }

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh
trap stop EXIT

echo "hot index on:"
start true
T=covhot
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
worst=0
for burst in 1 2 3 4 5; do
  sed -n "$(( (burst - 1) * N / 5 + 1 )),$(( burst * N / 5 ))p" "$CORPUS" | kcat -P -b $B -t $T -p 0 2>/dev/null
  sleep 3.5
  g=$(gauge chronik_search_unsearchable_offsets); [ "${g:-0}" -gt "$worst" ] && worst=$g
done
expect "gauge while producing (worst of 5 checks)" "$worst" "0"
expect "every record searchable" "$(total $T)" "$N"
for _ in $(seq 30); do [ "$(archived $T)" -ge 1 ] && break; sleep 2; done
sleep 7
expect "gauge once archived" "$(gauge chronik_search_unsearchable_offsets)" "0"
stop

echo "hot index off:"
start false
T=covcold
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
kcat -P -b $B -t $T -p 0 < "$CORPUS" 2>/dev/null
sleep 8 # two checks: the records are older than one interval
if [ "$(archived $T)" = 0 ]; then
  expect "gauge before the archive" "$(gauge chronik_search_unsearchable_offsets) $(gauge chronik_search_unsearchable_partitions)" "$N 1"
  expect "warned" "$(grep -c 'search coverage: acknowledged records' "$LOG-server.log" | awk '{ print ($1 > 0) }')" "1"
else
  echo "  FAIL: archived before the check could see the gap (test timing)"; fail=1
fi
for _ in $(seq 40); do [ "$(archived $T)" -ge 1 ] && break; sleep 2; done
sleep 7
expect "gauge once archived" "$(gauge chronik_search_unsearchable_offsets) $(gauge chronik_search_unsearchable_partitions)" "0 0"
expect "every record searchable" "$(total $T)" "$N"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
