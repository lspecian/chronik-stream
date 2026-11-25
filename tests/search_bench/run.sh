#!/bin/bash
# Search benchmark: one Chronik node, the gen.sh corpus produced over Kafka to a
# searchable topic, queried through /{topic}/_search. Comparable with the
# OpenSearch comparison (adaptor branch) and plain_tantivy_bench.
#
# Usage: run.sh <chronik-binary> <corpus-dir> <work-dir> [label]
# Env: PARTITIONS (1), CONC (parallel producers, 8), VUS (8), DURATION (60s),
#      AGGS=1 (also time the aggregation queries), SETTLE_SECS (60: wait before
#      the query load, for archiving and merges), TOPIC_CONFIG (topic configs
#      to set, kafka-configs syntax), K6_IMAGE, extra CHRONIK_* vars
#      pass through to the server.
# Needs: KAFKA_HOME, kcat, jq, docker (for k6).
#
# Prints: produce time, time until a marker sent after the corpus is found,
# selective totals (exactness), range/aggregation answers, archives, disk,
# RSS, per-query latency under load; writes <work-dir>/<label>.json.
set -u
BIN=${1:?chronik binary}; CORPUS=${2:?corpus dir}; WORK=${3:?work dir}; LABEL=${4:-chronik}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
PARTITIONS=${PARTITIONS:-1}; CONC=${CONC:-8}; VUS=${VUS:-8}; DURATION=${DURATION:-60s}
K6_IMAGE=${K6_IMAGE:-grafana/k6:0.54.0}
HERE="$(cd "$(dirname "$0")" && pwd)"
B=localhost:9092; API=http://localhost:6092; T=logs
N=$(cat "$CORPUS"/bulk-*.ndjson | wc -l)
DATA="$WORK/$LABEL-data"; RES="$WORK/$LABEL.json"
mkdir -p "$WORK"; rm -rf "$DATA"; mkdir -p "$DATA"
now() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.1f", b - a }'; }

CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true RUST_LOG=${RUST_LOG:-warn} \
  "$BIN" start --data-dir "$DATA" > "$WORK/$LABEL.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; wait $PID 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fs "$API/health" >/dev/null 2>&1 && break; sleep 1; done
cpu() { awk '{ printf "%.1f", ($14 + $15) / 100 }' /proc/$PID/stat; }
rss_mb() { awk '/VmRSS/ { printf "%d", $2 / 1024 }' /proc/$PID/status; }
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions "$PARTITIONS" --replication-factor 1 >/dev/null 2>&1
[ -n "${TOPIC_CONFIG:-}" ] && $K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T --add-config "$TOPIC_CONFIG" >/dev/null
sleep 2
search() { curl -s -m 60 -X POST "$API/$T/_search" -H 'Content-Type: application/json' -d "$1"; }

echo "=== $LABEL: produce $N documents ($PARTITIONS partition(s), $CONC producers)"
cpu0=$(cpu); t0=$(now)
ls "$CORPUS"/bulk-*.ndjson | xargs -P "$CONC" -I{} sh -c "kcat -P -b $B -t $T -X acks=1 -X linger.ms=5 -X batch.num.messages=5000 < {} 2>/dev/null"
produce_s=$(since "$t0"); cpu1=$(cpu)
echo "  produce: ${produce_s}s"

echo '{"ts":1767225600000,"level":"INFO","service":"svc-0","host":"host-00","status":200,"latency_ms":1,"message":"zzfinalmarkerzz"}' \
  | kcat -P -b $B -t $T -p 0 2>/dev/null
fresh_s=-1; t1=$(now)
for _ in $(seq 1200); do
  [ "$(search '{"query":{"match":{"message":"zzfinalmarkerzz"}},"size":1}' | jq -r '.hits.hits | length' 2>/dev/null)" = 1 ] \
    && { fresh_s=$(since "$t1"); break; }
  sleep 0.25
done
echo "  marker searchable ${fresh_s}s after it was sent"

answers=$(for q in \
  '{"query":{"bool":{"must":[{"match":{"message":"refused"}}],"filter":[{"term":{"service":"svc-3"}}]}},"size":10000,"track_total_hits":true}' \
  '{"query":{"bool":{"filter":[{"term":{"host":"host-07"}},{"term":{"level":"ERROR"}}]}},"size":10000,"track_total_hits":true}' \
  '{"query":{"bool":{"must":[{"match":{"message":"timeout"}}],"filter":[{"term":{"service":"svc-11"}},{"term":{"level":"WARN"}}]}},"size":10000,"track_total_hits":true}'; do
  search "$q" | jq -c '{total: .hits.total, returned: (.hits.hits | length)}'
done)
echo "$answers" | sed 's/^/  answer: /'
range=$(search '{"query":{"range":{"ts":{"gte":1767226200000,"lte":1767226800000}}},"size":0,"track_total_hits":true,"aggs":{"s":{"terms":{"field":"service","size":3}}}}' \
  | jq -c '{total: .hits.total.value, top3: [.aggregations.s.buckets[]? | [.key, .doc_count]]}' 2>/dev/null)
hist=$(search '{"size":0,"query":{"match_all":{}},"aggs":{"m":{"date_histogram":{"field":"ts","fixed_interval":"1m"}}}}' \
  | jq -c '{buckets: ([.aggregations.m.buckets[]?] | length), sum: ([.aggregations.m.buckets[]?.doc_count] | add // 0)}' 2>/dev/null)
echo "  range 10-20 min + terms(service): $range"
echo "  date_histogram 1m over all: $hist"

# Every record searchable, not just the newest: a burst can outrun archiving,
# and the answers above are then short. Counted from when the marker was sent.
all_s=-1
for _ in $(seq 1800); do
  [ "$(search '{"size":0,"track_total_hits":true,"query":{"match_all":{}}}' | jq -r '.hits.total.value' 2>/dev/null)" = $((N + 1)) ] \
    && { all_s=$(since "$t1"); break; }
  sleep 0.5
done
echo "  every record searchable ${all_s}s after the marker was sent"

sleep "${SETTLE_SECS:-60}" # background archiving / merges settle
archives=$(find "$DATA" \( -name '*.tar' -o -name '*.tar.gz' \) -path "*/$T/*" | wc -l)
disk=$(du -sm "$DATA" | cut -f1); rss=$(rss_mb)
echo "  archives: $archives; disk: ${disk} MB; RSS: ${rss} MB; produce CPU: $(awk -v a="$cpu0" -v b="$cpu1" 'BEGIN { printf "%.1f", b - a }') s"

echo "  queries: $VUS concurrent for $DURATION"
lat=$(docker run --rm --network host -e BASE=$API -e TOPIC=$T -e VUS="$VUS" -e DURATION="$DURATION" -e AGGS="${AGGS:-0}" \
  -i "$K6_IMAGE" run --quiet - < "$HERE/queries.js" 2>/dev/null)
jq -n --arg label "$LABEL" --argjson n "$N" --arg produce "$produce_s" --arg fresh "$fresh_s" --arg all "$all_s" \
  --arg answers "$answers" --arg range "$range" --arg hist "$hist" --argjson archives "$archives" \
  --argjson disk "$disk" --argjson rss "$rss" --argjson latency "${lat:-{\}}" \
  '{label: $label, docs: $n, produce_s: ($produce|tonumber), searchable_after_s: ($fresh|tonumber), all_searchable_after_s: ($all|tonumber),
    answers: ($answers | split("\n")), range: $range, histogram: $hist, archives: $archives,
    disk_mb: $disk, rss_mb: $rss, latency: $latency}' > "$RES"
jq -c '.latency | to_entries[] | select(.value["p(50)"] != null) | {q: .key, p50: .value["p(50)"], p99: .value["p(99)"], n: .value.count}' "$RES" | sed 's/^/  /'
echo "  results: $RES"
