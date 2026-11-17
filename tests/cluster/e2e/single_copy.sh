#!/bin/bash
# Thunderbird's single-copy partitions: the leader holds a partition only in
# its segment tier (no WAL dir), the followers hold nothing.
#   PASS = followers copy all 500 records, ISR goes full, and after the leader
#          is killed the new leader serves all 500.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
B=localhost:9092,localhost:9093,localhost:9094
T=onlyseg
cd $REPO
up() { for i in $(seq 90); do $K/kafka-topics.sh --bootstrap-server $1 --list >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
isr() { $K/kafka-topics.sh --bootstrap-server $B --describe --topic $T 2>/dev/null | grep -P "Partition: $P\t" | awk '{print "leader="$6, "isr="$10}'; }

echo "=== $LABEL ($TEST_BIN)"
CHRONIK_BIN=$TEST_BIN ./tests/cluster/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed (cluster not (re)started)"; exit 1; }; up localhost:9092 || { echo "cluster did not start"; exit 1; }
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 3 --replication-factor 3 >/dev/null 2>&1; sleep 8
# The catalog (Raft) leader, from each node's /admin/health (Unified API 6092+n-1).
R=$(for n in 1 2 3; do curl -s -m 2 localhost:$((6091 + n))/admin/health | grep -q '"is_leader":true' && echo $n; done | head -1)
[ -n "$R" ] || { echo "FAIL: no node reports catalog leadership"; exit 1; }
for P in 0 1 2; do L=$(isr | sed -E "s/leader=([0-9]+).*/\1/"); [ "$L" != "$R" ] && break; done
echo "setup: Raft (metadata) leader=node$R; using partition $P led by node$L"
for n in 1 2 3; do [ "$n" = "$L" ] || kill $(cat tests/cluster/data/node$n.pid); done
sleep 3
seq 0 499 | sed 's/^/rec-/' | kcat -P -b localhost:909$((L+1)) -t $T -p $P -X acks=1 -X batch.num.messages=50
echo "setup: leader=node$L took 500 records with both followers down; waiting for seal + archive"
for i in $(seq 30); do
  sleep 10
  n=$(find tests/cluster/data/node$L/segments -path "*/$T/$P/*" -name "*.segment" 2>/dev/null | wc -l)
  [ "$n" -gt 0 ] && break
done
echo "setup: leader segment files for $T: $(find tests/cluster/data/node$L/segments -path "*/$T/$P/*" -name '*.segment' | xargs -r -n1 basename | paste -sd' ')"
./tests/cluster/stop.sh >/dev/null 2>&1; sleep 2
rm -rf tests/cluster/data/node$L/wal/$T/$P
echo "setup: leader WAL dir for $T removed (segment tier only, as on Thunderbird)"

CHRONIK_BIN=$TEST_BIN ./tests/cluster/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed (cluster not (re)started)"; exit 1; }; up localhost:9092
fail=0
distinct() { timeout 60 kcat -C -b $B -t $T -p $P -o beginning -e -q 2>/dev/null | sort -u | wc -l; }
sleep 60
state=$(isr); echo "after 60s: $state"
replicas=$(echo "$state" | sed -E 's/.*isr=//' | tr ',' '\n' | grep -c .)
[ "$replicas" = 3 ] || { echo "  FAIL: ISR has $replicas replicas, expected 3"; fail=1; }
n=$(distinct); echo "read from leader: $n distinct of 500"; [ "$n" = 500 ] || fail=1
for n in 1 2 3; do [ "$n" = "$L" ] && continue; echo "  node$n WAL for $T: $(du -sk tests/cluster/data/node$n/wal/$T/$P 2>/dev/null | cut -f1)KB"; done
kill $(cat tests/cluster/data/node$L.pid); sleep 45
echo "leader node$L killed: $(isr)"
n=$(distinct); echo "read from new leader: $n distinct of 500"; [ "$n" = 500 ] || fail=1
panics=$(cat tests/cluster/logs/node*.log | grep -c panicked)
echo "panics: $panics"; [ "$panics" = 0 ] || fail=1
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi

