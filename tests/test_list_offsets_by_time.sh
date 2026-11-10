#!/bin/bash
# ListOffsets by timestamp must return the first offset at or after the time.
#
# It was a placeholder that answered offset 500 for every timestamp, so
# offsetsForTimes, `kafka-consumer-groups --reset-offsets --to-datetime`, and
# every "start from time T" consumer silently began at an arbitrary record.
#
# Produces two phases a few seconds apart, then asks for the moment between
# them, a moment before everything, and one after everything.
#
#   PRE-FIX : 500, 500, 500
#   POST-FIX: first offset of phase 2, 0, -1
#
# Exit: 0 pass, 1 fail, 2 setup failed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER="$PROJECT_ROOT/tests/cluster"
KAFKA_BIN="${KAFKA_BIN:-/opt/kafka/bin}"
ALL=localhost:9092,localhost:9093,localhost:9094
TOPIC="bytime$$"
PHASE="${PHASE:-2000}"

cleanup() { "$CLUSTER/stop.sh" >/dev/null 2>&1; }
trap cleanup EXIT
setup_fail() { echo "SETUP FAILED: $1" >&2; exit 2; }
[ -x "$KAFKA_BIN/kafka-get-offsets.sh" ] || setup_fail "Kafka CLI not at $KAFKA_BIN (set KAFKA_BIN)"

now_ms() { date +%s%3N; }
# The broker's raw answer. kcat prints -1 as -1; kafka-get-offsets omits a
# partition whose answer is -1, which reads as "no answer".
offset_at() {
    timeout 30 kcat -Q -b "$ALL" -t "$TOPIC:0:$1" 2>/dev/null \
        | sed -n 's/.* offset \(-\{0,1\}[0-9]*\).*/\1/p' | head -1
}
produce() {
    "$KAFKA_BIN/kafka-producer-perf-test.sh" --topic "$TOPIC" --num-records "$PHASE" \
        --record-size 100 --throughput 4000 \
        --producer-props bootstrap.servers="$ALL" acks=all >/dev/null 2>&1 || setup_fail "produce failed"
}

echo "=== ListOffsets by timestamp ==="
echo "binary: ${CHRONIK_BIN:-$PROJECT_ROOT/target/release/chronik-server}"

"$CLUSTER/stop.sh" >/dev/null 2>&1
"$CLUSTER/start.sh" >/dev/null 2>&1 || setup_fail "cluster did not start"
for _ in $(seq 1 60); do
    curl -fsS -m 2 http://localhost:6092/health >/dev/null 2>&1 && break
    sleep 2
done
"$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$ALL" --create --topic "$TOPIC" \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1 || setup_fail "topic create failed"
sleep 3

BEFORE=$(now_ms)
sleep 1
produce
sleep 3
BETWEEN=$(now_ms)
sleep 1
produce
sleep 3
AFTER=$(now_ms)

got_between=$(offset_at "$BETWEEN")
got_before=$(offset_at "$BEFORE")
got_after=$(offset_at "$AFTER")
echo "time between phases -> ${got_between:-none} (expect $PHASE)"
echo "time before all     -> ${got_before:-none} (expect 0)"
echo "time after all      -> ${got_after:-none} (expect -1, nothing that recent)"

FAIL=0
[ "${got_between:-x}" = "$PHASE" ] || { echo "FAIL: between phases"; FAIL=1; }
[ "${got_before:-x}" = "0" ] || { echo "FAIL: before everything"; FAIL=1; }
[ "${got_after:-x}" = "-1" ] || { echo "FAIL: after everything"; FAIL=1; }
[ $FAIL = 0 ] && echo "PASS: ListOffsets by timestamp returns the first offset at or after the time"
exit $FAIL
