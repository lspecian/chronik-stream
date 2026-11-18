#!/bin/bash
# Records the old Parquet flush dropped reach `{topic}_cold` after an upgrade.
#
# Before, the flush skipped every offset at or below the highest one already
# in Parquet, and sealed WAL segments were indexed in no particular order: on
# a backlog the first high segment written made every lower one look done and
# their records never reached Parquet (10M produced, 1.06M queryable). The WAL
# segments are deleted after indexing, so a fixed flush alone cannot bring
# them back; the columnar catch-up re-reads them from the node's archives.
#
# One node. LEGACY_BIN (v2.17.0, old flush) indexes a 3M-record backlog of a
# columnar topic (several sealed segments in one pass; at 1M it often sees
# two, in order); its cold table must come up short (else the bug did not
# reproduce and the test cannot judge: exit 77). The binary under test then
# starts on the same data directory, catch-up delay shortened. Every record
# must reach the cold table exactly once (COUNT = COUNT DISTINCT = N), and a
# fresh 3M backlog on the new binary must reach it in full.
#
# Usage: columnar_catchup.sh <binary> <label>
# Needs: KAFKA_HOME, LEGACY_BIN, kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
OLD=${LEGACY_BIN:?set LEGACY_BIN to a v2.17.0 chronik-server}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/colcatch-$LABEL; LOG=tests/cluster/logs/colcatch-$LABEL
B=localhost:9092; N=3000000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
sql() { curl -s -m 120 -X POST localhost:6092/_sql -H 'Content-Type: application/json' -d "{\"query\":\"$1\"}"; }
num() { grep -o "\"$1\":[0-9]*" | head -1 | cut -d: -f2; }
pid=
start() { # binary, log suffix, extra env...
  local bin=$1 suffix=$2; shift 2
  env CHRONIK_ADVERTISED_ADDR=localhost "$@" "$bin" start --data-dir "$D" > "$LOG-$suffix.log" 2>&1 &
  pid=$!
  for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return; sleep 1; done
  echo "FAIL: $bin did not come up"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; pid=; }
trap 'stop' EXIT
produce() { # topic
  $K/kafka-topics.sh --bootstrap-server $B --create --topic "$1" --partitions 1 --replication-factor 1 >/dev/null 2>&1
  $K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name "$1" \
    --add-config 'columnar.enabled=true,columnar.projection=otlp-logs' >/dev/null 2>&1
  sleep 2
  awk -v n="$N" 'BEGIN { for (o = 0; o < n; o++)
    printf "{\"severity\":\"%s\",\"resource\":{\"service.name\":\"svc%d\"},\"body\":\"record %d\"}\n", (o % 6 ? "INFO" : "ERROR"), o % 5, o }' |
    kcat -P -b $B -t "$1" -p 0 -X acks=1 -X linger.ms=20 -X batch.num.messages=10000 2>/dev/null
}
cold() { sql "SELECT COUNT(*) AS c FROM ${1//-/_}_cold" | num c; }
settle() { # topic: wait until the cold count stops moving (the old indexer is done)
  local last=-1 c same=0
  for _ in $(seq 120); do
    c=$(cold "$1"); c=${c:-0}
    if [ "$c" = "$last" ] && [ "$c" != 0 ]; then same=$((same + 1)); [ $same -ge 4 ] && break; else same=0; fi
    last=$c; sleep 10
  done
  echo "$c"
}

echo "=== $LABEL ($BIN, old $OLD)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"

start "$OLD" old
produce cc-old
before=$(settle cc-old)
stop
echo "  old binary left $before of $N records in Parquet"
if [ "${before:-0}" -ge $N ]; then
  echo "SKIP: the old flush wrote every record this time; the test cannot judge the catch-up"
  exit 77
fi

start "$BIN" new CHRONIK_COLUMNAR_CATCHUP_DELAY_SECS=5 CHRONIK_COLUMNAR_CATCHUP_INTERVAL_SECS=10
c=0
for _ in $(seq 90); do c=$(cold cc-old); [ "${c:-0}" = $N ] && break; sleep 5; done
expect "catch-up: every record in the cold table" "${c:-0}" "$N"
r=$(sql "SELECT COUNT(DISTINCT _offset) AS d, MIN(_offset) AS lo, MAX(_offset) AS hi FROM cc_old_cold")
expect "catch-up: each offset once" "$(num d <<<"$r")" "$N"
expect "catch-up: lowest offset" "$(num lo <<<"$r")" "0"
expect "catch-up: highest offset" "$(num hi <<<"$r")" "$((N - 1))"
expect "catch-up: projected ERROR count" \
  "$(sql "SELECT COUNT(*) AS c FROM cc_old_cold WHERE severity = 'ERROR'" | num c)" "$(( (N + 5) / 6 ))"
expect "catch-up logged its work" "$(grep -c 'columnar catch-up pass: wrote' "$LOG-new.log" | awk '{print ($1 > 0)}')" "1"

produce cc-new
c=0
for _ in $(seq 60); do c=$(cold cc-new); [ "${c:-0}" = $N ] && break; sleep 5; done
expect "new binary: every record in the cold table" "${c:-0}" "$N"
expect "new binary: each offset once" "$(sql "SELECT COUNT(DISTINCT _offset) AS d FROM cc_new_cold" | num d)" "$N"

expect "panics" "$(cat "$LOG-old.log" "$LOG-new.log" | grep -c panicked)" "0"
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
