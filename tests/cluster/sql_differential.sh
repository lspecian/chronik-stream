#!/usr/bin/env bash
# Differential /_sql check: the same queries over the same data, answered by a
# reference binary and a candidate binary, must agree — on every node.
#
# For engine upgrades (DataFusion, sqlparser) where the risk is not a crash but
# a merge, sort or aggregate that quietly answers differently. The data is
# deterministic and larger than the default row cap (1000), so every merge
# shape is exercised above it: GROUP BY with every aggregate, HAVING, ORDER BY
# by name, ordinal and aggregate, LIMIT/OFFSET, DISTINCT, subqueries, CTEs, and
# projected (auto-inferred) columns next to json_extract over `_value`.
#
# Usage: sql_differential.sh <reference-binary> <candidate-binary>
# Uses the standard 3-node harness (9092-9094, API 6092-6094). Needs kcat, jq.
set -u

REF=${1:?reference binary}; NEW=${2:?candidate binary}
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIR="$ROOT/tests/cluster"
RECORDS="${SQL_DIFF_RECORDS:-5000}"
PARTITIONS=6
TOPIC="sqldiff"
BOOT="localhost:9092,localhost:9093,localhost:9094"
OUT="${SQL_DIFF_OUT:-$(mktemp -d)}"

for b in "$REF" "$NEW"; do [ -x "$b" ] || { echo "SKIP: $b is not executable"; exit 77; }; done
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
command -v jq >/dev/null || { echo "SKIP: jq not installed"; exit 77; }
trap '"$DIR"/stop.sh >/dev/null 2>&1' EXIT

T=$TOPIC
QUERIES=(
  "SELECT COUNT(*) AS c FROM $T"
  "SELECT _partition, COUNT(*) AS n FROM $T GROUP BY _partition ORDER BY _partition"
  "SELECT svc, COUNT(*) AS n, SUM(amt) AS s, MIN(amt) AS mn, MAX(amt) AS mx, AVG(amt) AS a FROM $T GROUP BY svc ORDER BY svc"
  "SELECT lvl, COUNT(*) FROM $T GROUP BY lvl HAVING COUNT(*) > 1000 ORDER BY 2 DESC, 1"
  "SELECT lvl, COUNT(*) AS c FROM $T GROUP BY lvl ORDER BY c DESC, lvl LIMIT 1"
  "SELECT _partition, _offset FROM $T ORDER BY _offset DESC, _partition LIMIT 7"
  "SELECT _partition, _offset FROM $T ORDER BY _partition, _offset LIMIT 5 OFFSET 1234"
  "SELECT id, txt FROM $T ORDER BY id LIMIT 3 OFFSET 4990"
  "SELECT id FROM $T WHERE amt = 7 ORDER BY id DESC LIMIT 4"
  "SELECT COUNT(DISTINCT svc) AS d FROM $T"
  "SELECT svc, lvl, COUNT(*) AS n FROM $T WHERE amt >= 50 GROUP BY svc, lvl ORDER BY svc, lvl"
  "SELECT amt, COUNT(*) AS n FROM $T GROUP BY amt ORDER BY COUNT(*) DESC, amt LIMIT 5"
  "SELECT _partition, MAX(_offset) AS m FROM $T GROUP BY _partition ORDER BY 1"
  "SELECT json_extract_string(_value, '\$.svc') AS s, COUNT(*) AS n FROM $T GROUP BY 1 ORDER BY 1"
  "SELECT COUNT(*) AS c FROM $T WHERE txt LIKE 'm1%'"
  "SELECT COUNT(*) AS c FROM (SELECT svc FROM $T WHERE lvl = 'ERROR') sub"
  "WITH e AS (SELECT * FROM $T WHERE lvl = 'ERROR') SELECT svc, COUNT(*) AS n FROM e GROUP BY svc ORDER BY n DESC, svc"
  "SELECT svc FROM $T GROUP BY svc ORDER BY svc LIMIT 3 OFFSET 2"
  "SELECT id, amt FROM $T WHERE lvl = 'WARN' AND svc IN ('s1', 's3') ORDER BY amt DESC, id LIMIT 6"
  "SELECT UPPER(svc) AS u, COUNT(*) FROM $T GROUP BY UPPER(svc) ORDER BY u"
)

run_one() {  # <binary> <label>
  local bin=$1 label=$2
  echo "== $label: $bin"
  "$DIR"/stop.sh >/dev/null 2>&1
  CHRONIK_BIN=$bin "$DIR"/start.sh >/dev/null 2>&1 || { echo "FAIL: cluster did not start"; return 1; }
  sleep 25
  # Explicit 6-partition topic, then deterministic records spread over it.
  "${KAFKA_BIN:?set KAFKA_BIN}"/kafka-topics.sh --bootstrap-server "$BOOT" --create --topic "$TOPIC" \
    --partitions $PARTITIONS --replication-factor 3 >/dev/null 2>&1
  sleep 3
  local lvls=(INFO WARN ERROR)
  for p in $(seq 0 $((PARTITIONS - 1))); do
    for i in $(seq $p $PARTITIONS $((RECORDS - 1))); do
      printf '{"id":%d,"svc":"s%d","lvl":"%s","amt":%d,"txt":"m%d"}\n' "$i" $((i % 7)) "${lvls[$((i % 3))]}" $((i % 100)) "$i"
    done | timeout 60 kcat -P -b "$BOOT" -t "$TOPIC" -p "$p" 2>/dev/null
  done
  # Every node sees all of it, then give the indexer time to move part of it
  # to Parquet, so the answers come from the hot and cold tiers together.
  local ok=0
  for _ in $(seq 1 60); do
    ok=1
    for port in 6092 6093 6094; do
      c=$(curl -s -m 10 -X POST "http://localhost:$port/_sql" -H 'Content-Type: application/json' \
            -d "{\"query\":\"SELECT COUNT(*) AS c FROM $TOPIC\"}" | jq -r '.rows[0].c // 0' 2>/dev/null)
      [ "${c:-0}" = "$RECORDS" ] || ok=0
    done
    [ $ok = 1 ] && break
    sleep 2
  done
  [ $ok = 1 ] || echo "   note: not every node reached $RECORDS"
  sleep 90
  local n=0
  for q in "${QUERIES[@]}"; do
    n=$((n + 1))
    for port in 6092 6093 6094; do
      curl -s -m 30 -X POST "http://localhost:$port/_sql" -H 'Content-Type: application/json' \
        -d "$(jq -cn --arg q "$q" '{query: $q}')" \
        | jq -S '{columns, rows, truncated, partial, error}' > "$OUT/$label.q$n.$port.json" 2>/dev/null \
        || echo "(not JSON)" > "$OUT/$label.q$n.$port.json"
    done
  done
  "$DIR"/stop.sh >/dev/null 2>&1
}

run_one "$REF" ref || exit 1
run_one "$NEW" new || exit 1

FAIL=0
n=0
for q in "${QUERIES[@]}"; do
  n=$((n + 1))
  verdict=ok
  for port in 6092 6093 6094; do
    # Each node against the same node: a query whose answer legitimately
    # depends on which node merges it (a tie under LIMIT) still has to agree.
    cmp -s "$OUT/ref.q$n.$port.json" "$OUT/new.q$n.$port.json" || { verdict="DIFFERS (node $port)"; FAIL=1; }
    [ "$verdict" = ok ] && ! cmp -s "$OUT/ref.q$n.6092.json" "$OUT/ref.q$n.$port.json" && verdict="ok (ref nodes disagree)"
  done
  err=$(jq -r ".error // empty" "$OUT/new.q$n.6092.json" 2>/dev/null)
  [ -n "$err" ] && [ "${verdict#ok}" != "$verdict" ] && verdict="ok (both refuse)"
  rows=$(jq '.rows | length' "$OUT/new.q$n.6092.json" 2>/dev/null)
  printf '%-22s rows=%-3s %s\n' "$verdict" "${rows:-?}" "$q"
done
echo "outputs: $OUT"
[ $FAIL = 0 ] && echo "RESULT: PASS" || echo "RESULT: FAIL"
exit $FAIL
