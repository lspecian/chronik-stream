#!/bin/bash
# A retried idempotent produce must not leave holes in the offset space.
#
# Observed on a live cluster: under load, produce requests timed out
# (REQUEST_TIMED_OUT), the idempotent Java producer retried them, and the
# partition ended 3,405 offsets past the 30,000 records produced. The retries
# were correctly recognised as duplicates — no record was stored twice — but
# each one's offsets had been reserved before the duplicate check ran, so every
# retry burned a batch's worth of offsets that no record ever filled.
#
# This forces the same retries on a local 3-node cluster: a follower is paused
# with SIGSTOP while an acks=all producer with a short request timeout is
# writing, so the leader cannot complete the ISR wait in time and the producer
# retries. Then it compares Kafka's end offset with the number of records.
#
#   PRE-FIX : end offset > records produced (holes), with retries logged
#   POST-FIX: end offset == records produced, with retries logged
#
# A run that provokes no retry proves nothing either way, so it exits 2.
#
# Usage:
#   tests/test_retry_offset_gaps.sh                          # target/release
#   CHRONIK_BIN=/path/to/old tests/test_retry_offset_gaps.sh --expect-gaps
#
# Exit: 0 pass, 1 fail, 2 could not provoke retries / set up.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER="$PROJECT_ROOT/tests/cluster"
KAFKA_BIN="${KAFKA_BIN:-/opt/kafka/bin}"
BOOT=localhost:9092
TOPIC="gaps$$"
RECORDS="${RECORDS:-60000}"

EXPECT_GAPS=0
[ "${1:-}" = "--expect-gaps" ] && EXPECT_GAPS=1

STOPPED=""
cleanup() {
    [ -n "$STOPPED" ] && kill -CONT $STOPPED 2>/dev/null
    "$CLUSTER/stop.sh" >/dev/null 2>&1
}
trap cleanup EXIT
setup_fail() { echo "SETUP FAILED: $1" >&2; exit 2; }

[ -x "$KAFKA_BIN/kafka-topics.sh" ] || setup_fail "Kafka CLI not at $KAFKA_BIN (set KAFKA_BIN)"

echo "=== a retried idempotent produce must not leave offset holes ==="
echo "binary: ${CHRONIK_BIN:-$PROJECT_ROOT/target/release/chronik-server}"
echo "mode:   $([ $EXPECT_GAPS = 1 ] && echo 'expecting the PRE-FIX holes' || echo 'expecting the FIX')"

"$CLUSTER/stop.sh" >/dev/null 2>&1
"$CLUSTER/start.sh" >/dev/null 2>&1 || setup_fail "cluster did not start"
for _ in $(seq 1 60); do
    curl -fsS -m 2 http://localhost:6092/health >/dev/null 2>&1 && break
    sleep 2
done

# One partition led by node 1, so pausing a follower stalls exactly this ISR.
"$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$BOOT" --create --topic "$TOPIC" \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1 || setup_fail "topic create failed"
sleep 3

LOG=$(mktemp)
# Idempotent (the default with acks=all), with a request timeout short enough
# that a stalled ISR wait expires and the producer retries the same batch.
#
# Throughput-capped so the producer is still writing when the followers are
# paused. An uncapped run pushes all records through in about a second and is
# finished before the pause starts — which proves nothing and looks like a pass.
THROUGHPUT="${THROUGHPUT:-3000}"
"$KAFKA_BIN/kafka-producer-perf-test.sh" --topic "$TOPIC" \
    --num-records "$RECORDS" --record-size 100 --throughput "$THROUGHPUT" \
    --producer-props bootstrap.servers="$BOOT" acks=all enable.idempotence=true \
        request.timeout.ms=1500 delivery.timeout.ms=120000 linger.ms=20 >"$LOG" 2>&1 &
PRODUCER=$!

# Pause BOTH followers mid-stream, long enough for several requests to time
# out, then release them so ISR recovers and the producer can finish. Pausing
# one is not enough: acks=all completes once min.insync.replicas have the data,
# and the other follower still does.
LEADER=$("$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$BOOT" --describe --topic "$TOPIC" 2>/dev/null \
         | sed -n 's/.*Leader: \([0-9]*\).*/\1/p' | head -1)
[ -n "$LEADER" ] || setup_fail "could not determine the partition leader"
FOLLOWERS=""
for n in 1 2 3; do [ "$n" != "$LEADER" ] && FOLLOWERS="$FOLLOWERS $(cat "$CLUSTER/data/node$n.pid")"; done
echo "leader=node$LEADER, pausing followers:$FOLLOWERS"
sleep 2
STOPPED="$FOLLOWERS"
kill -STOP $FOLLOWERS
sleep 8
kill -CONT $FOLLOWERS; STOPPED=""

wait $PRODUCER
PRODUCER_EXIT=$?

retries=$(grep -cE "retrying" "$LOG")
dups=$(grep -cE "DUPLICATE_SEQUENCE" "$LOG")
echo "producer exit=$PRODUCER_EXIT  retries=$retries  duplicate-sequence responses=$dups"
grep -oE "Error: [A-Z_]+" "$LOG" | sort | uniq -c | sed 's/^/    /'

[ "$retries" -gt 0 ] || setup_fail "no produce request was retried — the scenario was not reached"
[ "$PRODUCER_EXIT" -eq 0 ] || setup_fail "producer failed (exit $PRODUCER_EXIT) — see $LOG"
# Every record must have been acknowledged, or "end offset == records" could
# hold by accident (holes offset by records that never landed).
failed_sends=$(grep -cE "TimeoutException|Exception in callback|Error when sending" "$LOG")
[ "$failed_sends" -eq 0 ] || setup_fail "$failed_sends sends failed outright — counts would not be comparable (see $LOG)"
grep -qE "^$RECORDS records sent" "$LOG" || setup_fail "perf test did not report all $RECORDS records sent (see $LOG)"

sleep 3
END=$("$KAFKA_BIN/kafka-get-offsets.sh" --bootstrap-server "$BOOT" --topic "$TOPIC" 2>/dev/null \
      | awk -F: '{s+=$3} END {print s+0}')
echo "records produced: $RECORDS   end offset: $END   holes: $((END - RECORDS))"

if [ $EXPECT_GAPS = 1 ]; then
    if [ "$END" -gt "$RECORDS" ]; then
        echo "PASS: pre-fix reproduces — $((END - RECORDS)) offsets hold no record"
        exit 0
    fi
    echo "FAIL: pre-fix did not leave holes this run (end=$END)"
    exit 1
fi

if [ "$END" -eq "$RECORDS" ]; then
    echo "PASS: $retries retries, and every offset holds exactly one record"
    exit 0
fi
echo "FAIL: end offset $END for $RECORDS records — $((END - RECORDS)) offsets hold no record"
exit 1
