#!/bin/bash
# Runs of small archives are merged into one, with nothing lost or doubled.
#
# A partition written in bursts gets one archive per indexer run: thunderbird's
# nodes hold 6,000-13,000, most of a few KB, and every search pays per archive.
# Here: one partition, six bursts of 50 records archived separately (six
# archives a node), then a restart with the merge pass enabled. Checks:
#   - each node's six archives become one, of plain tar format;
#   - the archives' records are all still searchable, once each, the word in
#     burst 4 included, and the merged archive's term filter skips it for a
#     word it does not hold;
#   - a consumer reads offsets 0-299 exactly once.
#
# Usage: archive_merge.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092
cd "$REPO"
fail=0
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
archives() { # [node] -> archive files of M (on one node, or all)
  find $C/data/node${1:-*} -path "*/$M/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l
}
produce() { # first last
  seq "$1" "$2" | awk '{ w = ($1 == 177) ? "mergedneedle" : "plain"; printf "{\"id\":%d,\"w\":\"%s\"}\n", $1, w }' \
    | kcat -P -b $B -t $M -p 0 -X acks=all 2>/dev/null
}

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
# No merging while the archives pile up.
CHRONIK_ARCHIVE_MERGE=false CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
M=am-$LABEL
$K/kafka-topics.sh --bootstrap-server $B --create --topic $M --partitions 1 --replication-factor 3 >/dev/null 2>&1
sleep 5
for burst in 0 1 2 3 4 5; do
  produce $((burst * 50 + 1)) $((burst * 50 + 50))
  for _ in $(seq 90); do [ "$(archives)" -ge $(((burst + 1) * 3)) ] && break; sleep 5; done
done
echo "before the merge: $(archives 1) / $(archives 2) / $(archives 3) archives on nodes 1/2/3"
expect "six archives a node" "$(archives 1) $(archives 2) $(archives 3)" "6 6 6"

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_ARCHIVE_MERGE_DELAY_SECS=5 CHRONIK_ARCHIVE_MERGE_GRACE_SECS=5 \
  CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
for _ in $(seq 60); do [ "$(archives)" -le 3 ] && break; sleep 2; done
echo "after the merge: $(archives 1) / $(archives 2) / $(archives 3)"
expect "one archive a node" "$(archives 1) $(archives 2) $(archives 3)" "1 1 1"
expect "the merged archives are plain tars" \
  "$(find $C/data -path "*/$M/*" -name 'segment-0-299.tar' 2>/dev/null | wc -l)" "3"
sed 's/\x1b\[[0-9;]*m//g' $C/logs/node1.log | grep -o 'merged archives.*' | head -1 | cut -c1-160 | sed 's/^/  log: /'

body=$(curl -s -m 60 -X POST "localhost:6092/$M/_search" -H 'Content-Type: application/json' -d '{"query":{"match":{"w":"mergedneedle"}},"size":10}')
expect "the word in burst 4 is found, once" \
  "$(grep -o '"total":{"value":[0-9]*' <<<"$body" | grep -o '[0-9]*$') $(grep -o '"offset":[0-9]*' <<<"$body" | sort -u | cut -d: -f2 | paste -sd, -)" "1 176"
ids=$(curl -s -m 60 -X POST "localhost:6092/$M/_search" -H 'Content-Type: application/json' -d '{"query":{"match_all":{}},"size":300}' | grep -o '"_id":"[^"]*"' | sort -u | wc -l)
expect "all 300 records searchable, each once" "$ids" "300"
skipped=$(curl -s -m 60 -X POST "localhost:6092/$M/_search" -H 'Content-Type: application/json' -d '{"query":{"match":{"w":"zzznotherezzz"}},"size":10}' | grep -o '"_segments":{[^}]*}' | grep -o '"skipped":[0-9]*' | cut -d: -f2)
expect "the merged archive's term filter skips it for an absent word" "${skipped:-none}" "3"
consumed=$(timeout 60 kcat -C -b $B -t $M -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | uniq | wc -l)
expect "a consumer reads offsets 0-299 once each" "$consumed" "300"
dups=$(timeout 60 kcat -C -b $B -t $M -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | uniq -d | wc -l)
expect "no offset delivered twice" "$dups" "0"

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
