#!/bin/bash
# Issue #73: typed columns projected out of JSON values, queryable by SQL.
#
# The path chronik-signal takes, on one node:
#   1. A columnar topic runs under the default projection (`auto`): 3,000
#      signal-shaped log records are flushed to Parquet with the inferred
#      top-level fields (`severity`, `timestamp` as text, `resource` as JSON
#      text — no `service_name`).
#   2. The topic switches to `columnar.projection=otlp-logs`, with one
#      attribute. An invalid declaration is refused by the admin call.
#   3. 3,000 more records. While they are still in the hot buffer, and again
#      once flushed:
#        - WHERE severity = 'ERROR' counts all 2,000 ERRORs (old segments
#          included: they lack the new columns and are read from `_value`);
#        - GROUP BY service_name splits 2,000 api / 4,000 web;
#        - a typed timestamp range and an attribute column filter correctly;
#        - `_value` is byte-for-byte what was produced.
#   4. The cold scan of a severity filter reads the `severity` column only.
#
# Usage: projected_columns.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/projcols-$LABEL; LOG=tests/cluster/logs/projcols-$LABEL
B=localhost:9092; T=signal-logs; V=signal_logs
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
sql() { curl -s -m 60 -X POST localhost:6092/_sql -H 'Content-Type: application/json' -d "{\"query\":\"$1\"}"; }
num() { grep -o "\"$1\":[0-9]*" | head -1 | cut -d: -f2; }
# n records from offset $1: every third an ERROR from "api", the rest INFO from "web".
records() {
  awk -v from="$1" -v n="$2" 'BEGIN { for (o = from; o < from + n; o++) {
    if (o % 3 == 0) { sev = "ERROR"; svc = "api"; num = 17 } else { sev = "INFO"; svc = "web"; num = 9 }
    printf "{\"attributes\":{\"request_id\":\"req-%d\"},\"body\":\"m%d\",\"resource\":{\"service.name\":\"%s\"},\"severity\":\"%s\",\"severity_number\":%d,\"timestamp\":\"2026-07-07T03:%02d:%02d.000+00:00\",\"trace_id\":null}\n", o, o, svc, sev, num, int(o / 100), o % 60
  } }'
}
counts() { # label
  local r
  r=$(sql "SELECT COUNT(*) AS c FROM $V WHERE severity = 'ERROR'")
  expect "$1: ERRORs by projected column" "$(num c <<<"$r")" "2000"
  r=$(sql "SELECT service_name, COUNT(*) AS c FROM $V GROUP BY service_name ORDER BY service_name")
  expect "$1: GROUP BY service_name" "$(grep -o '"c":[0-9]*' <<<"$r" | cut -d: -f2 | paste -sd/)" "2000/4000"
  r=$(sql "SELECT COUNT(*) AS c FROM $V WHERE timestamp >= TIMESTAMP '2026-07-07T03:30:00Z'")
  expect "$1: typed timestamp range" "$(num c <<<"$r")" "3000"
  r=$(sql "SELECT COUNT(*) AS c FROM $V WHERE attr_request_id = 'req-4242'")
  expect "$1: attribute column" "$(num c <<<"$r")" "1"
}

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
CHRONIK_ADVERTISED_ADDR=localhost \
  "$BIN" start --data-dir "$D" > "$LOG-server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T --add-config columnar.enabled=true >/dev/null 2>&1
sleep 2

records 0 3000 | kcat -P -b $B -t $T -p 0 -X acks=1 2>/dev/null
for _ in $(seq 60); do
  r=$(curl -s -m 10 localhost:6092/_sql/describe/${V}_cold)
  grep -q '"name":"severity"' <<<"$r" && break; sleep 5
done
expect "auto projection wrote severity to Parquet" "$(grep -o '"name":"severity"' <<<"$r")" '"name":"severity"'
expect "auto projection has no service_name" "$(grep -c '"name":"service_name"' <<<"$r")" "0"

bad=$($K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T \
  --add-config 'columnar.columns=svc:decimal:/resource/service.name' 2>&1)
expect "an invalid declared column is refused" "$(grep -q -i "InvalidConfigurationException.*decimal\|decimal.*InvalidConfiguration\|unknown type 'decimal'" <<<"$bad" && echo refused || echo accepted)" "refused"
expect "and was not stored" "$($K/kafka-configs.sh --bootstrap-server $B --describe --entity-type topics --entity-name $T 2>/dev/null | grep -c 'columnar.columns')" "0"
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T \
  --add-config 'columnar.projection=otlp-logs,columnar.projection.attributes=request_id' >/dev/null 2>&1
sleep 2

records 3000 3000 | kcat -P -b $B -t $T -p 0 -X acks=1 2>/dev/null
sleep 3
counts "hot+cold"

r=$(curl -s -m 10 localhost:6092/_sql/describe/$V)
expect "timestamp is a typed column" "$(grep -o '"name":"timestamp","data_type":"Timestamp(Nanosecond, Some(\\"UTC\\"))"' <<<"$r" | wc -l)" "1"

for _ in $(seq 60); do
  r=$(sql "SELECT COUNT(*) AS c FROM ${V}_cold"); [ "$(num c <<<"$r")" = 6000 ] && break; sleep 5
done
expect "all records flushed to Parquet" "$(num c <<<"$r")" "6000"
counts "cold"

r=$(sql "SELECT decode_utf8(_value) AS v FROM $V WHERE _offset = 4242")
want=$(records 4242 1 | sed 's/"/\\"/g')
expect "_value unchanged" "$(grep -c -F "\"v\":\"$want\"" <<<"$r")" "1"

plan=$(curl -s -m 30 -X POST localhost:6092/_sql/explain -H 'Content-Type: application/json' \
  -d "{\"query\":\"SELECT COUNT(*) FROM ${V}_cold WHERE severity = 'ERROR'\"}")
scan=$(grep -o 'ParquetExec[^\\]*' <<<"$plan" | head -1)
echo "  cold scan: ${scan:0:220}"
expect "the cold scan reads severity only" "$(grep -c 'projection=\[severity\]' <<<"$scan")" "1"

panics=$(grep -c panicked "$LOG-server.log")
expect "panics" "$panics" "0"
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
