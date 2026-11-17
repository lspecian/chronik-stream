#!/bin/bash
# Followers whose log diverges from offset 0 (the live #58 aftermath), end to end.
#   setup (released v2.16.7): leader holds 1000 "leader-N" records in batches of 7;
#     both followers' WAL is replaced by 100 "stranded-N" records in batches of 10,
#     so their log end (100) falls inside the leader's batch [98, 104].
#   test  ($1): restart on that data. PASS = followers rebuild, ISR goes full, and
#     after the leader is killed the new leader serves exactly the leader's 1000.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
SETUP_BIN="${SETUP_BIN:?set SETUP_BIN to a v2.16.7 chronik-server, which writes the log layout this test needs}"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
B=localhost:9092,localhost:9093,localhost:9094
T=sd
W="$REPO/tests/cluster/data/e2e/straddle-$LABEL"; rm -rf "$W"; mkdir -p "$W"
cd $REPO
up() { for i in $(seq 90); do $K/kafka-topics.sh --bootstrap-server $1 --list >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
isr() { $K/kafka-topics.sh --bootstrap-server $B --describe --topic $T 2>/dev/null | grep "Partition: 0" | awk '{print "leader="$6, "isr="$10}'; }

echo "=== $LABEL"
# --- setup: the leader's history
CHRONIK_BIN=$SETUP_BIN ./tests/cluster/start.sh >/dev/null 2>&1; up localhost:9092 || { echo "cluster did not start"; exit 1; }
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 3 >/dev/null 2>&1; sleep 5
seq 0 999 | sed 's/^/leader-/' | kcat -P -b $B -t $T -p 0 -X batch.num.messages=7 -X linger.ms=200 -X acks=all
sleep 15
L=$(isr | sed -E 's/leader=([0-9]+).*/\1/'); echo "setup: $(isr) end=$(kcat -Q -b $B -t $T:0:-1 2>/dev/null | awk '{print $NF}')"
./tests/cluster/stop.sh >/dev/null 2>&1; sleep 2

# --- setup: a different history, from offset 0, on a standalone node
mkdir -p $W/standalone
CHRONIK_METRICS_PORT=13599 CHRONIK_UNIFIED_API_PORT=6599 CHRONIK_ADVERTISED_ADDR=localhost \
  $SETUP_BIN start --data-dir $W/standalone --kafka-port 9599 > $W/standalone.log 2>&1 &
SPID=$!; up localhost:9599 || { echo "standalone did not start"; exit 1; }
$K/kafka-topics.sh --bootstrap-server localhost:9599 --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1; sleep 2
seq 0 99 | sed 's/^/stranded-/' | kcat -P -b localhost:9599 -t $T -p 0 -X batch.num.messages=10 -X linger.ms=200 -X acks=all
sleep 3; kill -TERM $SPID; wait $SPID 2>/dev/null

for n in 1 2 3; do
  [ "$n" = "$L" ] && continue
  rm -rf tests/cluster/data/node$n/wal/$T/0 tests/cluster/data/node$n/segments/$T tests/cluster/data/node$n/columnar/$T
  cp -r $W/standalone/wal/$T/0 tests/cluster/data/node$n/wal/$T/0
done
echo "setup: leader=node$L; followers now hold $(ls $W/standalone/wal/$T/0 | wc -l) planted WAL file(s)"

# --- the binary under test
CHRONIK_BIN=$TEST_BIN ./tests/cluster/restart.sh >/dev/null 2>&1; up localhost:9092
fail=0
sleep 60
state=$(isr); echo "after 60s: $state"
replicas=$(echo "$state" | sed -E 's/.*isr=//' | tr ',' '\n' | grep -c .)
[ "$replicas" = 3 ] || { echo "  FAIL: ISR has $replicas replicas, expected 3"; fail=1; }
for n in 1 2 3; do
  [ "$n" = "$L" ] && continue
  f=tests/cluster/logs/node$n.log
  echo "  node$n: straddle_warnings=$(grep -c 'logs have diverged' $f) rebuilds=$(grep -c 'rebuilding it from the leader' $f) not_truncating=$(grep -c 'not truncating' $f)"
done

# --- which history does a follower now serve? kill the leader and read.
kill $(cat tests/cluster/data/node$L.pid); sleep 45
echo "leader node$L killed: $(isr)"
kcat -C -b localhost:9092,localhost:9093,localhost:9094 -t $T -p 0 -o beginning -e -q 2>/dev/null > $W/read.txt
got="total=$(wc -l < $W/read.txt) leader=$(grep -c '^leader-' $W/read.txt) stranded=$(grep -c '^stranded-' $W/read.txt) distinct=$(sort -u $W/read.txt | wc -l)"
echo "  read back: $got"
# The new leader serves exactly the old leader's history, none of the stranded one.
[ "$got" = "total=1000 leader=1000 stranded=0 distinct=1000" ] || fail=1
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
