#!/bin/bash
# A crash at any point of an archive merge loses nothing and leaks nothing.
#
# A merge uploads the merged archive, swaps it into the catalogue in one step,
# then deletes the sources. Every node is killed (abort) right after the
# upload, then — on the next boot — right after the swap; a third, normal boot
# must leave each partition with exactly one archive per node and no leftover
# files (the orphan sweep), every record searchable once and consumable once.
#
# Topic A takes both crashes (its first merge dies after the upload, its retry
# after the swap); topic B merges normally on the last boot.
#
# Usage: archive_merge_crash.sh <binary> <label>
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
files() { # topic node -> archive files (any format) on that node
  find $C/data/node$2 -path "*/$1/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l
}
per_node() { echo "$(files $1 1) $(files $1 2) $(files $1 3)"; }
produce() { # topic first last
  seq "$2" "$3" | awk '{ printf "{\"id\":%d,\"w\":\"%s\"}\n", $1, ($1 == 222) ? "crashneedle" : "plain" }' \
    | kcat -P -b $B -t "$1" -p 0 -X acks=all 2>/dev/null
}
nodes_alive() { local n; n=$(pgrep -fc "start --config $REPO/$C/node[123].toml" 2>/dev/null); echo "${n:-0}"; }
boot_until_abort() { # what
  CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_ARCHIVE_MERGE_DELAY_SECS=5 CHRONIK_TEST_ARCHIVE_MERGE_ABORT=$1 \
    CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1
  # restart.sh may report the nodes dead: they abort on purpose, seconds in.
  # Whether the merge ran and died where intended is what the file counts show.
  for _ in $(seq 90); do [ "$(nodes_alive)" -eq 0 ] && return 0; sleep 1; done
  echo "  FAIL: nodes still alive after the $1 abort was due"; fail=1
  $C/stop.sh >/dev/null 2>&1
}

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_ARCHIVE_MERGE=false CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
TA=amca-$LABEL; TB=amcb-$LABEL
for t in $TA $TB; do $K/kafka-topics.sh --bootstrap-server $B --create --topic $t --partitions 1 --replication-factor 3 >/dev/null 2>&1; done
sleep 5
for burst in 0 1 2 3 4 5; do
  produce $TA $((burst * 50 + 1)) $((burst * 50 + 50))
  produce $TB $((burst * 50 + 1)) $((burst * 50 + 50))
  want=$(((burst + 1) * 3))
  for _ in $(seq 90); do
    a=$(find $C/data -path "*/$TA/*" \( -name '*.tar' -o -name '*.tar.gz' \) | wc -l)
    b=$(find $C/data -path "*/$TB/*" \( -name '*.tar' -o -name '*.tar.gz' \) | wc -l)
    [ "$a" -ge $want ] && [ "$b" -ge $want ] && break
    sleep 5
  done
done
expect "six archives a node, both topics" "$(per_node $TA) / $(per_node $TB)" "6 6 6 / 6 6 6"
$C/stop.sh >/dev/null 2>&1

echo "crash right after the upload:"
boot_until_abort after-upload
expect "A: sources plus the unswapped upload" "$(per_node $TA)" "7 7 7"
echo "crash right after the swap:"
boot_until_abort after-swap
expect "A: merged archive plus the undeleted sources" "$(per_node $TA)" "7 7 7"

echo "normal boot:"
CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_ARCHIVE_MERGE_DELAY_SECS=5 CHRONIK_ARCHIVE_MERGE_GRACE_SECS=5 \
  CHRONIK_ARCHIVE_ORPHAN_AGE_SECS=0 CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
for _ in $(seq 60); do
  [ "$(per_node $TA)" = "1 1 1" ] && [ "$(per_node $TB)" = "1 1 1" ] && break
  sleep 2
done
expect "A: one archive a node, leftovers swept" "$(per_node $TA)" "1 1 1"
expect "B: one archive a node" "$(per_node $TB)" "1 1 1"
for t in $TA $TB; do
  ids=$(curl -s -m 60 -X POST "localhost:6092/$t/_search" -H 'Content-Type: application/json' -d '{"query":{"match_all":{}},"size":300}' | grep -o '"_id":"[^"]*"' | sort -u | wc -l)
  expect "$t: all 300 records searchable, each once" "$ids" "300"
  hit=$(curl -s -m 60 -X POST "localhost:6092/$t/_search" -H 'Content-Type: application/json' -d '{"query":{"match":{"w":"crashneedle"}},"size":5}' | grep -o '"total":{"value":[0-9]*' | grep -o '[0-9]*$')
  expect "$t: the word is found once" "$hit" "1"
  offs=$(timeout 60 kcat -C -b $B -t $t -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null)
  expect "$t: offsets 0-299 consumed once each" "$(sort -n <<<"$offs" | uniq | wc -l) $(sort -n <<<"$offs" | uniq -d | wc -l)" "300 0"
done

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
