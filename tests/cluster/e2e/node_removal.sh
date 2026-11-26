#!/bin/bash
# Zero-downtime node addition and removal, as CLAUDE.md documents it.
#
#   1. Three nodes; `cluster add-node 4` (CLI) and node 4 starts with
#      CHRONIK_CLUSTER_JOIN=true, joining instead of founding a cluster.
#   2. A topic created now is placed over four brokers, so node 4 holds
#      replicas; 20,000 records are produced with acks=all.
#   3. `cluster remove-node 4` (CLI, graceful): replicas move off node 4 before
#      the membership change. Afterwards every partition has three replicas,
#      none on node 4, all in sync, and the cluster has three voters.
#   4. Node 4 is killed: every record is still there (end offsets and a full
#      read through the remaining brokers).
#   5. The admin API refuses a graceful removal that would leave two voters.
#
# Replaces tests/test_node_removal.sh, which pointed at configs that never
# existed, stopped at its first passing check (set -e and ((x++)) from 0),
# and only warned when node 4 was still a member after its removal.
#
# Usage: node_removal.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, curl.
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
KEY="node-removal-test-key"
export EXTRA_ENV="CHRONIK_ADMIN_API_KEY=$KEY"
source "$(dirname "$0")/../catalog/lib.sh"
T=nr_topic; N=20000; P=8
OUT="$DIR/data/nr-$LABEL"
N4_LOG="$DIR/logs/node4.log"
N4_PID=""

stop_node4() { [ -n "$N4_PID" ] && kill -9 "$N4_PID" 2>/dev/null; N4_PID=""; }
trap stop_node4 EXIT

status() { # the cluster status as one node reports it (JSON)
  curl -s "http://localhost:$((6091 + $1))/admin/status" -H "X-API-Key: $KEY"
}
voters() { status "$1" | jq -r '[.nodes[].node_id] | sort | map(tostring) | join(",")' 2>/dev/null; }
# Partitions of the topic that still name node 4, or are not fully in sync.
bad_partitions() {
  status "$1" | jq -r --arg t $T '[.partitions[] | select(.topic == $t)
    | select((.replicas | index(4)) or (.replicas | length) != 3 or (.isr | length) != 3)] | length' 2>/dev/null
}
on_node4() { status "$1" | jq -r --arg t $T '[.partitions[] | select(.topic == $t) | select(.replicas | index(4))] | length' 2>/dev/null; }
end_offsets() { # sum of the topic's end offsets, through one broker
  "$K/kafka-get-offsets.sh" --bootstrap-server "localhost:$1" --topic $T --time -1 2>/dev/null \
    | awk -F: '{ s += $3 } END { print s+0 }'
}
wait_eq() { # seconds, description, wanted, command...: until the command prints the wanted value
  local secs=$1 what=$2 want=$3 got; shift 3
  for _ in $(seq 1 "$secs"); do got=$("$@"); [ "$got" = "$want" ] && return 0; sleep 1; done
  fail "$what: got '$got', want '$want' (after ${secs}s)"; return 1
}

echo "=== $LABEL ($CHRONIK_BIN)"
rm -rf "$OUT" "$DIR/data/node4"; mkdir -p "$OUT"; : > "$N4_LOG"
cluster_start || { fail "cluster did not start"; finish "node removal"; }

# --- 1. add node 4 -----------------------------------------------------------
sed -e 's/^node_id = 1$/node_id = 4/' \
    -e 's#^data_dir = "tests/cluster/data/node1".*#data_dir = "tests/cluster/data/node4"#' \
    -e 's/0\.0\.0\.0:9092/0.0.0.0:9095/; s/0\.0\.0\.0:9291/0.0.0.0:9294/; s/0\.0\.0\.0:5001/0.0.0.0:5004/' \
    -e '/^\[advertise\]/,/^$/{s/localhost:9092/localhost:9095/; s/localhost:9291/localhost:9294/; s/localhost:5001/localhost:5004/}' \
    "$DIR/node1.toml" > "$OUT/node4.toml"
printf '\n[[peers]]\nid = 4\nkafka = "localhost:9095"\nwal = "localhost:9294"\nraft = "localhost:5004"\n' >> "$OUT/node4.toml"

out=$(CHRONIK_ADMIN_API_KEY=$KEY "$CHRONIK_BIN" cluster add-node 4 --kafka localhost:9095 --wal localhost:9294 \
  --raft localhost:5004 --config "$DIR/node1.toml" 2>&1); rc=$?
echo "$out" | grep -E '✓|✗|rror' | sed 's/^/  add-node: /' | head -4
[ $rc = 0 ] && echo "$out" | grep -q '✓' || fail "cluster add-node 4 failed (rc=$rc)"

env $(node_env) CHRONIK_ADMIN_API_KEY=$KEY CHRONIK_CLUSTER_JOIN=true RUST_LOG=info \
  "$CHRONIK_BIN" start --config "$OUT/node4.toml" >> "$N4_LOG" 2>&1 &
N4_PID=$!; disown "$N4_PID"  # killed by this script; no job notice
wait_port 9095 || fail "node 4 did not start"
wait_eq 60 "voters after add-node" 1,2,3,4 voters 1
echo "  voters after add: $(voters 1)"

# --- 2. data on four brokers ---------------------------------------------------
"$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic $T --partitions $P \
  --replication-factor 3 >/dev/null 2>&1 || fail "topic create failed"
held=$(on_node4 1); echo "  partitions with a replica on node 4: ${held:-?} of $P"
[ "${held:-0}" -gt 0 ] || fail "node 4 holds no replica, so its removal would test nothing"
seq 1 $N | kcat -P -b localhost:9092,localhost:9093,localhost:9094 -t $T -X acks=all -X linger.ms=5 2>"$OUT/produce.err" \
  || fail "produce failed: $(head -2 "$OUT/produce.err")"
before=$(end_offsets 9092); echo "  records before removal: $before"
[ "$before" = $N ] || fail "end offsets sum to $before, want $N"

# --- 3. graceful removal through the CLI ---------------------------------------
out=$(CHRONIK_ADMIN_API_KEY=$KEY "$CHRONIK_BIN" cluster remove-node 4 --config "$DIR/node1.toml" 2>&1); rc=$?
echo "$out" | grep -E '✓|✗|leader|rror' | sed 's/^/  remove-node: /' | head -4
[ $rc = 0 ] && echo "$out" | grep -q '✓' || fail "cluster remove-node 4 failed (rc=$rc)"
wait_eq 90 "voters after remove-node" 1,2,3 voters 1
for n in 1 2 3; do
  wait_eq 60 "node $n: partitions still on node 4 or not fully in sync" 0 bad_partitions $n
done
echo "  voters after removal: $(voters 1); partitions on node 4 or under-replicated: $(bad_partitions 1)"

# --- 4. node 4 gone, every record still there ----------------------------------
stop_node4
for port in 9092 9093 9094; do
  e=$(end_offsets $port); [ "$e" = $N ] || fail "through $port the end offsets sum to $e, want $N"
done
read_back=$(timeout 120 kcat -C -b localhost:9092,localhost:9093,localhost:9094 -t $T -o beginning -e -q 2>/dev/null | wc -l)
echo "  records read back after node 4 stopped: $read_back"
[ "$read_back" = $N ] || fail "read back $read_back records, want $N"

# --- 5. the API refuses to go below three voters -------------------------------
r=$(curl -s -X POST http://localhost:6092/admin/remove-node -H "X-API-Key: $KEY" \
  -H 'Content-Type: application/json' -d '{"node_id": 3, "force": false}')
echo "  remove-node 3 (would leave 2): $r"
echo "$r" | grep -q '"success":false' && echo "$r" | grep -q 'at least 3' \
  || fail "a removal leaving two voters was not refused"
[ "$(voters 1)" = 1,2,3 ] || fail "voters changed after a refused removal: $(voters 1)"

grep -q panicked "$N4_LOG" && fail "node 4 panicked: $(grep -m1 panicked "$N4_LOG")"
finish "node removal"
