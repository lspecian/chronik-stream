#!/bin/bash
# The search benchmark against OpenSearch: the same gen.sh corpus, the same
# measurements and the same k6 query mix as run.sh does for Chronik, so the two
# result files line up. OpenSearch runs in Docker on this machine with an
# explicit mapping (keyword strings, integer numbers, epoch-millis date), as a
# user who knows the data would set it up.
#
# Usage: opensearch.sh <corpus-dir> <work-dir> [label]
# Env: CONC (parallel _bulk senders, 8), VUS (8), DURATION (60s), AGGS=1,
#      OS_IMAGE (opensearchproject/opensearch:2.19.1), OS_HEAP (4g), K6_IMAGE.
# Needs: docker, jq.
set -u
CORPUS=${1:?corpus dir}; WORK=${2:?work dir}; LABEL=${3:-opensearch}
CONC=${CONC:-8}; VUS=${VUS:-8}; DURATION=${DURATION:-60s}
OS_IMAGE=${OS_IMAGE:-opensearchproject/opensearch:2.19.1}; OS_HEAP=${OS_HEAP:-4g}
K6_IMAGE=${K6_IMAGE:-grafana/k6:0.54.0}
HERE="$(cd "$(dirname "$0")" && pwd)"
API=http://localhost:9200; T=logs
N=$(cat "$CORPUS"/bulk-*.ndjson | wc -l)
DATA="$WORK/$LABEL-data"; RES="$WORK/$LABEL.json"; BULK="$WORK/$LABEL-bulk"
mkdir -p "$WORK"; rm -rf "$DATA" "$BULK"; mkdir -p "$DATA" "$BULK"; chmod 777 "$DATA"
now() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.1f", b - a }'; }
# _bulk bodies: an action line before every document (prepared before timing).
for f in "$CORPUS"/bulk-*.ndjson; do sed 's/^/{"index":{}}\n/' "$f" > "$BULK/$(basename "$f")"; done

docker rm -f "$LABEL" >/dev/null 2>&1
docker run -d --name "$LABEL" -p 9200:9200 \
  -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
  -e "OPENSEARCH_JAVA_OPTS=-Xms$OS_HEAP -Xmx$OS_HEAP" \
  -v "$(cd "$DATA" && pwd)":/usr/share/opensearch/data "$OS_IMAGE" >/dev/null
trap 'docker rm -f "$LABEL" >/dev/null 2>&1' EXIT
CG=/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "$LABEL").scope
cpu() { awk '/usage_usec/ { printf "%.1f", $2 / 1e6 }' "$CG/cpu.stat"; }
rss_mb() { awk '{ printf "%d", $1 / 1048576 }' "$CG/memory.current"; }
for _ in $(seq 120); do curl -fs "$API/" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "$API/" >/dev/null || { echo "OpenSearch did not start"; exit 1; }
curl -s -XPUT "$API/$T" -H 'Content-Type: application/json' -d '{
  "mappings": {"properties": {
    "ts": {"type": "date", "format": "epoch_millis"},
    "level": {"type": "keyword"}, "service": {"type": "keyword"}, "host": {"type": "keyword"},
    "status": {"type": "integer"}, "latency_ms": {"type": "integer"},
    "message": {"type": "text"}}}}' >/dev/null
search() { curl -s -m 60 -X POST "$API/$T/_search" -H 'Content-Type: application/json' -d "$1"; }

echo "=== $LABEL: ingest $N documents ($CONC senders)"
cpu0=$(cpu); t0=$(now)
bad=$(ls "$BULK"/bulk-*.ndjson | xargs -P "$CONC" -I{} curl -s -o /dev/null -w '%{http_code}\n' \
  -XPOST "$API/$T/_bulk" -H 'Content-Type: application/x-ndjson' --data-binary @{} | grep -vc '^200$')
produce_s=$(since "$t0"); cpu1=$(cpu)
echo "  ingest: ${produce_s}s (non-200 responses: $bad)"

printf '%s\n' '{"index":{}}' '{"ts":1767225600000,"level":"INFO","service":"svc-0","host":"host-00","status":200,"latency_ms":1,"message":"zzfinalmarkerzz"}' \
  | curl -s -o /dev/null -XPOST "$API/$T/_bulk" -H 'Content-Type: application/x-ndjson' --data-binary @-
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

sleep 60 # background merges settle
segments=$(curl -s "$API/_cat/segments/$T?format=json" | jq length)
disk=$(du -sm "$DATA" | cut -f1); rss=$(rss_mb)
echo "  segments: $segments; disk: ${disk} MB; memory: ${rss} MB; ingest CPU: $(awk -v a="$cpu0" -v b="$cpu1" 'BEGIN { printf "%.1f", b - a }') s"

echo "  queries: $VUS concurrent for $DURATION"
lat=$(docker run --rm --network host -e BASE=$API -e TOPIC=$T -e VUS="$VUS" -e DURATION="$DURATION" -e AGGS="${AGGS:-0}" \
  -i "$K6_IMAGE" run --quiet - < "$HERE/queries.js" 2>/dev/null)
jq -n --arg label "$LABEL" --argjson n "$N" --arg produce "$produce_s" --arg fresh "$fresh_s" --arg all "$all_s" \
  --arg answers "$answers" --arg range "$range" --arg hist "$hist" --argjson archives "$segments" \
  --argjson disk "$disk" --argjson rss "$rss" --argjson latency "${lat:-{\}}" \
  '{label: $label, docs: $n, produce_s: ($produce|tonumber), searchable_after_s: ($fresh|tonumber), all_searchable_after_s: ($all|tonumber),
    answers: ($answers | split("\n")), range: $range, histogram: $hist, archives: $archives,
    disk_mb: $disk, rss_mb: $rss, latency: $latency}' > "$RES"
jq -c '.latency | to_entries[] | select(.value["p(50)"] != null) | {q: .key, p50: .value["p(50)"], p99: .value["p(99)"], n: .value.count}' "$RES" | sed 's/^/  /'
echo "  results: $RES"
