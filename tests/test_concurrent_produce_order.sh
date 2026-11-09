#!/bin/bash
# Concurrent produce requests must enter the log in offset order, or Fetch skips
# acknowledged records.
#
# Offsets used to be reserved several awaits before the WAL append. With more
# than one produce request in flight per partition — the Java producer's default
# is five — a later offset range could reach the WAL first, leaving the file
# holding e.g. 7552 before 7549..7551. Fetch walks the WAL in file order and
# serves the first batch at or past the requested offset, so a consumer asking
# for 7549 was handed 7552 and never saw 7549..7551. Those records were
# acknowledged, on disk, and counted by SQL; no Kafka consumer could read them.
#
# The defect needs no fault at all, so this uses none: a stock idempotent
# producer at max.in.flight=5, then a full consume, checking every offset is
# read exactly once against the end offset Kafka reports.
#
#   PRE-FIX : some offsets never consumed (and they are present via SQL)
#   POST-FIX: every offset consumed
#
# Usage:
#   tests/test_concurrent_produce_order.sh                     # target/release
#   CHRONIK_BIN=/path/to/old tests/test_concurrent_produce_order.sh --expect-skips
#
# Exit: 0 pass, 1 fail, 2 setup failed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER="$PROJECT_ROOT/tests/cluster"
KAFKA_BIN="${KAFKA_BIN:-/opt/kafka/bin}"
BOOT=localhost:9092
TOPIC="order$$"
RECORDS="${RECORDS:-60000}"
INFLIGHT="${INFLIGHT:-5}"

EXPECT_SKIPS=0
[ "${1:-}" = "--expect-skips" ] && EXPECT_SKIPS=1

cleanup() { "$CLUSTER/stop.sh" >/dev/null 2>&1; }
trap cleanup EXIT
setup_fail() { echo "SETUP FAILED: $1" >&2; exit 2; }

[ -x "$KAFKA_BIN/kafka-producer-perf-test.sh" ] || setup_fail "Kafka CLI not at $KAFKA_BIN (set KAFKA_BIN)"

echo "=== concurrent produces must be readable in full ==="
echo "binary: ${CHRONIK_BIN:-$PROJECT_ROOT/target/release/chronik-server}  max.in.flight=$INFLIGHT"

"$CLUSTER/stop.sh" >/dev/null 2>&1
"$CLUSTER/start.sh" >/dev/null 2>&1 || setup_fail "cluster did not start"
for _ in $(seq 1 60); do
    curl -fsS -m 2 http://localhost:6092/health >/dev/null 2>&1 && break
    sleep 2
done

# One partition, so every in-flight request competes for the same log.
"$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$BOOT" --create --topic "$TOPIC" \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1 || setup_fail "topic create failed"
sleep 3

LOG=$(mktemp)
"$KAFKA_BIN/kafka-producer-perf-test.sh" --topic "$TOPIC" --num-records "$RECORDS" \
    --record-size 100 --throughput 4000 \
    --producer-props bootstrap.servers="$BOOT" acks=all enable.idempotence=true \
        max.in.flight.requests.per.connection="$INFLIGHT" >"$LOG" 2>&1 \
    || setup_fail "producer failed (see $LOG)"
grep -qE "^$RECORDS records sent" "$LOG" || setup_fail "not all $RECORDS records were acknowledged (see $LOG)"
sleep 5

END=$("$KAFKA_BIN/kafka-get-offsets.sh" --bootstrap-server "$BOOT" --topic "$TOPIC" 2>/dev/null \
      | awk -F: '{s+=$3} END {print s+0}')
[ "$END" -eq "$RECORDS" ] || setup_fail "end offset $END for $RECORDS records — offset holes, a different defect (see test_retry_offset_gaps.sh)"

OFFS=$(mktemp)
"$KAFKA_BIN/kafka-console-consumer.sh" --bootstrap-server "$BOOT" --topic "$TOPIC" \
    --from-beginning --max-messages "$RECORDS" --timeout-ms 60000 \
    --property print.offset=true --property print.value=false 2>/dev/null \
  | sed -n 's/.*Offset:\([0-9]*\).*/\1/p' | sort -n -u > "$OFFS"

GOT=$(wc -l < "$OFFS")
SKIPPED=$(awk 'NR==1 && $1>0 {s+=$1} NR>1 && $1-prev>1 {s+=$1-prev-1; g++} {prev=$1} END {print s+0}' "$OFFS")
echo "end offset: $END   distinct offsets consumed: $GOT   offsets skipped by Fetch: $SKIPPED"

# Prove a skipped range is really in the log, so a failure is unambiguous.
if [ "$SKIPPED" -gt 0 ]; then
    gap=$(awk 'NR>1 && $1-prev>1 {print prev+1; exit} {prev=$1}' "$OFFS")
    if [ -n "$gap" ]; then
        insql=$(curl -s -m 30 -X POST http://localhost:6092/_sql -H 'Content-Type: application/json' \
            -d "{\"query\":\"SELECT COUNT(*) FROM $TOPIC WHERE _offset = $gap\"}" \
            | sed -n 's/.*"count(\*)":\([0-9]*\).*/\1/p')
        echo "first skipped offset $gap: present in the log per SQL = ${insql:-unknown}"
    fi
fi

if [ $EXPECT_SKIPS = 1 ]; then
    [ "$SKIPPED" -gt 0 ] && { echo "PASS: pre-fix reproduces — $SKIPPED acknowledged records unreachable through Fetch"; exit 0; }
    echo "FAIL: pre-fix skipped nothing this run"; exit 1
fi
if [ "$GOT" -eq "$RECORDS" ] && [ "$SKIPPED" -eq 0 ]; then
    echo "PASS: all $RECORDS records consumed, none skipped"
    exit 0
fi
echo "FAIL: $SKIPPED records were acknowledged but never served by Fetch"
exit 1
