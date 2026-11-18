#!/bin/bash
# Archives written before v2.18.0 are brought to the current format, and
# superseded archive files are swept — on keys as real clusters write them.
#
# v2.18.0 on thunderbird: its archives were all .tar.gz without term filters,
# merging rewrote only runs of small archives (most partitions have one or
# two), and the orphan sweep listed nothing because the catalogue's keys are
# absolute ("/data/tantivy_indexes/…") while listed keys are not — so word
# searches kept unpacking archives and hit the time limit, and 4,000-12,000
# superseded archive files a node stayed on disk. (The local cluster's keys are
# relative; the absolute-key listing is covered by a unit test in
# chronik-storage, archive_merge::the_sweep_works_with_absolute_catalogue_keys.)
# v2.18.0 here also deleted an archive holding offsets nothing else covered: it
# had no coverage check.
#
# Here: three bursts archived by LEGACY_BIN (.tar.gz, no filters; too few to
# merge), a superseded copy (segment-0-20 beside segment-0-49) and an archive
# holding offsets nothing registered covers planted on each node, then the
# binary under test with the merge pass on. Checks: each node's three archives
# are plain tars with term filters, the superseded file is gone, the uncovered
# one is kept, a word search skips every archive for an absent word, and the
# 150 records are searchable once and consumed once.
#
# Usage: archive_upgrade.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, LEGACY_BIN (a pre-v2.18.0 chronik-server).
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
LEGACY_BIN="${LEGACY_BIN:?set LEGACY_BIN to a pre-v2.18.0 chronik-server}"
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
count() { # node name-pattern -> files of U on that node
  find $C/data/node$1 -path "*/$U/*" -name "$2" 2>/dev/null | wc -l
}
per_node() { echo "$(count 1 "$1") $(count 2 "$1") $(count 3 "$1")"; }
produce() { # first last
  seq "$1" "$2" | awk '{ printf "{\"id\":%d,\"w\":\"%s\"}\n", $1, ($1 == 77) ? "upgradeneedle" : "plain" }' \
    | kcat -P -b $B -t $U -p 0 -X acks=all 2>/dev/null
}

echo "=== $LABEL ($TEST_BIN, archives from $LEGACY_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$LEGACY_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
U=au-$LABEL
$K/kafka-topics.sh --bootstrap-server $B --create --topic $U --partitions 1 --replication-factor 3 >/dev/null 2>&1
sleep 5
for burst in 0 1 2; do
  produce $((burst * 50 + 1)) $((burst * 50 + 50))
  for _ in $(seq 90); do [ "$(find $C/data -path "*/$U/*" -name '*.tar.gz' | wc -l)" -ge $(((burst + 1) * 3)) ] && break; sleep 5; done
done
$C/stop.sh >/dev/null 2>&1
expect "three .tar.gz archives a node" "$(per_node '*.tar.gz')" "3 3 3"
for n in 1 2 3; do
  d=$(dirname "$(find $C/data/node$n -path "*/$U/*" -name 'segment-0-49.tar.gz' | head -1)")
  cp "$d/segment-0-49.tar.gz" "$d/segment-0-20.tar.gz"       # superseded: inside 0-49
  cp "$d/segment-0-49.tar.gz" "$d/segment-9000-9100.tar.gz"  # nothing registered covers it
done

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_ARCHIVE_MERGE_DELAY_SECS=5 CHRONIK_ARCHIVE_MERGE_GRACE_SECS=5 \
  CHRONIK_ARCHIVE_ORPHAN_AGE_SECS=0 CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
for _ in $(seq 60); do [ "$(per_node 'segment-0-20.tar.gz')" = "0 0 0" ] && [ "$(per_node '*.tar')" = "3 3 3" ] && break; sleep 2; done
sleep 8 # the grace period, then the old files go
echo "after the upgrade:"
expect "three plain tars a node" "$(per_node '*.tar')" "3 3 3"
expect "each with its term filter" "$(per_node '*.tar.terms')" "3 3 3"
expect "the superseded copy swept" "$(per_node 'segment-0-20.tar.gz')" "0 0 0"
expect "the uncovered archive kept" "$(per_node 'segment-9000-9100.tar.gz')" "1 1 1"
expect "no other .tar.gz left" "$(per_node '*.tar.gz')" "1 1 1"

hit=$(curl -s -m 60 -X POST "localhost:6092/$U/_search" -H 'Content-Type: application/json' -d '{"query":{"match":{"w":"upgradeneedle"}},"size":5}')
expect "the word is found, once" "$(grep -o '"total":{"value":[0-9]*' <<<"$hit" | grep -o '[0-9]*$') $(grep -o '"offset":[0-9]*' <<<"$hit" | cut -d: -f2 | sort -u | paste -sd, -)" "1 76"
skipped=$(curl -s -m 60 -X POST "localhost:6092/$U/_search" -H 'Content-Type: application/json' -d '{"query":{"match":{"w":"zzznotherezzz"}},"size":5}' | grep -o '"_segments":{[^}]*}' | grep -o '"skipped":[0-9]*' | cut -d: -f2)
expect "an absent word skips every archive" "${skipped:-none}" "9"
ids=$(curl -s -m 60 -X POST "localhost:6092/$U/_search" -H 'Content-Type: application/json' -d '{"query":{"match_all":{}},"size":200}' | grep -o '"_id":"[^"]*"' | sort -u | wc -l)
expect "all 150 records searchable, each once" "$ids" "150"
offs=$(timeout 60 kcat -C -b $B -t $U -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null)
expect "offsets 0-149 consumed once each" "$(sort -n <<<"$offs" | uniq | wc -l) $(sort -n <<<"$offs" | uniq -d | wc -l)" "150 0"

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
