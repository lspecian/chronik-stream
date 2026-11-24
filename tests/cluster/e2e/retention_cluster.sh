#!/bin/bash
# Retention on a 3-node cluster: every replica deletes its own expired archive,
# the earliest offset moves, and after the leader is killed the new leader
# serves the same start.
#
# Usage: retention_cluster.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092,localhost:9093,localhost:9094; PER=500; T=retc-$LABEL
cd "$REPO"
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
segs() { ls $C/data/node$1/segments/segments/$T/0/*.segment 2>/dev/null | wc -l; }
earliest() { $K/kafka-get-offsets.sh --bootstrap-server $B --topic $T --time -2 2>/dev/null | awk -F: '{ print $3 }'; }
count_read() { timeout 60 kcat -C -b $B -t $T -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | wc -l; }

echo "=== $LABEL ($BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_RETENTION_DELAY_SECS=5 CHRONIK_RETENTION_CHECK_INTERVAL_SECS=5 CHRONIK_BIN=$BIN $C/start.sh >/dev/null 2>&1 \
  || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 3 --config retention.ms=-1 >/dev/null 2>&1
sleep 5
for burst in 1 2 3; do
  seq 1 $PER | awk -v b=$burst '{ printf "burst%d record %d\n", b, $1 }' | kcat -P -b $B -t $T -p 0 -X acks=all 2>/dev/null
  for _ in $(seq 40); do [ "$(segs 1)$(segs 2)$(segs 3)" = "$burst$burst$burst" ] && break; sleep 3; done
done
expect "three segments on every node" "$(segs 1) $(segs 2) $(segs 3)" "3 3 3"

$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T --add-config retention.ms=1000 >/dev/null 2>&1
for _ in $(seq 30); do [ "$(segs 1)$(segs 2)$(segs 3)" = 111 ] && break; sleep 2; done
expect "one segment left on every node" "$(segs 1) $(segs 2) $(segs 3)" "1 1 1"
expect "earliest offset" "$(earliest)" "$((2 * PER))"
expect "records read" "$(count_read)" "$PER"

leader=$($K/kafka-topics.sh --bootstrap-server $B --describe --topic $T 2>/dev/null | grep -o 'Leader: [0-9]*' | awk '{ print $2 }')
pid=$(pgrep -f "node$leader.toml" | head -1)
kill -9 $pid 2>/dev/null
echo "killed node$leader (leader)"
sleep 10
for _ in $(seq 15); do e=$(earliest); [ -n "$e" ] && break; sleep 2; done # until a new leader answers
expect "earliest offset after failover" "$e" "$((2 * PER))"
expect "records read after failover" "$(count_read)" "$PER"

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
