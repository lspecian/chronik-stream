#!/bin/bash
# The node-local metadata WAL is compacted, and a node boots from the
# checkpoint to the same state.
#
# Before: the metadata WAL was never truncated; every event since a node's
# first boot stayed on disk and was replayed at every boot.
#
#   1. Three nodes with a tiny compaction threshold and small data-WAL
#      segments, so records are archived and node-local metadata (segment
#      records, watermarks) keeps changing.
#   2. 30,000 records in batches; every node compacts its metadata WAL and its
#      segments are deleted.
#   3. 5,000 more records after the checkpoint, then every node restarts: it
#      loads the checkpoint, replays only later records, and every record
#      (archived or not) reads back from offset 0, through every node.
#
# Usage: metadata_compaction.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
source "$(dirname "$0")/../catalog/lib.sh"
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
T=mc_topic
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
produce() { # first count
  seq "$1" $(($1 + $2 - 1)) | awk '{ printf "%d|{\"n\":%d,\"pad\":\"%0100d\"}\n", $1, $1, 0 }' \
    | kcat -P -K '|' -b "$BOOTS" -t $T -X acks=all -X linger.ms=5 -X batch.num.messages=200 2>/dev/null
}
distinct_via() { # port -> distinct records read from offset 0
  kcat -C -b "localhost:$1" -t $T -o beginning -e -q -f '%s\n' 2>/dev/null | sort -u | wc -l
}
segments_bytes() { # node -> bytes of metadata WAL segments
  du -sb "$DIR/data/node$1/metadata_wal/__chronik_metadata" 2>/dev/null | awk '{ print $1 + 0 }'
}
compactions() { grep -c 'Compacted the metadata WAL' "$DIR/logs/node$1.log" 2>/dev/null; }

echo "=== $LABEL ($CHRONIK_BIN)"
EXTRA_ENV="CHRONIK_METADATA_WAL_COMPACT_BYTES=4KB CHRONIK_WAL_ROTATION_SIZE=256KB" cluster_start \
  || { echo "FAIL: cluster did not start"; exit 1; }
trap cluster_stop EXIT
"$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic $T --partitions 3 --replication-factor 3 >/dev/null 2>&1
sleep 3
for i in $(seq 0 5); do produce $((i * 5000 + 1)) 5000; sleep 5; done
total=30000

compacted=0
for _ in $(seq 150); do
  c=0; for n in 1 2 3; do [ "$(compactions $n)" -ge 1 ] && c=$((c + 1)); done
  [ $c = 3 ] && { compacted=1; break; }
  sleep 2
done
expect "every node compacted its metadata WAL" "$compacted" "1"
for n in 1 2 3; do
  expect "node $n: checkpoint written" "$( [ -s "$DIR/data/node$n/metadata_wal/checkpoint.img" ] && echo yes || echo no)" "yes"
  echo "  node $n: metadata WAL segments now $(segments_bytes $n) bytes; $(grep -h 'Compacted the metadata WAL' "$DIR/logs/node$n.log" | tail -1 | sed 's/.*Compacted/Compacted/')"
done

produce $((total + 1)) 5000
total=$((total + 5000))
sleep 5
expect "records before the restart" "$(distinct_via 9092)" "$total"

echo "-- every node restarts"
for n in 1 2 3; do
  kill "$(node_pid $n)" 2>/dev/null
  for _ in $(seq 60); do kill -0 "$(node_pid $n)" 2>/dev/null || break; sleep 1; done
  node_start $n || { echo "  FAIL node $n did not restart"; fail=1; }
done
sleep 15
for n in 1 2 3; do
  expect "node $n: booted from the checkpoint" \
    "$(grep -c 'Loaded the metadata checkpoint' "$DIR/logs/node$n.log")" "1"
done
for port in 9092 9093 9094; do
  expect "distinct records via $port after the restart" "$(distinct_via $port)" "$total"
done
produce $((total + 1)) 1000
total=$((total + 1000))
expect "records written after the restart" "$(distinct_via 9094)" "$total"
expect "panics" "$(panics)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
