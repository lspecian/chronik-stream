#!/bin/bash
# Delete a topic whose records were archived, recreate it under the same name.
# Nothing removed a deleted topic's archive, and the segment tier reads it.
#   PASS = the recreated topic starts at offset 0, serves only its own records
#          (before and after its own WAL is reclaimed), and no node keeps the
#          old topic's archive.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
B=localhost:9092,localhost:9093,localhost:9094
T=reborn; N=150; M=30
cd $REPO
up() { for i in $(seq 90); do $K/kafka-topics.sh --bootstrap-server $1 --list >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
ends() { $K/kafka-get-offsets.sh --bootstrap-server $B --topic $T 2>/dev/null | sort | paste -sd' '; }
walfiles() { find tests/cluster/data/node*/wal/$T -name '*.log' -size +0 2>/dev/null | wc -l; }
. tests/cluster/segment_text.sh
old_archived() { # segment files holding an old record
  local f n=0
  for f in tests/cluster/data/node*/segments/segments/$T/*/*.segment; do
    seg_text "$f" | grep -aqE "old-[0-9]+" && n=$((n + 1))
  done
  echo $n
}
check() {
  local out recs olds dupo
  out=$(timeout 60 kcat -C -b $B -t $T -p 0 -o beginning -e -q -f '%o %s\n' 2>/dev/null)
  recs=$(echo "$out" | grep -c .); olds=$(echo "$out" | grep -c " old-"); dupo=$(echo "$out" | awk '{print $1}' | sort | uniq -d | grep -c .)
  echo "  $1: records=$recs old_topic_records=$olds dup_offsets=$dupo first-last=$(echo "$out" | awk '{print $1}' | sed -n '1p;$p' | paste -sd-) (expect $M new, 0 old, 0-$((M-1)))"
  [ "$recs" = $M ] && [ "$olds" = 0 ] && [ "$dupo" = 0 ]
}

echo "=== $LABEL ($TEST_BIN)"
CHRONIK_BIN=$TEST_BIN ./tests/cluster/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed (cluster not (re)started)"; exit 1; }; up localhost:9092 || { echo "cluster did not start"; exit 1; }
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 3 >/dev/null 2>&1; sleep 8
seq 1 $N | sed 's/^/old-/' | kcat -P -b $B -t $T -p 0 -X acks=all -X batch.num.messages=20
for i in $(seq 40); do sleep 10; [ "$(walfiles)" = 0 ] && break; done
setup_archived=$(old_archived)
echo "setup: old topic archived ($setup_archived segment file(s) across nodes), WAL left: $(walfiles)"
# Nothing archived means nothing for the delete to purge: the check below
# would pass without testing anything.
[ "$setup_archived" -gt 0 ] || { echo "FAIL: setup archived nothing"; ./tests/cluster/stop.sh >/dev/null 2>&1; exit 1; }
$K/kafka-topics.sh --bootstrap-server $B --delete --topic $T >/dev/null 2>&1; sleep 15
echo "after delete: old-topic segment files left on all nodes: $(old_archived)"
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 3 >/dev/null 2>&1; sleep 8
echo "recreated; end offsets: $(ends)"
fail=0
[ "$(ends)" = "$T:0:0" ] || fail=1
seq 1 $M | sed 's/^/new-/' | kcat -P -b $B -t $T -p 0 -X acks=all -X batch.num.messages=10
sleep 5
check "before reclaim" || fail=1
for i in $(seq 40); do sleep 10; [ "$(walfiles)" = 0 ] && break; done
echo "recreated topic's WAL reclaimed (left: $(walfiles))"
check "after reclaim" || fail=1
left=$(old_archived); echo "  old-topic segment files on any node: $left (expect 0)"
[ "$left" = 0 ] || fail=1
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi

