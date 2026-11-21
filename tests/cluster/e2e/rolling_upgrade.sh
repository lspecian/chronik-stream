#!/bin/bash
# A rolling upgrade between two builds keeps one working cluster.
#
# For changes to inter-node traffic (the catalog's gRPC/Raft transport, the
# replica fetch path): the 3-node cluster starts on the OLD binary and nodes
# are replaced by the NEW one, one at a time. In every mixed state:
#   - a topic created through an old node and one created through a new node
#     both appear on every node (catalog writes forwarded across versions);
#   - acks=all produce succeeds and every partition returns to a full ISR
#     (replica fetches across versions);
# and at the end every record produced at any stage is readable.
#
# Usage: rolling_upgrade.sh <old-binary> <new-binary> <label>
# Needs: KAFKA_BIN (or KAFKA_HOME), kcat.
set -u
OLD=${1:?old binary}; NEW=${2:?new binary}; LABEL=${3:?label}
KAFKA_BIN=${KAFKA_BIN:-${KAFKA_HOME:?set KAFKA_HOME or KAFKA_BIN}/bin}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
DIR=tests/cluster
B=localhost:9092,localhost:9093,localhost:9094
T=roll_main
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
for b in "$OLD" "$NEW"; do [ -x "$b" ] || { echo "SKIP: $b is not executable"; exit 77; }; done
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
trap '"$DIR"/stop.sh >/dev/null 2>&1' EXIT

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
produce() { # <topic> <first> <count>
  seq "$2" $(($2 + $3 - 1)) | awk '{ printf "{\"n\":%d}\n", $1 }' \
    | kcat -P -b $B -t "$1" -X acks=all -X message.timeout.ms=60000 2>/dev/null
}
ends() { # <topic>
  "$KAFKA_BIN"/kafka-get-offsets.sh --bootstrap-server $B --topic "$1" 2>/dev/null | awk -F: '{s+=$3} END {print s+0}'
}
full_isr() { # <topic>: partitions whose ISR has fewer than 3 replicas, after waiting
  local short
  for _ in $(seq 60); do
    short=$("$KAFKA_BIN"/kafka-topics.sh --bootstrap-server $B --describe --topic "$1" 2>/dev/null \
      | grep -o 'Isr: [0-9,]*' | awk -F'[ ,]' '{ if (NF - 1 < 3) n++ } END { print n+0 }')
    [ "$short" = 0 ] && break
    sleep 2
  done
  echo "$short"
}
topic_on_every_node() { # <topic>: nodes that list it
  local n=0
  for port in 9092 9093 9094; do
    kcat -L -b localhost:$port -t "$1" -m 5 2>/dev/null | grep -q "topic \"$1\" with" && n=$((n + 1))
  done
  echo $n
}

echo "=== $LABEL: $OLD -> $NEW"
"$DIR"/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$OLD "$DIR"/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
for port in 9092 9093 9094; do wait_port $port || { echo "FAIL: node on $port not up"; exit 1; }; done
sleep 10
"$KAFKA_BIN"/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 6 --replication-factor 3 >/dev/null 2>&1
sleep 3
produce $T 0 6000
total=6000
expect "all old: records" "$(ends $T)" "$total"

for n in 1 2 3; do
  echo "-- node $n -> new"
  stop_node $n
  start_node $n "$NEW"
  wait_port $((9091 + n)) || { echo "  FAIL node $n did not come back on the new binary"; fail=1; break; }
  sleep 10
  new_port=$((9091 + n)); old_port=$((9091 + (n % 3) + 1))
  [ $n = 3 ] && old_port=9092 # all new by now; any node will do
  "$KAFKA_BIN"/kafka-topics.sh --bootstrap-server localhost:$new_port --create --topic roll_via_new_$n \
    --partitions 3 --replication-factor 3 >/dev/null 2>&1
  "$KAFKA_BIN"/kafka-topics.sh --bootstrap-server localhost:$old_port --create --topic roll_via_old_$n \
    --partitions 3 --replication-factor 3 >/dev/null 2>&1
  sleep 3
  expect "step $n: topic created via a new node, on every node" "$(topic_on_every_node roll_via_new_$n)" "3"
  expect "step $n: topic created via another node, on every node" "$(topic_on_every_node roll_via_old_$n)" "3"
  produce $T $total 1000
  total=$((total + 1000))
  expect "step $n: records (acks=all)" "$(ends $T)" "$total"
  expect "step $n: partitions short of a full ISR" "$(full_isr $T)" "0"
done

got=$(kcat -C -b $B -t $T -o beginning -e -q -f '%s\n' 2>/dev/null | sort -u | wc -l)
expect "all new: distinct records readable" "$got" "$total"
expect "panics" "$(cat "$DIR"/logs/node*.log | grep -c panicked)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
