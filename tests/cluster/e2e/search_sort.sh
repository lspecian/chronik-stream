#!/bin/bash
# `sort` orders hits by the requested keys, across partitions, nodes, the hot
# index and archives.
#
# Up to v2.20.1 `sort` was parsed and ignored: `"sort": [{"offset": "asc"}]`
# answered in relevance order (the newest records first). Each source now
# collects its top `from + size` by the sort keys and the node and cluster
# merges order by them (crate::sort).
#
# 1,200 JSON records over 3 partitions, RF3: {"n", "status" (a number, absent
# from every 7th record), "level" (a string), "word"}. For several sorts, the
# first 60 ids (one search, and paged by 15) must equal the order computed
# here from the records produced, every hit must carry the `sort` values of
# its record and no `_score` (no key needs one). Checked while the records are
# hot, once archived, and after a restart with the hot index off.
#
# Usage: search_sort.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092
cd "$REPO"
fail=0
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
expect() { # what actual expected
  if [ "$2" = "$3" ]; then echo "  ok   $1"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
WORK=$(mktemp -d)
search() { # sort-json query-json from size -> response body
  curl -s -m 60 -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' \
    -d "{\"query\":$2,\"sort\":$1,\"from\":$3,\"size\":$4}"
}
ids() { grep -o '"_id":"[^"]*"' | cut -d'"' -f4 | paste -sd' ' -; }

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
T=sort-$LABEL
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 3 --replication-factor 3 >/dev/null 2>&1
sleep 5
# Records: partition p holds n = p*400 .. p*400+399 at offsets 0..399.
: > "$WORK/rows" # id status level word
for p in 0 1 2; do
  seq $((p * 400)) $((p * 400 + 399)) | awk -v p=$p -v rows="$WORK/rows" '{
      n = $1; off = n - p * 400
      split("error info warn debug", lv, " "); level = lv[(n % 4) + 1]
      word = (n % 3 == 0) ? "alpha" : "beta"
      if (n % 7 == 0) { printf "{\"n\":%d,\"level\":\"%s\",\"word\":\"%s\"}\n", n, level, word; st = "-" }
      else { st = (n * 37) % 500; printf "{\"n\":%d,\"status\":%d,\"level\":\"%s\",\"word\":\"%s\"}\n", n, st, level, word }
      printf "%d-%d %s %s %s %d %d\n", p, off, st, level, word, p, off >> rows }' \
    | kcat -P -b $B -t $T -p $p -X acks=all 2>/dev/null
done
sleep 5

# want <name> <filter: all|alpha> <sort command over "id status level word p off">
want() {
  local rows="$WORK/rows"
  [ "$2" = alpha ] && { grep ' alpha ' "$WORK/rows" > "$WORK/f"; rows="$WORK/f"; }
  eval "$3" < "$rows" | head -60 | cut -d' ' -f1 | paste -sd' ' -
}
# Missing status ("-") sorts last both ways: give it a key past every value.
CASES=(
  'offset asc, partition asc|[{"offset":"asc"},{"partition":"asc"}]|{"match_all":{}}|all|sort -k6,6n -k5,5n'
  'status desc, partition, offset|[{"status":"desc"},"partition","offset"]|{"match_all":{}}|all|awk '"'"'{k=($2=="-")?-1:$2; print k, $0}'"'"' | sort -k1,1nr -k6,6n -k7,7n | cut -d" " -f2-'
  'status asc (missing last), offset desc, partition|[{"status":{"order":"asc"}},{"offset":"desc"},{"partition":"asc"}]|{"match_all":{}}|all|awk '"'"'{k=($2=="-")?99999:$2; print k, $0}'"'"' | sort -k1,1n -k7,7nr -k6,6n | cut -d" " -f2-'
  'level.keyword asc, offset desc, partition|[{"level.keyword":"asc"},{"offset":"desc"},"partition"]|{"match_all":{}}|all|sort -k3,3 -k6,6nr -k5,5n'
  'a word match by status desc, partition, offset|[{"status":"desc"},"partition","offset"]|{"match":{"word":"alpha"}}|alpha|awk '"'"'{k=($2=="-")?-1:$2; print k, $0}'"'"' | sort -k1,1nr -k6,6n -k7,7n | cut -d" " -f2-'
)

round() { # label
  echo "$1:"
  for c in "${CASES[@]}"; do
    IFS='|' read -r name sortj query filter cmd <<<"$c"
    local expected one paged body
    expected=$(want "$name" "$filter" "$cmd")
    body=$(search "$sortj" "$query" 0 60)
    one=$(ids <<<"$body")
    paged=$(for from in 0 15 30 45; do search "$sortj" "$query" $from 15 | ids; done | paste -sd' ' -)
    expect "$name: one search" "$one" "$expected"
    expect "$name: paged" "$paged" "$expected"
    expect "$name: no _score, sort values on every hit" \
      "$(grep -o '"_score":[^,}]*' <<<"$body" | sort -u | paste -sd' ' -) $(grep -o '"sort":\[' <<<"$body" | wc -l)" \
      '"_score":null 60'
  done
  # The first hit's sort values are its record's.
  local first; first=$(search '[{"status":"desc"},"partition","offset"]' '{"match_all":{}}' 0 1)
  expect "sort values of the first hit" "$(grep -o '"sort":\[[^]]*\]' <<<"$first")" '"sort":[499,0,27]'
}

round "hot"
for _ in $(seq 60); do
  [ "$(find $C/data -path "*/$T/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l)" -ge 9 ] && break
  sleep 5
done
echo "archives on disk: $(find $C/data -path "*/$T/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l)"
sleep 10
round "archived, hot index on"

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
sleep 15
round "restarted, hot index off"

rm -rf "$WORK"
$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
