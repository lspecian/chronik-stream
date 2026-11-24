#!/bin/bash
# Exact totals, range queries and aggregations over a topic's records, each
# record counted once: archived and still-hot records together, and on a
# cluster across replicas.
#
# Up to v2.19.2: a `range` on a JSON field answered nothing (the JSON field had
# no columns); aggregations over topic data answered nothing; `size: 0` always
# reported a total of 0; totals were only what the page collection saw.
#
# Here: N JSON log records (tests/search_bench/gen.sh), archived, then M more
# that stay hot (no archive yet). Expected values are computed from the corpus
# with awk, independently of Chronik. Checks, each over all N + M records:
#   total (size 0), a range on a JSON number, a range + term filter, terms per
#   level (all buckets), the sum of a date_histogram (1m), an avg.
# Single node, then the 3-node cluster (3 partitions, RF3).
#
# Usage: search_aggregations.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, jq.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
C=tests/cluster; D=$C/data/aggs-$LABEL; LOG=$C/logs/aggs-$LABEL
N=20000; M=3000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
tests/search_bench/gen.sh "$D/corpus" $((N + M)) $((N + M)) >/dev/null
CORPUS="$D/corpus/bulk-00000.ndjson"
head -n $N "$CORPUS" > "$D/first.ndjson"; tail -n $M "$CORPUS" > "$D/rest.ndjson"

# Expected answers, from the corpus.
T0=1767225600000; LO=$((T0 + 100000)); HI=$((T0 + 400000))
num() { grep -o "\"$1\":[0-9]*" "$CORPUS" | cut -d: -f2; }
str() { grep -o "\"$1\":\"[^\"]*\"" "$CORPUS" | cut -d'"' -f4; }
want_total=$((N + M))
want_range=$(num ts | awk -v lo=$LO -v hi=$HI '$1 >= lo && $1 <= hi' | wc -l)
want_range_error=$(paste -d' ' <(num ts) <(str level) | awk -v lo=$LO -v hi=$HI '$1 >= lo && $1 <= hi && $2 == "ERROR"' | wc -l)
want_levels=$(str level | sort | uniq -c | awk '{ printf "%s=%s ", $2, $1 }' | sed 's/ $//')
want_avg=$(num latency_ms | awk '{ s += $1 } END { printf "%.3f", s / NR }')

search() { curl -s -m 60 -X POST "localhost:6092/$1/_search" -H 'Content-Type: application/json' -d "$2"; }
check() { # topic where
  local t=$1
  echo "$2:"
  expect "total (size 0)" "$(search $t '{"size":0}' | jq -c '[.hits.total.value, .hits.total.relation]')" "[$want_total,\"eq\"]"
  expect "range on a JSON number" \
    "$(search $t "{\"size\":0,\"query\":{\"range\":{\"ts\":{\"gte\":$LO,\"lte\":$HI}}}}" | jq '.hits.total.value')" "$want_range"
  expect "range + term filter" \
    "$(search $t "{\"size\":0,\"query\":{\"bool\":{\"filter\":[{\"range\":{\"ts\":{\"gte\":$LO,\"lte\":$HI}}},{\"term\":{\"level\":\"ERROR\"}}]}}}" | jq '.hits.total.value')" "$want_range_error"
  expect "terms per level" \
    "$(search $t '{"size":0,"aggs":{"l":{"terms":{"field":"level","size":10}}}}' | jq -r '[.aggregations.l.buckets[] | "\(.key)=\(.doc_count)"] | sort | join(" ")')" "$want_levels"
  local hist; hist=$(search $t '{"size":0,"aggs":{"m":{"date_histogram":{"field":"ts","fixed_interval":"1m"}}}}')
  expect "date_histogram sum" "$(jq '[.aggregations.m.buckets[].doc_count] | add' <<<"$hist")" "$want_total"
  expect "date_histogram first key" "$(jq -c '.aggregations.m.buckets[0] | [.key, .key_as_string]' <<<"$hist")" "[$T0,\"2026-01-01T00:00:00.000Z\"]"
  expect "avg" "$(search $t '{"size":0,"aggs":{"a":{"avg":{"field":"latency_ms"}}}}' | jq -r '.aggregations.a.value * 1000 | round / 1000 | tostring' | awk '{ printf "%.3f", $1 }')" "$want_avg"
  expect "track_total_hits" "$(search $t '{"size":5,"track_total_hits":true,"query":{"term":{"level":"ERROR"}}}' | jq -c '[.hits.total.value, (.hits.hits | length)]')" \
    "[$(str level | grep -c '^ERROR$'),5]"
}
segs() { find "$1" -path "*/$2/*" -name '*.tar' 2>/dev/null | wc -l; }

echo "=== $LABEL ($BIN): single node"
$C/stop.sh >/dev/null 2>&1
. $C/ports_free.sh
CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true "$BIN" start --data-dir "$D/single" > "$LOG-single.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
$K/kafka-topics.sh --bootstrap-server localhost:9092 --create --topic agg --partitions 1 --replication-factor 1 >/dev/null 2>&1
kcat -P -b localhost:9092 -t agg -p 0 < "$D/first.ndjson" 2>/dev/null
for _ in $(seq 40); do [ "$(segs "$D/single" agg)" -ge 1 ] && break; sleep 3; done
sleep 3
kcat -P -b localhost:9092 -t agg -p 0 < "$D/rest.ndjson" 2>/dev/null
sleep 3
echo "  archives: $(segs "$D/single" agg) (the last $M records are not archived yet)"
check agg "single node, archived + hot"
kill $pid; wait $pid 2>/dev/null; trap - EXIT

echo "=== $LABEL: 3-node cluster, 3 partitions, RF3"
CHRONIK_BIN=$BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
B=localhost:9092,localhost:9093,localhost:9094
$K/kafka-topics.sh --bootstrap-server $B --create --topic aggc --partitions 3 --replication-factor 3 >/dev/null 2>&1
sleep 5
kcat -P -b $B -t aggc -X acks=all < "$D/first.ndjson" 2>/dev/null
for _ in $(seq 60); do [ "$(segs $C/data aggc)" -ge 9 ] && break; sleep 3; done
sleep 3
kcat -P -b $B -t aggc -X acks=all < "$D/rest.ndjson" 2>/dev/null
sleep 5
echo "  archives on all nodes: $(segs $C/data aggc)"
check aggc "cluster, archived + hot, each record once across replicas"
$C/stop.sh >/dev/null 2>&1

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
