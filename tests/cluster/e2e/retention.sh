#!/bin/bash
# Topic retention (retention.ms, retention.bytes) deletes archived data, and a
# DeleteRecords' deleted records leave the archive and search too.
#
# Up to v2.19.2 neither limit was enforced once data was archived (the WAL is
# reclaimed after archiving, so that is all of it but the tail): archives were
# kept forever. DeleteRecords moved the log start and freed WAL only; search
# kept answering with the deleted records.
#
# Here, one node, a retention pass every 5 s: three searchable topics, each
# given three archived segments (three bursts, each sealed and archived), with
# no limit while they pile up (retention.ms=-1). Then:
#   - ms:    retention.ms=1000          -> the two older segments go;
#   - bytes: retention.bytes=1          -> the two older segments go;
#   - del:   DeleteRecords to burst 3   -> the two older segments go;
# and for each: the earliest offset is burst 3's first, a consumer from the
# beginning reads burst 3 only, one raw segment is left, search finds burst 3's
# word and not burst 1's. After a restart the same holds. A topic created
# without retention.ms gets Kafka's default (7 days); one with -1 keeps
# everything (the "keep" topic below keeps its three segments).
#
# Usage: retention.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/retention-$LABEL; LOG=tests/cluster/logs/retention-$LABEL
B=localhost:9092; PER=500
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
segments() { ls "$D"/segments/segments/"$1"/0/*.segment 2>/dev/null | wc -l; }
earliest() { $K/kafka-get-offsets.sh --bootstrap-server $B --topic "$1" --time -2 2>/dev/null | awk -F: '{ print $3 }'; }
first_read() { timeout 30 kcat -C -b $B -t "$1" -p 0 -o beginning -c 1 -q -f '%o' 2>/dev/null; }
count_read() { timeout 60 kcat -C -b $B -t "$1" -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | wc -l; }
found() { # topic word -> hits
  curl -s -m 30 -X POST "localhost:6092/$1/_search" -H 'Content-Type: application/json' \
    -d "{\"query\":{\"match\":{\"value\":\"$2\"}},\"size\":$PER,\"track_total_hits\":true}" \
    | grep -o '"total":{"value":[0-9]*' | grep -o '[0-9]*$'
}
start() {
  CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true \
    CHRONIK_RETENTION_DELAY_SECS=5 CHRONIK_RETENTION_CHECK_INTERVAL_SECS=5 \
    "$BIN" start --data-dir "$D" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL: server did not start"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
start
trap stop EXIT
TOPICS="ms bytes del keep"
for t in $TOPICS; do
  $K/kafka-topics.sh --bootstrap-server $B --create --topic "$t" --partitions 1 --replication-factor 1 \
    --config retention.ms=-1 >/dev/null 2>&1
done
$K/kafka-topics.sh --bootstrap-server $B --create --topic dflt --partitions 1 --replication-factor 1 >/dev/null 2>&1
sleep 2
for burst in 1 2 3; do
  for t in $TOPICS; do
    seq 1 $PER | awk -v b=$burst '{ printf "burst%dword record %d\n", b, $1 }' | kcat -P -b $B -t "$t" -p 0 -X acks=1 2>/dev/null
  done
  for _ in $(seq 40); do
    n=0; for t in $TOPICS; do [ "$(segments "$t")" -ge $burst ] && n=$((n + 1)); done
    [ $n = 4 ] && break; sleep 3
  done
done
expect "three archived segments each" "$(for t in $TOPICS; do printf '%s ' "$(segments "$t")"; done)" "3 3 3 3 "
b3=$((2 * PER)) # burst 3's first offset

$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name ms --add-config retention.ms=1000 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name bytes --add-config retention.bytes=1 >/dev/null 2>&1
printf '{"partitions":[{"topic":"del","partition":0,"offset":%d}],"version":1}\n' $b3 > "$D/delete-records.json"
$K/kafka-delete-records.sh --bootstrap-server $B --offset-json-file "$D/delete-records.json" >/dev/null 2>&1
for _ in $(seq 30); do
  [ "$(segments ms)$(segments bytes)$(segments del)" = 111 ] && break; sleep 2
done
sleep 3

check() { # when
  echo "$1:"
  for t in ms bytes del; do
    expect "$t: earliest offset" "$(earliest $t)" "$b3"
    expect "$t: first record read" "$(first_read $t)" "$b3"
    expect "$t: records read" "$(count_read $t)" "$PER"
    expect "$t: raw segments left" "$(segments $t)" "1"
    expect "$t: search, deleted burst 1 / kept burst 3" "$(found $t burst1word) $(found $t burst3word)" "0 $PER"
  done
  expect "keep (-1): every segment and record" "$(segments keep) $(count_read keep)" "3 $((3 * PER))"
}
check "after the retention pass"
stop; start; sleep 8
check "after a restart"

dflt=$($K/kafka-configs.sh --bootstrap-server $B --describe --all --entity-type topics --entity-name dflt 2>/dev/null \
  | grep -o ' retention.ms=[0-9-]*' | head -1 | cut -d= -f2)
expect "topic created without retention.ms gets Kafka's default" "$dflt" "604800000"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
