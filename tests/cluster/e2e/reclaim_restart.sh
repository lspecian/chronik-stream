#!/bin/bash
# After v2.16.9 reclaims a partition's ENTIRE WAL, does a full restart keep its
# offsets? Boot restores high watermarks only for partitions still in the WAL.
#   PASS = end offsets unchanged after restart, new produces continue from the
#          old end (no reused offsets), every record readable exactly once,
#          also after the leader of each partition is killed.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
B=localhost:9092,localhost:9093,localhost:9094
T=reclaimed; LIVE=liveonly; N=200; M=50
cd $REPO
up() { for i in $(seq 90); do $K/kafka-topics.sh --bootstrap-server $1 --list >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
ends() { $K/kafka-get-offsets.sh --bootstrap-server $B --topic $T 2>/dev/null | sort | paste -sd' '; }
walfiles() { find tests/cluster/data/node*/wal/$T -name "*.log" -size +0 2>/dev/null | wc -l; }
readp() { timeout 60 kcat -C -b $B -t $T -p $1 -o beginning -e -q -f '%o %s\n' 2>/dev/null; }
check() {  # label
  local bad=0
  for p in 0 1 2; do
    local out; out=$(readp $p)
    local offs vals dupo
    offs=$(echo "$out" | grep -c .); vals=$(echo "$out" | awk '{print $2}' | sort -u | grep -c .)
    dupo=$(echo "$out" | awk '{print $1}' | sort | uniq -d | grep -c .)
    local exp=$2
    echo "  $1 p$p: records=$offs distinct_values=$vals dup_offsets=$dupo (expect $exp)"
    [ "$offs" = "$exp" ] && [ "$vals" = "$exp" ] && [ "$dupo" = 0 ] || bad=1
  done
  return $bad
}

echo "=== $LABEL ($TEST_BIN)"
CHRONIK_BIN=$TEST_BIN ./tests/cluster/start.sh >/dev/null 2>&1; up localhost:9092 || { echo "cluster did not start"; exit 1; }
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 3 --replication-factor 3 >/dev/null 2>&1
$K/kafka-topics.sh --bootstrap-server $B --create --topic $LIVE --partitions 1 --replication-factor 3 >/dev/null 2>&1; sleep 8
for p in 0 1 2; do seq 1 $N | sed "s/^/p$p-a/" | kcat -P -b $B -t $T -p $p -X acks=all -X batch.num.messages=20; done
echo "setup: produced $N per partition; ends: $(ends)"
for i in $(seq 40); do sleep 10; [ "$(walfiles)" = 0 ] && break; done
echo "setup: WAL files left for $T on all nodes: $(walfiles) (after $((i*10))s); segment files: $(find tests/cluster/data/node*/segments -path "*/$T/*" -name '*.segment' | wc -l)"
[ "$(walfiles)" = 0 ] || { echo "INCONCLUSIVE: WAL was not fully reclaimed"; ./tests/cluster/stop.sh >/dev/null 2>&1; exit 2; }
echo x | kcat -P -b $B -t $LIVE -p 0 -X acks=all   # keep the WAL non-empty, as on every real node
PRE=$(ends)

./tests/cluster/stop.sh >/dev/null 2>&1; sleep 2
CHRONIK_BIN=$TEST_BIN ./tests/cluster/restart.sh >/dev/null 2>&1; up localhost:9092; sleep 20
POST=$(ends)
echo "ends before restart: $PRE"
echo "ends after restart:  $POST"
for p in 0 1 2; do seq 1 $M | sed "s/^/p$p-b/" | kcat -P -b $B -t $T -p $p -X acks=all -X batch.num.messages=10; done
echo "after producing $M more per partition: $(ends)"
fail=0
[ "$PRE" = "$POST" ] || fail=1
check "full read" $((N+M)) || fail=1
for p in 0 1 2; do
  echo -n "  p$p offsets of the new records: "; readp $p | grep " p$p-b" | awk '{print $1}' | sed -n '1p;$p' | paste -sd'-'
done
L=$($K/kafka-topics.sh --bootstrap-server $B --describe --topic $T 2>/dev/null | grep -P "Partition: 0\t" | awk '{print $6}')
kill $(cat tests/cluster/data/node$L.pid); sleep 25
echo "killed node$L (leader of p0)"
check "after failover" $((N+M)) || fail=1
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi

