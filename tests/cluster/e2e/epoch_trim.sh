#!/bin/bash
# DeleteRecords reaches every replica, and the leader-epoch history below the
# new log start goes with the records.
#
# Before:
#   - DeleteRecords moved the log start on the leader only. Followers kept the
#     deleted records and served them again once one of them led.
#   - The leader-epoch history (and its checkpoint) was never trimmed from the
#     start: it kept one entry per leadership for the life of the partition.
#
#   1. Three nodes; one partition led by node 1. Records written in three
#      leaderships (node 1, another node while node 1 is down, node 1 again
#      once leadership returns to it), so the history has three entries.
#   2. DeleteRecords to an offset inside the third leadership: on every node
#      the checkpoint is one entry starting there.
#   3. Node 1 goes down: the new leader serves exactly the records after the
#      deleted ones. More records written; node 1 comes back, leads again and
#      every replica agrees (no "Not truncating").
#   4. Every node restarts: the log start and the trimmed history survive.
#
# Usage: epoch_trim.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
source "$(dirname "$0")/../catalog/lib.sh"
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
T=etrim_topic; CUT=7500
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
produce() { # first last
  seq "$1" "$2" | awk '{ printf "%d|v%d\n", $1, $1 }' | kcat -P -K '|' -b "$BOOTS" -t $T -X acks=all 2>/dev/null
}
# Records the current leader serves from the beginning: "count first last".
served() { kcat -C -b "$BOOTS" -t $T -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | awk 'NR==1{f=$1} {n++; l=$1} END {print n+0, f, l}'; }
leader() { "$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --describe --topic $T 2>/dev/null | grep -o 'Leader: [0-9-]*' | awk '{print $2}'; }
wait_leader_is() { for _ in $(seq 90); do [ "$(leader)" = "$1" ] && return 0; sleep 2; done; return 1; }
wait_leader_not() { for _ in $(seq 60); do l=$(leader); [ -n "$l" ] && [ "$l" != "$1" ] && [ "$l" != "-1" ] && return 0; sleep 2; done; return 1; }
history() { cat "$DIR/data/node$1/leader-epochs/$T/"* 2>/dev/null | grep -E '^[0-9]+ [0-9]+$' | tr '\n' ',' ; }

echo "=== $LABEL ($CHRONIK_BIN)"
EXTRA_ENV="CHRONIK_LEADER_REBALANCE_INTERVAL_SECS=10" cluster_start || { echo "FAIL: cluster did not start"; exit 1; }
trap cluster_stop EXIT
"$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic $T --replica-assignment 1:2:3 >/dev/null 2>&1
sleep 5

echo "-- three leaderships"
produce 1 3000
node_kill 1
wait_leader_not 1 || { echo "  FAIL leadership did not move off node 1"; fail=1; }
produce 3001 6000
node_start 1 || { echo "  FAIL node 1 did not restart"; fail=1; }
wait_leader_is 1 || { echo "  FAIL leadership did not return to node 1"; fail=1; }
sleep 3
produce 6001 9000
expect "served before the delete" "$(served)" "9000 0 8999"
sleep 3
for n in 1 2 3; do
  entries=$(history $n | tr ',' '\n' | grep -c .)
  expect "node $n history has entries from at least 2 leaderships" "$( [ "$entries" -ge 2 ] && echo yes || echo "$(history $n)")" "yes"
done

echo "-- DeleteRecords to $CUT"
printf '{"partitions":[{"topic":"%s","partition":0,"offset":%d}],"version":1}\n' $T $CUT > "$DIR/data/etrim-delete.json"
"$K"/kafka-delete-records.sh --bootstrap-server "$BOOTS" --offset-json-file "$DIR/data/etrim-delete.json" >/dev/null 2>&1
expect "served after the delete" "$(served)" "1500 $CUT 8999"
sleep 5
latest=$(history 1 | tr ',' '\n' | grep . | tail -1 | awk '{print $1}')
for n in 1 2 3; do
  expect "node $n history after the delete" "$(history $n)" "$latest $CUT,"
done

echo "-- node 1 goes down: a follower leads"
node_kill 1
wait_leader_not 1 || { echo "  FAIL leadership did not move off node 1"; fail=1; }
expect "served by node $(leader)" "$(served)" "1500 $CUT 8999"
produce 9001 10000
node_start 1 || { echo "  FAIL node 1 did not restart"; fail=1; }
wait_leader_is 1 || { echo "  FAIL leadership did not return to node 1"; fail=1; }
sleep 5
expect "served by node 1 again" "$(served)" "2500 $CUT 9999"

echo "-- every node restarts: the log start survives"
for n in 1 2 3; do node_kill $n; done
for n in 1 2 3; do node_start $n || { echo "  FAIL node $n did not restart"; fail=1; }; done
wait_leader_is 1 || { echo "  FAIL node 1 does not lead after the restart"; fail=1; }
sleep 5
expect "served after the restart" "$(served)" "2500 $CUT 9999"
for n in 1 2 3; do
  expect "node $n history after the restart" "$(history $n | cut -d, -f1)" "$latest $CUT"
done
expect "followers that could not reconcile" "$(grep -ci 'not truncating' "$DIR"/logs/node*.log | awk -F: '{s+=$2} END {print s+0}')" "0"
expect "panics" "$(panics)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
