#!/bin/bash
# The SQL hot buffer keeps the NEWEST CHRONIK_HOT_BUFFER_MAX_RECORDS records
# (#68).
#
# It read the limit forward from the Parquet flush point, so it kept the oldest
# unflushed records: on v2.18.1, 100,000 fresh records with a 10,000 limit gave
# `/_sql` offsets 0-9,999, and the newest 90,000 were in neither table until the
# flush caught up. (The issue also said the limit counted produce batches; the
# WAL read has counted messages since v2.16.4, and this run confirms 10,000.)
#
# Here, on one node with a 10,000 limit, before the first Parquet flush
# (segments are at least 10 s old when indexed): 100,000 records in batches of
# 100, then `/_sql` must see exactly the newest 10,000 (offsets 90,000-99,999),
# and every record once the flush runs.
#
# Usage: hot_buffer_limit.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/hotbuf-$LABEL; LOG=tests/cluster/logs/hotbuf-$LABEL
B=localhost:9092; T=hotbuf
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
sql() { curl -s -m 30 -X POST localhost:6092/_sql -H 'Content-Type: application/json' -d "{\"query\":\"$1\"}"; }
num() { grep -o "\"$1\":[0-9]*" | head -1 | cut -d: -f2; }

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_HOT_BUFFER_MAX_RECORDS=10000 \
  "$BIN" start --data-dir "$D" > "$LOG-server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T --add-config columnar.enabled=true >/dev/null 2>&1
sleep 2
seq 0 99999 | awk '{ printf "{\"n\":%d}\n", $1 }' \
  | kcat -P -b $B -t $T -p 0 -X acks=1 -X batch.num.messages=100 -X linger.ms=50 2>/dev/null
sleep 4 # the hot buffer refreshes every second

r=$(sql "SELECT COUNT(*) AS c, MIN(_offset) AS lo, MAX(_offset) AS hi FROM $T")
expect "rows the hot buffer holds (newest)" "$(num c <<<"$r") $(num lo <<<"$r") $(num hi <<<"$r")" "10000 90000 99999"

for _ in $(seq 60); do
  r=$(sql "SELECT COUNT(*) AS c FROM $T"); [ "$(num c <<<"$r")" = 100000 ] && break; sleep 5
done
expect "every record once flushed to Parquet" "$(num c <<<"$r")" "100000"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
