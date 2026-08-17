#!/usr/bin/env bash
# Issue #22: `/_sql` must return the whole topic, and the same answer, from every
# node — at RF=node_count.
#
# The bug: the fan-out was skipped whenever this node was a *replica* of every
# partition, which at RF=3 on 3 nodes is always true. Each node then answered
# from what it could see locally, and what a node can see is a subset that varies
# by node — so `COUNT(*)` was partial and depended on which node you asked.
#
# The opposite mistake is just as wrong and this test catches it too: if every
# node served every partition it replicates and the results were merged, each row
# would be counted three times.
#
# So there are exactly two ways to fail, and the test asserts against both:
#
#   too few   → the fan-out is being skipped, or a partition is unowned
#   too many  → partitions are being served by more than one node
#
# Correct is exactly N on every node.
#
# Uses the standard 3-node harness (9092-9094, API 6092-6094), which already
# enables CHRONIK_DEFAULT_COLUMNAR. Requires a release build and kcat.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIR="$ROOT/tests/cluster"
BIN="$ROOT/target/release/chronik-server"
RECORDS="${SQL_FANOUT_RECORDS:-300}"
PARTITIONS="${SQL_FANOUT_PARTITIONS:-6}"
TOPIC="sqlfan_$$"
BOOT="localhost:9092,localhost:9093,localhost:9094"
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

[ -x "$BIN" ] || { say "SKIP: build first (cargo build --release --bin chronik-server)"; exit 0; }
command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }
command -v curl >/dev/null || { say "SKIP: curl not installed"; exit 0; }

trap '"$DIR"/stop.sh >/dev/null 2>&1' EXIT

say "== SQL fan-out correctness at RF=3: $TOPIC =="
"$DIR"/stop.sh >/dev/null 2>&1
"$DIR"/start.sh >/dev/null 2>&1 || { fail "cluster did not start"; exit 1; }
sleep 25

# Create the topic, then ASK how many partitions it has rather than assuming.
#
# Producing to a partition the topic does not have fails silently under
# `2>/dev/null`. Assuming 6 against an auto-create default of 3 cost half the
# records and looked exactly like the bug under test: a consistent 150 of 300.
timeout 30 kcat -P -b "$BOOT" -t "$TOPIC" -p 0 <<< '{"p":0,"n":0}' 2>/dev/null
sleep 3
PARTITIONS=$(timeout 15 kcat -L -b "$BOOT" 2>/dev/null \
  | awk -v t="\"$TOPIC\"" '$0 ~ "topic " t {found=1} found && /partition [0-9]+, leader/ {c++} END {print c+0}')
[ "${PARTITIONS:-0}" -ge 2 ] || { fail "topic has ${PARTITIONS:-0} partition(s); this test needs at least 2"; exit 1; }

PER=$((RECORDS / PARTITIONS))
RECORDS=$((PER * PARTITIONS + 1))  # +1 for the record that created the topic

# JSON payloads are produced so the rows have something to count beyond metadata.
say "-- producing $((PER * PARTITIONS)) records across $PARTITIONS partitions (+1 already sent)"
for p in $(seq 0 $((PARTITIONS - 1))); do
  seq 1 "$PER" \
    | sed "s/^/{\"p\":$p,\"n\":/;s/$/}/" \
    | timeout 60 kcat -P -b "$BOOT" -t "$TOPIC" -p "$p" \
        -X request.required.acks=all \
        -X socket.timeout.ms=5000 2>/dev/null
done

# The indexer must have sealed and written Parquet, and the hot buffer must have
# picked up whatever is not yet sealed. Both are on timers; this waits for the
# count to stop moving rather than assuming a duration.
say "-- waiting for the row count to settle"
settled=0
last=-1
for _ in $(seq 1 60); do
  n=$(curl -s -m 10 -X POST "http://localhost:6092/_sql" \
        -H 'Content-Type: application/json' \
        -d "{\"query\":\"SELECT COUNT(*) AS c FROM $TOPIC\"}" 2>/dev/null \
      | grep -oE '"c":[0-9]+' | head -1 | tr -cd '0-9')
  n="${n:-0}"
  if [ "$n" = "$last" ] && [ "$n" != "0" ]; then settled=1; break; fi
  last="$n"
  sleep 2
done
[ "$settled" = 1 ] || say "   note: count still moving after 120s (last=$last)"

# Every node must give the same answer, and it must be the right one.
say "-- asking every node for COUNT(*)"
for port in 6092 6093 6094; do
  c=$(curl -s -m 15 -X POST "http://localhost:$port/_sql" \
        -H 'Content-Type: application/json' \
        -d "{\"query\":\"SELECT COUNT(*) AS c FROM $TOPIC\"}" 2>/dev/null \
      | grep -oE '"c":[0-9]+' | head -1 | tr -cd '0-9')
  c="${c:-0}"

  if [ "$c" -eq "$RECORDS" ]; then
    verdict="ok"
  elif [ "$c" -lt "$RECORDS" ]; then
    verdict="TOO FEW — fan-out skipped, or a partition is unowned"
  else
    verdict="TOO MANY — a partition is served by more than one node"
  fi

  say "   node on API $port: COUNT(*) = $c / $RECORDS  ($verdict)"
  [ "$c" -eq "$RECORDS" ] || fail "node on $port returned $c of $RECORDS"
done

# A row-level query must also be complete: COUNT can be right while the rows a
# client actually reads are not.
#
# Deliberately a plain projection, de-duplicated here rather than with SELECT
# DISTINCT: DISTINCT is refused across a fan-out on purpose, because each node
# de-duplicates only its own partitions and a value held by two nodes would
# survive twice.
say "-- checking every partition is visible to a row query"
parts=$(curl -s -m 20 -X POST "http://localhost:6093/_sql" \
          -H 'Content-Type: application/json' \
          -d "{\"query\":\"SELECT _partition FROM $TOPIC\",\"limit\":100000}" 2>/dev/null \
        | grep -oE '"_partition":[0-9]+' | sort -u | wc -l)
say "   distinct partitions returned: ${parts:-0} / $PARTITIONS"
[ "${parts:-0}" -eq "$PARTITIONS" ] || fail "row query saw ${parts:-0} of $PARTITIONS partitions"

# And an unmergeable query must be refused rather than answered wrongly.
say "-- checking an unmergeable query is refused, not guessed"
avg=$(curl -s -m 15 -X POST "http://localhost:6092/_sql" \
        -H 'Content-Type: application/json' \
        -d "{\"query\":\"SELECT AVG(_offset) AS a FROM $TOPIC\"}" 2>/dev/null)
case "$avg" in
  *DistributedQueryUnsupported*) say "   AVG refused, as it must be" ;;
  *) fail "AVG returned an answer across a fan-out: $avg" ;;
esac

if [ "$FAIL" = 0 ]; then
  say ""
  say "== PASS: every node returns the whole topic exactly once at RF=3 =="
else
  say ""
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
