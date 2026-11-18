#!/bin/bash
# Issue #73 benchmark: a filter / GROUP BY on a projected column against the
# same query decoding `_value` with the JSON functions, on one topic.
#
# One node; an `otlp-logs` topic; N signal-shaped log records (default 1M),
# waited for until every one is in Parquet (counted, not assumed). Each query
# runs RUNS times against `{topic}_cold`; the median server-side time is shown.
#
# Usage: bench_projection.sh <binary> [records] [runs]
# Needs: KAFKA_HOME, kcat.
set -u
BIN=${1:?binary}; N=${2:-1000000}; RUNS=${3:-5}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/benchproj; LOG=tests/cluster/logs/benchproj
B=localhost:9092; T=bench-logs; V=bench_logs
sql() { curl -s -m 600 -X POST localhost:6092/_sql -H 'Content-Type: application/json' -d "{\"query\":\"$1\"}"; }
num() { grep -o "\"$1\":[0-9]*" | head -1 | cut -d: -f2; }

tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
CHRONIK_ADVERTISED_ADDR=localhost "$BIN" start --data-dir "$D" > "$LOG-server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T \
  --add-config 'columnar.enabled=true,columnar.projection=otlp-logs' >/dev/null 2>&1
sleep 2

start=$SECONDS
awk -v n="$N" 'BEGIN { split("ERROR WARN INFO DEBUG INFO INFO", sev, " "); split("api web db cache auth", svc, " ");
  for (o = 0; o < n; o++) {
    s = sev[o % 6 + 1]; v = svc[o % 5 + 1]
    printf "{\"attributes\":{\"request_id\":\"req-%d\"},\"body\":\"request %d handled by %s\",\"resource\":{\"service.name\":\"%s\"},\"severity\":\"%s\",\"severity_number\":%d,\"timestamp\":\"2026-07-07T%02d:%02d:%02d.000+00:00\",\"trace_id\":null}\n", o, o, v, v, s, o % 24, o % 24, int(o / 60) % 60, o % 60
  } }' | kcat -P -b $B -t $T -p 0 -X acks=1 -X linger.ms=20 -X batch.num.messages=10000 2>/dev/null
echo "produced $N records in $((SECONDS - start))s; waiting for Parquet"
for _ in $(seq 720); do
  c=$(sql "SELECT COUNT(*) AS c FROM ${V}_cold" | num c); [ "${c:-0}" = "$N" ] && break; sleep 5
done
[ "${c:-0}" = "$N" ] || { echo "FAIL: only ${c:-0} of $N records reached Parquet"; exit 1; }
echo "all $N in Parquet after $((SECONDS - start))s"

median() { sort -n | awk '{ a[NR] = $1 } END { print a[int((NR + 1) / 2)] }'; }
bench() { # label, sql
  local times=() r ms
  sql "$2" >/dev/null # warm the file cache and the listing
  for _ in $(seq "$RUNS"); do
    r=$(sql "$2"); ms=$(num execution_time_ms <<<"$r"); times+=("$ms")
  done
  printf '  %-58s %6s ms   %s\n' "$1" "$(printf '%s\n' "${times[@]}" | median)" "$(grep -o '"rows":.*' <<<"$r" | cut -c1-80)"
}
echo "median of $RUNS runs, $N records:"
bench "count WHERE severity = 'ERROR' (projected)" \
  "SELECT COUNT(*) AS c FROM ${V}_cold WHERE severity = 'ERROR'"
bench "count WHERE json_extract_string(_value, '\$.severity')" \
  "SELECT COUNT(*) AS c FROM ${V}_cold WHERE json_extract_string(_value, '\$.severity') = 'ERROR'"
bench "GROUP BY service_name (projected)" \
  "SELECT service_name, COUNT(*) AS c FROM ${V}_cold GROUP BY service_name ORDER BY service_name"
bench "GROUP BY json_project_string(_value, service.name)" \
  "SELECT json_project_string(_value, '/resource/service.name') AS s, COUNT(*) AS c FROM ${V}_cold GROUP BY s ORDER BY s"
bench "count WHERE severity_number >= 17 (projected)" \
  "SELECT COUNT(*) AS c FROM ${V}_cold WHERE severity_number >= 17"
bench "same through the unified view (hot + cold)" \
  "SELECT COUNT(*) AS c FROM ${V} WHERE severity = 'ERROR'"
