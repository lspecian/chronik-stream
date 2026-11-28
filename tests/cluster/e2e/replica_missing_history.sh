#!/bin/bash
# A replica missing the start of a partition is repaired, and never leaves the
# start unreadable.
#
# Before v2.20.6 a follower handed records from X by a leader that said its log
# began at 0 took X as the start and carried on: it held [X, end), counted as in
# sync once its tail caught up, and could lead. Consumers reading from it got
# nothing below X, while another replica still held those records. On
# Thunderbird: 9 partitions, about 133,000 offsets.
#
# Each scenario starts a 3-node cluster with one partition (replicas 1,2,3),
# writes 20,000 records, waits until every node has archived the start and
# reclaimed its WAL, then removes a node's archived segments ending below
# 10,000 while it is stopped — the shape the bug left.
#
#   A. Follower missing the start (node 1, the preferred leader). Before: it
#      came back in sync and took the leadership back, serving from ~10,000.
#      Now it rebuilds from the leader first; once it leads, all 20,000 are
#      read from offset 0.
#   B. Leader missing the start (node 1 leads when the cluster restarts).
#      Before: it served from ~10,000 for good. Now it hands the leadership to
#      a replica that holds the start; all 20,000 are read.
#   C. No replica holds the start. Before: the earliest offset said 0 while the
#      first record was ~10,000. Now the log start moves to the first record
#      that exists: earliest offset = first record read, and every record from
#      there is read.
#
# Search archives are off for the topic: fetch falls back to them, and they
# would keep serving what the plant removes.
#
# Usage: replica_missing_history.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
source "$(dirname "$0")/../catalog/lib.sh"
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
T=mh_topic; N=20000; CUT=10000
ENV="CHRONIK_WAL_ROTATION_SIZE=64KB CHRONIK_LEADER_REBALANCE_INTERVAL_SECS=10 CHRONIK_DEFAULT_SEARCHABLE=false CHRONIK_DEFAULT_COLUMNAR=false"
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
raw() { echo "$DIR/data/node$1/segments/segments/$T/0"; }
# Archived segments of a node ending below CUT, and where its archive starts.
below_cut() { ls "$(raw "$1")" 2>/dev/null | awk -F'[-.]' -v cut=$CUT '$2 < cut'; }
archive_start() { ls "$(raw "$1")" 2>/dev/null | awk -F'[-.]' 'NR == 1 || $1 < m { m = $1 } END { print m + 0 }'; }
archive_end() { ls "$(raw "$1")" 2>/dev/null | awk -F'[-.]' '$2 > m { m = $2 } END { print m + 0 }'; }
leader() { "$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --describe --topic $T 2>/dev/null | grep -o 'Leader: [0-9-]*' | awk '{ print $2 }'; }
# "count first last" of what the current leader serves from the beginning.
served() { kcat -C -b "$BOOTS" -t $T -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | awk 'NR == 1 { f = $1 } { n++; l = $1 } END { print n + 0, f, l }'; }
earliest() { "$K"/kafka-get-offsets.sh --bootstrap-server "$BOOTS" --topic $T --time -2 2>/dev/null | cut -d: -f3; }
until_served() { # want, seconds
  local got=""
  for _ in $(seq $(($2 / 5))); do got=$(served); [ "$got" = "$1" ] && break; sleep 5; done
  echo "$got"
}
# Whether a node logged a line (logs start over with each cluster).
logged() { grep -qh "$2" "$DIR"/logs/node$1.log 2>/dev/null && echo yes || echo no; }
stop_all() { for n in 1 2 3; do node_kill $n; done; sleep 2; }
start_all() { for n in 1 2 3; do node_start $n || { echo "  FAIL node $n did not restart"; fail=1; }; done; }
plant() { for n in "$@"; do below_cut $n | while read -r f; do rm -f "$(raw $n)/$f"; done; done; }

setup() {
  EXTRA_ENV="$ENV" cluster_start || { echo "FAIL: cluster did not start"; exit 1; }
  "$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic $T --replica-assignment 1:2:3 >/dev/null 2>&1
  "$K"/kafka-configs.sh --bootstrap-server "$BOOTS" --alter --entity-type topics --entity-name $T \
    --add-config searchable=false,columnar.enabled=false >/dev/null 2>&1
  sleep 5
  # Four bursts, each sealed and archived on every node before the next:
  # segments 0-4999, 5000-9999, ... so CUT falls on a segment boundary.
  local ready=0 burst
  for burst in 1 2 3 4; do
    seq $(( (burst - 1) * N / 4 + 1 )) $(( burst * N / 4 )) \
      | awk '{ printf "%d|{\"n\":%d,\"pad\":\"%080d\"}\n", $1, $1, 0 }' \
      | kcat -P -K '|' -b "$BOOTS" -t $T -X acks=all 2>/dev/null
    for _ in $(seq 60); do
      ready=1
      for n in 1 2 3; do [ "$(archive_end $n)" -ge $(( burst * N / 4 - 1 )) ] || ready=0; done
      [ $ready = 1 ] && break
      sleep 3
    done
  done
  expect "every node archived all four bursts" "$ready" "1"
  expect "segments below $CUT on each node" "$(for n in 1 2 3; do printf '%s ' "$(below_cut $n | wc -l)"; done)" "2 2 2 "
  expect "no search archives to fall back to" "$(find "$DIR"/data/node*/segments/tantivy_indexes/$T -type f 2>/dev/null | wc -l)" "0"
  expect "served after writing" "$(served)" "$N 0 $((N - 1))"
}

echo "=== $LABEL ($CHRONIK_BIN)"
trap cluster_stop EXIT

echo "-- A. a follower missing the start"
setup
node_kill 1
for _ in $(seq 60); do l=$(leader); [ -n "$l" ] && [ "$l" != 1 ] && [ "$l" != -1 ] && break; sleep 2; done
plant 1
expect "node 1's archive now starts above $CUT" "$( [ "$(archive_start 1)" -ge $((CUT - 2000)) ] && echo yes || echo "$(archive_start 1)")" "yes"
node_start 1 || { echo "  FAIL node 1 did not restart"; fail=1; }
back=0
for _ in $(seq 60); do [ "$(leader)" = 1 ] && { back=1; break; }; sleep 3; done
expect "leadership returned to node 1" "$back" "1"
sleep 5
expect "served once node 1 leads" "$(until_served "$N 0 $((N - 1))" 60)" "$N 0 $((N - 1))"
expect "node 1 rebuilt its copy from the leader" "$(logged 1 "rebuilding this copy")" "yes"
cluster_stop

echo "-- B. a leader missing the start"
setup
stop_all
plant 1
start_all
expect "served (from a replica that holds the start)" "$(until_served "$N 0 $((N - 1))" 120)" "$N 0 $((N - 1))"
expect "node 1 handed the leadership on" "$(logged 1 "Handing it the leadership")" "yes"
cluster_stop

echo "-- C. no replica holds the start"
setup
stop_all
plant 1 2 3
first=$(for n in 1 2 3; do archive_start $n; done | sort -n | head -1)
start_all
want="$((N - first)) $first $((N - 1))"
expect "served from the first record that exists" "$(until_served "$want" 120)" "$want"
got=""; for _ in $(seq 24); do got=$(earliest); [ "$got" = "$first" ] && break; sleep 5; done
expect "earliest offset is that record" "$got" "$first"
expect "the leader moved the log start" "$(logged "[123]" "are on no replica")" "yes"
expect "panics" "$(panics)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
