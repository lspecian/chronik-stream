#!/bin/bash
# Many topics created at once must not stop a broker.
#
# The background watermark sync iterated the produce handler's partition map
# and awaited one metadata write per partition inside the loop, holding a
# shard's read lock across each await. A new partition state takes that
# shard's write lock synchronously on a runtime thread, so with topics being
# created (auto-create, replica fetcher installing followers) every runtime
# thread ended up parked on the write lock, and the sync task, the only one
# able to release it, never ran again: the broker stopped answering anything,
# for good, process alive. Found while 2,000 topics were being created for the
# retention-keep test; reproduced on v2.20.0 within minutes (16 concurrent
# auto-creates); stacks showed all 16 workers in `install_partition_state`.
# The WAL's stale-segment sealer had the same shape over its queue map.
#
# Here: 3 nodes, $TOPICS topics auto-created 16 at a time while each node is
# watched. PASS = no node ever goes $STALL_S seconds without its periodic
# "Produce metrics" line, every create returns within 60 s, every topic exists
# on every node, and an acks=all write to a fresh topic still works at the end.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
B=localhost:9092,localhost:9093,localhost:9094
TOPICS=${CONCURRENT_CREATES_TOPICS:-1500}
STALL_S=${CONCURRENT_CREATES_STALL_S:-40}
cd "$REPO"
. tests/cluster/ports_free.sh
up() { for i in $(seq 90); do $K/kafka-topics.sh --bootstrap-server $1 --list >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
# Seconds since node $1 last logged its 10-second produce metrics line.
silent_for() {
  local last; last=$(sed 's/\x1b\[[0-9;]*m//g' tests/cluster/logs/node$1.log | grep "Produce metrics" | tail -1 | cut -c1-19)
  [ -z "$last" ] && { echo 0; return; }
  echo $(( $(date -u +%s) - $(date -u -d "$last" +%s) ))
}

echo "=== $LABEL ($TEST_BIN)"
CHRONIK_BIN=$TEST_BIN ./tests/cluster/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up localhost:9092 || { echo "FAIL: cluster did not start"; exit 1; }
sleep 5

W=tests/cluster/data/concurrent_creates; mkdir -p $W; : > $W/slow
start=$SECONDS
( seq 1 "$TOPICS" | xargs -P 16 -I{} sh -c \
    "echo x | timeout 60 kcat -P -b $B -t cc{}-$$ -X acks=all 2>/dev/null || echo cc{} >> $W/slow" ) &
gen=$!
worst=0; frozen=""
while kill -0 $gen 2>/dev/null; do
  sleep 5
  for n in 1 2 3; do
    s=$(silent_for $n); [ "$s" -gt "$worst" ] && worst=$s
    [ "$s" -gt "$STALL_S" ] && frozen="node$n silent ${s}s"
  done
  [ -n "$frozen" ] && break
done
if [ -n "$frozen" ]; then
  kill $gen 2>/dev/null; pkill -f "kcat -P -b $B -t cc" 2>/dev/null
  ./tests/cluster/stop.sh >/dev/null 2>&1
  echo "  FAIL $frozen (a broker stopped answering)"
  echo "RESULT $LABEL: FAIL"; exit 1
fi
wait $gen
echo "  $TOPICS auto-creates in $((SECONDS - start)) s; longest node silence ${worst}s (limit ${STALL_S}s)"

fail=0
slow=$(wc -l < $W/slow)
[ "$slow" = 0 ] && echo "  ok   every create returned within 60 s" || { echo "  FAIL $slow creates took over 60 s"; fail=1; }
sleep 5
for n in 1 2 3; do
  c=$($K/kafka-topics.sh --bootstrap-server localhost:$((9091 + n)) --list 2>/dev/null | grep -c "^cc[0-9]*-$$\$")
  [ "$c" = "$TOPICS" ] && echo "  ok   node$n lists all $TOPICS topics" || { echo "  FAIL node$n lists $c of $TOPICS topics"; fail=1; }
done
T=cc-final-$$
seq 1 1000 | kcat -P -b $B -t $T -X acks=all 2>/dev/null
got=$(timeout 60 kcat -C -b $B -t $T -o beginning -e -q 2>/dev/null | wc -l)
[ "$got" = 1000 ] && echo "  ok   acks=all to a fresh topic afterwards: 1000/1000" || { echo "  FAIL acks=all afterwards: $got/1000"; fail=1; }

./tests/cluster/stop.sh >/dev/null 2>&1
[ $fail = 0 ] && echo "RESULT $LABEL: PASS" || echo "RESULT $LABEL: FAIL"
exit $fail
