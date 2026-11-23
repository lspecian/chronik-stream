#!/bin/bash
# A leader that lost its leader-epoch history still answers for the epoch it
# leads at.
#
# Before: the leader answered OffsetForLeaderEpoch from its local history only.
# Where that history was gone (checkpoints missing, the WAL it is rebuilt from
# already reclaimed) every answer was "unknown", so its followers could never
# truncate a divergent tail there, and every handshake logged one warning per
# partition. On a live cluster 9,308 of 9,308 answers were "unknown", all for
# the current epoch.
#
#   1. Three nodes; a topic of 6 partitions, RF3, written and archived so the
#      WAL is reclaimed (history can then only come from the checkpoints).
#   2. The leader of those partitions loses its checkpoints for the topic and
#      restarts.
#   3. Followers re-run the handshake (a topic create re-plans them): the
#      leader answers every epoch query (none "unknown"), followers log no
#      "Not truncating", and every record is still readable from every node.
#
# Usage: epoch_without_history.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
source "$(dirname "$0")/../catalog/lib.sh"
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
T=ewh_topic; N=20000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
distinct_via() { kcat -C -b "localhost:$1" -t $T -o beginning -e -q -f '%k\n' 2>/dev/null | sort -u | wc -l; }

echo "=== $LABEL ($CHRONIK_BIN)"
EXTRA_ENV="CHRONIK_WAL_ROTATION_SIZE=64KB" cluster_start || { echo "FAIL: cluster did not start"; exit 1; }
trap cluster_stop EXIT
# All six partitions led by node 1, so one node's history is what is lost.
"$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic $T \
  --replica-assignment 1:2:3,1:2:3,1:2:3,1:2:3,1:2:3,1:2:3 >/dev/null 2>&1
sleep 5
seq 1 $N | awk '{ printf "%d|{\"n\":%d,\"pad\":\"%080d\"}\n", $1, $1, 0 }' \
  | kcat -P -K '|' -b "$BOOTS" -t $T -X acks=all 2>/dev/null
expect "records before" "$(distinct_via 9092)" "$N"

# Wait until node 1 has reclaimed WAL segments for the topic (archived).
reclaimed=0
for _ in $(seq 90); do
  segs=$(find "$DIR/data/node1/wal/$T" -name 'wal_*.log' 2>/dev/null | wc -l)
  arch=$(find "$DIR/data/node1" -path "*$T*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l)
  [ "$arch" -ge 6 ] && [ "$segs" -le 6 ] && { reclaimed=1; break; }
  sleep 5
done
expect "node 1 archived the topic and reclaimed its WAL" "$reclaimed" "1"
expect "node 1 holds epoch history for the topic" "$(ls "$DIR/data/node1/leader-epochs/$T" 2>/dev/null | wc -l)" "6"

echo "-- node 1 loses its epoch history for the topic and restarts"
before=$(wc -l < "$DIR/logs/node1.log")
node_kill 1
rm -rf "$DIR/data/node1/leader-epochs/$T"
node_start 1 || { echo "  FAIL node 1 did not restart"; fail=1; }
for _ in $(seq 60); do
  "$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --describe --topic $T 2>/dev/null | grep -q 'Leader: 1' && break
  sleep 2
done
sleep 10
# A topic create re-plans every follower: each re-runs the handshake with node 1.
"$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic ${T}_poke --partitions 1 --replication-factor 3 >/dev/null 2>&1
sleep 15

# Node 1's answers since the restart, in both log formats: a summary per
# request ("answered N partition(s), M with an unknown epoch") and, from older
# builds, one line per partition ("epoch E ends at X").
since=$(tail -n +"$((before + 1))" "$DIR/logs/node1.log" | sed 's/\x1b\[[0-9;]*m//g')
asked=$(( $(grep -oE 'answered [0-9]+ partition' <<<"$since" | awk '{ s += $2 } END { print s+0 }') \
  + $(grep -cE "OffsetForLeaderEpoch $T-[0-9]+: epoch" <<<"$since") ))
unknown=$(( $(grep -oE '[0-9]+ with an unknown epoch' <<<"$since" | awk '{ s += $1 } END { print s+0 }') \
  + $(grep -cE "OffsetForLeaderEpoch $T-[0-9]+: epoch [0-9]+ ends at -1" <<<"$since") ))
expect "node 1 answered epoch queries after the restart" "$( [ "$asked" -ge 1 ] && echo yes || echo "asked=$asked")" "yes"
expect "answers with an unknown epoch" "$unknown" "0"
expect "followers that could not reconcile" "$(cat "$DIR/logs/node2.log" "$DIR/logs/node3.log" | grep -ci 'not truncating')" "0"
for port in 9092 9093 9094; do
  expect "records via $port" "$(distinct_via $port)" "$N"
done
expect "panics" "$(panics)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
