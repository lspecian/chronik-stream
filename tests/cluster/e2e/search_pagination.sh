#!/bin/bash
# Paging through a search (`from`/`size`) returns every match exactly once.
#
# v2.17.2 and earlier applied `from` in the wrong places: each archived segment
# skipped its own first `from` hits, the hot index ignored `from`, and neither
# the node nor the cluster merge skipped it on the merged list; ties (every hit
# of match_all) were broken differently by each source. Page 2 repeated page 1
# and records that belonged on later pages never appeared on any.
#
# Topic 2 holds more records per source than the page window, so each source
# has to cut its top from + size in the order the merge uses. (The v2.17.3
# failure on thunderbird came from leftover RealtimeIndexer indexes, which no
# current binary writes; a unit test in chronik-search covers those.)
#
# Topic 1: 600 JSON records over 3 partitions, RF3 (every node holds them all).
# For match_all and for a query matching half the records, page through with
# from = 0, s, 2s, ... and check the pages hold no duplicate, cover every
# match, equal one search for all of them, and come back the same when asked
# twice. Topic 2: 6,000 records (2,000 a partition); page through the first
# 500 and compare with one search of 500. Each while the records are hot, once
# archived, and after a restart with the hot index off. Plus the result window
# limit (from + size <= 10,000).
#
# Usage: search_pagination.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092
cd "$REPO"
fail=0
N=600
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
expect() { # what actual expected
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
ids() { # topic query from size -> one _id per line, in response order
  curl -s -m 60 -X POST "localhost:6092/$1/_search" -H 'Content-Type: application/json' \
    -d "{\"query\":$2,\"from\":$3,\"size\":$4}" | grep -o '"_id":"[^"]*"' | cut -d'"' -f4
}
# check <label> <topic> <query> <page size> <expected matches>
check() {
  local what=$1 t=$2 q=$3 s=$4 want=$5 tmp
  tmp=$(mktemp -d)
  ids "$t" "$q" 0 "$want" > "$tmp/all"
  local from=0
  : > "$tmp/paged"
  while [ $from -lt $((want + s)) ]; do
    ids "$t" "$q" $from "$s" >> "$tmp/paged"
    from=$((from + s))
  done
  ids "$t" "$q" "$s" "$s" > "$tmp/p2a"; ids "$t" "$q" "$s" "$s" > "$tmp/p2b"
  expect "$what: one search returns every match" "$(sort -u "$tmp/all" | wc -l)" "$want"
  expect "$what: no record on two pages" "$(sort "$tmp/paged" | uniq -d | wc -l)" "0"
  expect "$what: pages cover every match" "$(sort -u "$tmp/paged" | wc -l)" "$want"
  expect "$what: pages in order = one search" "$(cmp -s "$tmp/all" "$tmp/paged" && echo same || echo differ)" "same"
  expect "$what: a page asked twice is the same" "$(cmp -s "$tmp/p2a" "$tmp/p2b" && echo same || echo differ)" "same"
  rm -rf "$tmp"
}
# deep <label> <topic> <query> <page size> <depth>: the first <depth> hits of a
# topic holding many more, paged vs one search — each source must cut its top
# from + size the way the merge orders them.
deep() {
  local what=$1 t=$2 q=$3 s=$4 depth=$5 tmp
  tmp=$(mktemp -d)
  ids "$t" "$q" 0 "$depth" > "$tmp/all"
  : > "$tmp/paged"
  for from in $(seq 0 "$s" $((depth - s))); do
    ids "$t" "$q" "$from" "$s" >> "$tmp/paged"
  done
  expect "$what: one search returns $depth" "$(sort -u "$tmp/all" | wc -l)" "$depth"
  expect "$what: no record on two pages" "$(sort "$tmp/paged" | uniq -d | wc -l)" "0"
  expect "$what: pages in order = one search" "$(cmp -s "$tmp/all" "$tmp/paged" && echo same || echo differ)" "same"
  rm -rf "$tmp"
}
round() { # label
  echo "$1:"
  check "match_all" $T '{"match_all":{}}' 50 $N
  check "half the records" $T '{"match":{"parity":"even"}}' 37 $((N / 2))
  deep "deep match_all" $D '{"match_all":{}}' 50 500
  deep "deep half the records" $D '{"match":{"parity":"even"}}' 50 500
}

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
T=pg-$LABEL
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 3 --replication-factor 3 >/dev/null 2>&1
sleep 5
D=pgd-$LABEL
$K/kafka-topics.sh --bootstrap-server $B --create --topic $D --partitions 3 --replication-factor 3 >/dev/null 2>&1
sleep 5
produce() { # topic first last partition
  seq "$2" "$3" | awk '{
      printf "{\"id\":%d,\"parity\":\"%s\"}\n", $1, ($1 % 2 == 0) ? "even" : "odd" }' \
    | kcat -P -b $B -t "$1" -p "$4" -X acks=all 2>/dev/null
}
for p in 0 1 2; do
  produce $T $((p * 200 + 1)) $((p * 200 + 200)) $p
  produce $D $((p * 2000 + 1)) $((p * 2000 + 2000)) $p
done
sleep 5
round "hot"

for _ in $(seq 60); do
  [ "$(find $C/data -path "*/$T/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l)" -ge 9 ] && [ "$(find $C/data -path "*/$D/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l)" -ge 9 ] && break
  sleep 5
done
echo "archives on disk: $(find $C/data -path "*/$T/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l) + $(find $C/data -path "*/$D/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l)"
sleep 10
round "archived, hot index on"

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
sleep 15
round "restarted, hot index off"

echo "result window:"
r=$(curl -s -m 30 -w ' %{http_code}' -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' \
  -d '{"query":{"match_all":{}},"from":9995,"size":10}')
expect "from + size over 10,000 is refused" "${r##* } $(grep -o 'Result window is too large' <<<"$r" | head -1)" "400 Result window is too large"
r=$(curl -s -m 30 -o /dev/null -w '%{http_code}' -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' \
  -d '{"query":{"match_all":{}},"from":9990,"size":10}')
expect "from + size of exactly 10,000 is served" "$r" "200"

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
