#!/bin/bash
# POST /admin/reindex rebuilds a node's archived search indexes from its raw
# tier: archives written before the JSON field had columns then answer range
# queries and aggregations, with nothing lost or doubled.
#
# Here, one node: the previous binary archives N JSON records (no JSON columns),
# then this one, on the same data directory:
#   - before: a range on a JSON field cannot count the archived records;
#   - POST /admin/reindex, poll GET /admin/reindex until done: every archive
#     rebuilt, none incomplete or failed;
#   - after: total, range, terms per level match the corpus (awk); every
#     record consumable once; a word search finds the same records;
#   - a second reindex finds every archive current; after the grace period the
#     old files are gone (one file per catalogue entry); all of it again after a
#     restart.
#
# Usage: reindex.sh <binary> <previous-binary> <label>
# Needs: KAFKA_HOME, kcat, jq.
set -u
BIN=${1:?binary under test}; PREV=${2:?previous binary (archives without JSON columns)}; LABEL=${3:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/reindex-$LABEL; LOG=tests/cluster/logs/reindex-$LABEL
B=localhost:9092; T=reidx; N=20000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
tests/search_bench/gen.sh "$D/corpus" $N $N >/dev/null
CORPUS="$D/corpus/bulk-00000.ndjson"
T0=1767225600000; LO=$((T0 + 100000)); HI=$((T0 + 400000))
num() { grep -o "\"$1\":[0-9]*" "$CORPUS" | cut -d: -f2; }
str() { grep -o "\"$1\":\"[^\"]*\"" "$CORPUS" | cut -d'"' -f4; }
want_range=$(num ts | awk -v lo=$LO -v hi=$HI '$1 >= lo && $1 <= hi' | wc -l)
want_levels=$(str level | sort | uniq -c | awk '{ printf "%s=%s ", $2, $1 }' | sed 's/ $//')
want_timeout=$(grep -cw timeout "$CORPUS")

start() { # binary
  CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_HOT_TEXT_ENABLED=false \
    CHRONIK_ARCHIVE_MERGE=false CHRONIK_ARCHIVE_MERGE_GRACE_SECS=2 \
    "$1" start --data-dir "$D/data" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL: server did not start"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }
search() { curl -s -m 60 -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' -d "$1"; }
range_total() { search "{\"size\":0,\"query\":{\"range\":{\"ts\":{\"gte\":$LO,\"lte\":$HI}}}}" | jq -c '[.hits.total.value, .hits.total.relation]'; }
archives() { find "$D/data" -path "*/tantivy_indexes/$T/*" -name '*.tar' | wc -l; }
catalogued() { jq "[.[\"$T-0\"][]?] | length" "$D/data/segment_index.json"; }
reindex() { # body -> final progress
  curl -s -m 10 -X POST localhost:6092/admin/reindex -H 'Content-Type: application/json' -d "$1" >/dev/null
  for _ in $(seq 120); do
    p=$(curl -s -m 10 localhost:6092/admin/reindex)
    [ "$(jq -r .running <<<"$p")" = false ] && { echo "$p"; return; }
    sleep 1
  done
  echo '{"running":true}'
}
check() { # when
  echo "$1:"
  expect "total" "$(search '{"size":0}' | jq '.hits.total.value')" "$N"
  expect "range on a JSON number" "$(range_total)" "[$want_range,\"eq\"]"
  expect "terms per level" \
    "$(search '{"size":0,"aggs":{"l":{"terms":{"field":"level","size":10}}}}' | jq -r '[.aggregations.l.buckets[] | "\(.key)=\(.doc_count)"] | sort | join(" ")')" "$want_levels"
  expect "word search" "$(search '{"size":0,"query":{"match":{"message":"timeout"}}}' | jq '.hits.total.value')" "$want_timeout"
  expect "every record consumed once" \
    "$(timeout 60 kcat -C -b $B -t $T -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | uniq | wc -l)" "$N"
}

echo "=== $LABEL ($BIN, archives written by $PREV)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh
trap stop EXIT
start "$PREV"
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
# Three archives: three bursts, each sealed (30 s idle) before the next.
for part in 1 2 3; do
  sed -n "$(( (part - 1) * N / 3 + 1 )),$(( part * N / 3 + (part == 3 ? N % 3 : 0) ))p" "$CORPUS" | kcat -P -b $B -t $T -p 0 2>/dev/null
  for _ in $(seq 40); do [ "$(archives)" -ge $part ] && break; sleep 3; done
done
expect "archives written by the previous binary" "$(archives)" "3"
stop

start "$BIN"
echo "before the reindex:"
expect "range cannot count the old archives" "$(range_total | jq -c '.[0] < '"$want_range")" "true"

p=$(reindex "{\"topic\":\"$T\"}")
echo "  reindex: $(jq -c '{archives, rebuilt, current, incomplete, raced, failed}' <<<"$p")"
expect "every archive rebuilt" "$(jq -c '[.archives, .rebuilt, .incomplete, .failed]' <<<"$p")" "[3,3,0,0]"
check "after the reindex"

p=$(reindex "{\"topic\":\"$T\"}")
expect "a second reindex finds them current" "$(jq -c '[.rebuilt, .current]' <<<"$p")" "[0,3]"
sleep 4 # grace period: the replaced files go
expect "one file per catalogue entry" "$(archives) $(catalogued)" "3 3"

stop; start "$BIN"
check "after a restart"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
