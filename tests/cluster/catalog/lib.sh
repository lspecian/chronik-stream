# Shared helpers for the cluster-catalog acceptance suite (T1–T12).
# Sourced by the t*.sh scripts next to it. Uses the local 3-node cluster
# (tests/cluster/node{1,2,3}.toml) through tests/cluster/start.sh and stop.sh;
# single nodes are stopped and restarted with exactly start.sh's command line.
#
# Env:
#   CHRONIK_BIN  binary under test (default: target/release/chronik-server)
#   KAFKA_HOME   a Kafka distribution (bin/ and libs/) for the stock tools

set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DIR="$ROOT/tests/cluster"
BIN="${CHRONIK_BIN:-$ROOT/target/release/chronik-server}"
KAFKA_HOME="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}"
K="$KAFKA_HOME/bin"
FAULTS="$DIR/data/faults"
TOOL_SRC="$ROOT/tests/cluster/catalog/CatalogTool.java"
BOOTS="localhost:9092,localhost:9093,localhost:9094"
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }
pass() { say "ok: $*"; }
port_of() { echo $((9091 + $1)); }

tool() { java -cp "$KAFKA_HOME/libs/*" "$TOOL_SRC" "$@" 2>/dev/null; }

# Same environment start.sh gives every node, plus the fault-injection file.
node_env() {
  echo "CHRONIK_FAULT_FILE=$FAULTS/node%n CHRONIK_EMBEDDING_PROVIDER=openai CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_DEFAULT_COLUMNAR=true"
}

cluster_start() {
  "$DIR/stop.sh" >/dev/null 2>&1
  rm -rf "$FAULTS"
  env $(node_env) ${EXTRA_ENV:-} CHRONIK_BIN="$BIN" "$DIR/start.sh" >/dev/null 2>&1
  mkdir -p "$FAULTS"
  for n in 1 2 3; do wait_port "$(port_of $n)" || { say "node $n did not start"; return 1; }; done
  wait_leader 30 >/dev/null || { say "no catalog leader"; return 1; }
}

cluster_stop() { "$DIR/stop.sh" >/dev/null 2>&1; }

wait_port() { for _ in $(seq 1 120); do kcat -L -b "localhost:$1" -t __none >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }

node_pid() { cat "$DIR/data/node$1.pid" 2>/dev/null; }

node_kill() { local p; p=$(node_pid "$1"); [ -n "$p" ] && kill -9 "$p" 2>/dev/null; sleep 0.5; }

node_start() {
  env $(node_env) ${EXTRA_ENV:-} RUST_LOG="${RUST_LOG:-info,chronik_server::integrated_server::server=warn,chronik_wal::group_commit=warn,tantivy=warn}" \
    "$BIN" start --config "$DIR/node$1.toml" >> "$DIR/logs/node$1.log" 2>&1 &
  echo $! > "$DIR/data/node$1.pid"
  wait_port "$(port_of "$1")"
}

# Faults for one node (see NetFaults): fault 1 "drop 2" "drop 3"; heal 1.
fault() { local n=$1; shift; mkdir -p "$FAULTS"; printf '%s\n' "$@" > "$FAULTS/node$n"; sleep 0.5; }
heal()  { rm -f "$FAULTS/node$1"; sleep 0.5; }

# The node that most recently logged becoming catalog leader.
current_leader() {
  for n in 1 2 3; do
    t=$(grep 'became leader at term' "$DIR/logs/node$n.log" 2>/dev/null | tail -1 | grep -o 'term: [0-9]*' | grep -o '[0-9]*')
    [ -n "$t" ] && echo "$t $n"
  done | sort -n | tail -1 | awk '{print $2}'
}

wait_leader() { for _ in $(seq 1 $(( $1 * 4 ))); do l=$(current_leader); [ -n "$l" ] && { echo "$l"; return 0; }; sleep 0.25; done; return 1; }

# Topics with a prefix as seen by one broker: "name partitions" sorted.
topics_on() { "$K/kafka-topics.sh" --bootstrap-server "localhost:$(port_of "$1")" --describe 2>/dev/null \
  | awk -v p="$2" '/PartitionCount/ && $2 ~ "^"p {for(i=1;i<=NF;i++) if($i=="PartitionCount:") print $2, $(i+1)}' | sort; }

panics() { grep -l 'panicked' "$DIR"/logs/node*.log 2>/dev/null | wc -l; }

finish() {
  [ "$(panics)" = 0 ] || fail "a node panicked: $(grep -h -m1 panicked "$DIR"/logs/node*.log)"
  cluster_stop
  if [ "$FAIL" = 0 ]; then say "== PASS: $1 =="; else say "== FAIL: $1 =="; fi
  exit "$FAIL"
}
