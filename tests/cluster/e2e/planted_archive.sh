#!/bin/bash
# Thunderbird's tb-1: a partition whose leader holds records only in its archive
# (no WAL), while the metadata store records a high watermark of 0. v2.16.9
# reported end offset 0, followers copied nothing and showed in sync, and the
# next produce would have been given offset 0 again.
#   PASS = end offset = archived records; followers copy them; new records get
#          the next offsets (none reused); every record readable once, also
#          after the leader is killed.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
B=localhost:9092,localhost:9093,localhost:9094
DONOR=donor; T=planted; N=160; M=20
cd $REPO
up() { for i in $(seq 90); do $K/kafka-topics.sh --bootstrap-server $1 --list >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
ends() { $K/kafka-get-offsets.sh --bootstrap-server $B --topic $1 2>/dev/null | sort | paste -sd' '; }
leader() { $K/kafka-topics.sh --bootstrap-server $B --describe --topic $T 2>/dev/null | grep -P "Partition: $1\t" | awk '{print $6}'; }
isr() { $K/kafka-topics.sh --bootstrap-server $B --describe --topic $T 2>/dev/null | grep -P "Partition: $1\t" | awk '{print $10}'; }
readp() { timeout 60 kcat -C -b $B -t $T -p $1 -o beginning -e -q -f '%o %s\n' 2>/dev/null; }
. tests/cluster/segment_text.sh
held() { seg_text tests/cluster/data/node$1/wal/$T/$3/*.log tests/cluster/data/node$1/segments/segments/$T/$3/*.segment | grep -ahoE "$2-[0-9]+" | sort -u | wc -l; }

echo "=== $LABEL ($TEST_BIN)"
CHRONIK_BIN=$TEST_BIN ./tests/cluster/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed (cluster not (re)started)"; exit 1; }; up localhost:9092 || { echo "cluster did not start"; exit 1; }
for t in $DONOR $T; do $K/kafka-topics.sh --bootstrap-server $B --create --topic $t --partitions 3 --replication-factor 3 >/dev/null 2>&1; done
sleep 8
# Donor records in every partition, archived on every node.
for p in 0 1 2; do seq 1 $N | sed "s/^/p$p-old-/" | kcat -P -b $B -t $DONOR -p $p -X acks=all -X batch.num.messages=20; done
for i in $(seq 40); do sleep 10; [ "$(find tests/cluster/data/node*/wal/$DONOR -name '*.log' -size +0 | wc -l)" = 0 ] && break; done
echo "setup: donor archived on all nodes ($(find tests/cluster/data/node*/segments -path "*/$DONOR/*" -name '*.segment' | wc -l) segment files)"
declare -A L; for p in 0 1 2; do L[$p]=$(leader $p); done
echo "setup: $T leaders p0=node${L[0]} p1=node${L[1]} p2=node${L[2]}"
./tests/cluster/stop.sh >/dev/null 2>&1; sleep 2
# Plant: each partition's records only in its LEADER's archive. No WAL, and the
# metadata store never saw a produce to $T, so it records 0.
for p in 0 1 2; do
  n=${L[$p]}; d=tests/cluster/data/node$n/segments/segments
  mkdir -p $d/$T/$p && cp $d/$DONOR/$p/*.segment $d/$T/$p/
done
echo "setup: planted; leader-held records: p0=$(held ${L[0]} p0-old 0) p1=$(held ${L[1]} p1-old 1) p2=$(held ${L[2]} p2-old 2)"

CHRONIK_BIN=$TEST_BIN ./tests/cluster/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed (cluster not (re)started)"; exit 1; }; up localhost:9092; sleep 45
echo "end offsets after restart: $(ends $T)"
fail=0
for p in 0 1 2; do [ "$(ends $T | grep -o "$T:$p:[0-9]*" | cut -d: -f3)" = $N ] || fail=1; done
for p in 0 1 2; do seq 1 $M | sed "s/^/p$p-new-/" | kcat -P -b $B -t $T -p $p -X acks=all -X batch.num.messages=10; done
sleep 20
for p in 0 1 2; do
  out=$(readp $p); recs=$(echo "$out" | grep -c .); dupo=$(echo "$out" | awk '{print $1}' | sort | uniq -d | grep -c .)
  newo=$(echo "$out" | grep " p$p-new" | awk '{print $1}' | sed -n '1p;$p' | paste -sd-)
  f1=""; for n in 1 2 3; do [ "$n" = "${L[$p]}" ] || f1="$f1 node$n=$(held $n p$p-old $p)"; done
  echo "  p$p: records=$recs dup_offsets=$dupo new_offsets=$newo isr=$(isr $p) followers_hold_old:$f1"
  [ "$recs" = $((N+M)) ] && [ "$dupo" = 0 ] && [ "$newo" = "$N-$((N+M-1))" ] || fail=1
  for n in 1 2 3; do [ "$n" = "${L[$p]}" ] || [ "$(held $n p$p-old $p)" = $N ] || fail=1; done
done
kill $(cat tests/cluster/data/node${L[0]}.pid); sleep 25
out=$(readp 0); echo "  after killing node${L[0]} (p0 leader): p0 records=$(echo "$out" | grep -c .) distinct=$(echo "$out" | awk '{print $2}' | sort -u | grep -c .)"
[ "$(echo "$out" | awk '{print $2}' | sort -u | grep -c .)" = $((N+M)) ] || fail=1
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi

