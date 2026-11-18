#!/bin/bash
# Records the seal-time embedding pass failed to embed are embedded later.
#
# Thunderbird ran out of OpenAI credit; every embed failed, was logged as a
# rate limit, and the WAL segment was deleted anyway, so 8 vector topics had no
# index on any node — for good. The vector catch-up now re-reads what an index
# is missing from the node's log and embeds it once the provider answers.
#
# On one node, with a local embedder (crates/chronik-embeddings/examples/
# fake_embedder.rs) that answers 503 while a fail file exists:
#   1. 2,000 records are produced and indexed while the embedder is down: the
#      index holds nothing, and the log says the records were not embedded.
#   2. The embedder comes back: within a few catch-up passes the index holds
#      all 2,000, and a vector search answers.
#   3. Deleting the topic removes its vector index (it used to outlive it).
#
# Usage: vector_catchup.sh <binary> <label>
# Needs: KAFKA_HOME, kcat; builds the fake_embedder example.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/veccatch-$LABEL; LOG=tests/cluster/logs/veccatch-$LABEL
B=localhost:9092; T=vec-catchup; PORT=18731; DIMS=8; FAIL=$D/embedder-down
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
vectors() { curl -s -m 10 localhost:6092/_vector/$T/stats | grep -o '"total_vectors":[0-9]*' | head -1 | cut -d: -f2; }

echo "=== $LABEL ($BIN)"
cargo build --release -q -p chronik-embeddings --example fake_embedder || { echo "FAIL: could not build fake_embedder"; exit 1; }
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
touch "$FAIL"
target/release/examples/fake_embedder $PORT $DIMS --fail-file "$FAIL" > "$LOG-embedder.log" 2>&1 &
epid=$!
CHRONIK_ADVERTISED_ADDR=localhost \
  CHRONIK_EMBEDDING_PROVIDER=external CHRONIK_EMBEDDING_ENDPOINT=http://127.0.0.1:$PORT/embed \
  CHRONIK_EMBEDDING_DIMENSIONS=$DIMS CHRONIK_HOT_VECTOR_ENABLED=false \
  CHRONIK_VECTOR_CATCHUP_DELAY_SECS=20 CHRONIK_VECTOR_CATCHUP_INTERVAL_SECS=15 \
  "$BIN" start --data-dir "$D" > "$LOG-server.log" 2>&1 &
pid=$!
trap 'kill $pid $epid 2>/dev/null; wait $pid $epid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T --add-config vector.enabled=true >/dev/null 2>&1
sleep 2
seq 0 1999 | awk '{ printf "record %d about topic %d and color %s\n", $1, $1 % 17, ($1 % 2 ? "red" : "blue") }' \
  | kcat -P -b $B -t $T -p 0 -X acks=1 2>/dev/null

# Wait for the seal-time pass to try (and fail).
for _ in $(seq 60); do grep -q "records not embedded (provider error)" "$LOG-server.log" && break; sleep 5; done
expect "seal-time embed reported the failure" "$(grep -c 'records not embedded (provider error)' "$LOG-server.log" | awk '{print ($1 > 0)}')" "1"
expect "index while the embedder is down" "$(vectors)" "0"
sleep 20
expect "still empty: catch-up does not count a failure as done" "$(vectors)" "0"

rm -f "$FAIL"
for _ in $(seq 40); do [ "$(vectors)" = 2000 ] && break; sleep 5; done
expect "index after the embedder came back" "$(vectors)" "2000"
hits=$(curl -s -m 30 -X POST localhost:6092/_vector/$T/search -H 'Content-Type: application/json' \
  -d '{"query":"record 42 about topic 8 and color blue","k":5}' | grep -o '"offset":[0-9]*' | wc -l)
expect "vector search hits" "$hits" "5"

$K/kafka-topics.sh --bootstrap-server $B --delete --topic $T >/dev/null 2>&1
for _ in $(seq 20); do [ ! -d "$D/vectors/$T" ] && break; sleep 2; done
expect "vector index removed with the topic" "$( [ -d "$D/vectors/$T" ] && echo present || echo gone)" "gone"
expect "no longer listed" "$(curl -s -m 10 localhost:6092/_vector/topics | grep -c "\"$T\"")" "0"

expect "panics" "$(grep -c panicked "$LOG-server.log")" "0"
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
