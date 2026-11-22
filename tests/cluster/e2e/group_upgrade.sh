#!/bin/bash
# Consumer-group offsets survive the rolling upgrade that moves them into the
# catalog.
#
# Before the upgrade each broker keeps offsets in its own metadata store. The
# catalog takes them over only once every voter runs a build that understands
# catalog offsets; until then mixed clusters keep the old behaviour.
#
#   1. Three nodes on the OLD binary; a group consumes 6,000 records and
#      commits.
#   2. Nodes move to the NEW binary one at a time. While any node is old the
#      catalog does not enable group offsets (each
#      broker coordinates its own groups, as before the upgrade).
#   3. After the last node: the catalog enables group offsets, every broker
#      reports the offsets committed before the upgrade, and a member resumes
#      with only the 1,000 records produced afterwards.
#
# Usage: group_upgrade.sh <old-binary> <new-binary> <label>
# Needs: KAFKA_BIN (or KAFKA_HOME), kcat.
set -u
OLD=${1:?old binary}; NEW=${2:?new binary}; LABEL=${3:?label}
KAFKA_BIN=${KAFKA_BIN:-${KAFKA_HOME:?set KAFKA_HOME or KAFKA_BIN}/bin}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
DIR=tests/cluster
B=localhost:9092,localhost:9093,localhost:9094
T=gu_topic; G=gu_group; N=6000
OUT="$DIR/data/gu-$LABEL"
ROLLOUT='catalog consumer-group offsets' # either rollout step
ENABLED='catalog consumer-group offsets enabled'
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
for b in "$OLD" "$NEW"; do [ -x "$b" ] || { echo "SKIP: $b is not executable"; exit 77; }; done
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
trap 'kill $m 2>/dev/null; "$DIR"/stop.sh >/dev/null 2>&1' EXIT
m=

# Same command line and environment as tests/cluster/restart.sh, one node.
start_node() { # <n> <binary>
  OPENAI_API_KEY="${OPENAI_API_KEY:-}" CHRONIK_EMBEDDING_PROVIDER=openai CHRONIK_DEFAULT_SEARCHABLE=true \
  CHRONIK_DEFAULT_COLUMNAR=true RUST_LOG="${RUST_LOG:-info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn}" \
    "$2" start --config "$DIR/node$1.toml" >> "$DIR/logs/node$1.log" 2>&1 &
  echo $! > "$DIR/data/node$1.pid"
}
stop_node() { # <n>
  local pid; pid=$(cat "$DIR/data/node$1.pid")
  kill "$pid" 2>/dev/null
  for _ in $(seq 60); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done
  kill -9 "$pid" 2>/dev/null
}
wait_port() { # <port>
  for _ in $(seq 120); do kcat -L -b localhost:$1 -m 3 >/dev/null 2>&1 && return 0; sleep 1; done
  return 1
}
produce() { # <first> <count>
  seq "$1" $(($1 + $2 - 1)) | awk '{ printf "%d|{\"n\":%d}\n", $1, $1 }' \
    | kcat -P -K '|' -b $B -t $T -X acks=all -X message.timeout.ms=60000 2>/dev/null
}
committed_via() { # port -> total committed offset of the group on the topic
  "$KAFKA_BIN"/kafka-consumer-groups.sh --bootstrap-server "localhost:$1" --describe --group $G 2>&1 \
    | tee -a "$OUT/describe-$1" \
    | awk -v t=$T '$2 == t && $4 ~ /^[0-9]+$/ { s += $4 } END { print s+0 }'
}
consume() { # <port> <outfile> <expected>: one member reads until it has <expected> records, commits, leaves
  kcat -u -C -b "localhost:$1" -G $G $T -X auto.offset.reset=earliest -X enable.auto.commit=true \
    -X auto.commit.interval.ms=300 -q -f '%s\n' > "$2" 2>/dev/null &
  m=$!
  for _ in $(seq 90); do [ "$(wc -l < "$2")" -ge "$3" ] && break; sleep 1; done
  sleep 3
  kill -INT $m 2>/dev/null; wait $m 2>/dev/null; m=
}
logged() { grep -l "$1" "$DIR"/logs/node*.log 2>/dev/null | wc -l; }

echo "=== $LABEL: $OLD -> $NEW"
"$DIR"/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$OLD "$DIR"/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
for port in 9092 9093 9094; do wait_port $port || { echo "FAIL: node on $port not up"; exit 1; }; done
rm -rf "$OUT"; mkdir -p "$OUT"
sleep 10
"$KAFKA_BIN"/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 6 --replication-factor 3 >/dev/null 2>&1
sleep 3
produce 1 $N
consume 9092 "$OUT/before" $N
expect "all old: records consumed" "$(wc -l < "$OUT/before")" "$N"
# Not read back here: v2.18.3 answers the all-topics OffsetFetch that
# kafka-consumer-groups --describe sends with nothing (fixed since).

for n in 1 2 3; do
  echo "-- node $n -> new"
  stop_node $n
  start_node $n "$NEW"
  wait_port $((9091 + n)) || { echo "  FAIL node $n did not come back on the new binary"; fail=1; break; }
  if [ $n -lt 3 ]; then
    sleep 15 # several rollout rounds (2 s each) with an old voter present
    expect "step $n: catalog group offsets still off" "$(logged "$ROLLOUT")" "0"
  fi
done

on=0
for _ in $(seq 60); do [ "$(logged "$ENABLED")" -ge 1 ] && { on=1; break; }; sleep 1; done
expect "all new: catalog group offsets enabled" "$on" "1"
sleep 5 # every node records its merge of local offsets
expect "all new: committed offsets via node 1" "$(committed_via 9092)" "$N"
expect "all new: committed offsets via node 2" "$(committed_via 9093)" "$N"
expect "all new: committed offsets via node 3" "$(committed_via 9094)" "$N"

produce $((N + 1)) 1000
consume 9094 "$OUT/after" 1000
expect "all new: resumed at the committed offsets (only the new records)" "$(wc -l < "$OUT/after")" "1000"
expect "all new: records from before the upgrade read again" \
  "$(grep -oE '"n":[0-9]+' "$OUT/after" | cut -d: -f2 | awk -v n=$N '$1 <= n { bad++ } END { print bad+0 }')" "0"
expect "panics" "$(cat "$DIR"/logs/node*.log | grep -c panicked)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
