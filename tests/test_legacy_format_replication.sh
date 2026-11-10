#!/bin/bash
# A topic written in the legacy (v0/v1) message format must replicate.
#
# Followers used to refuse anything but v2 batches ("only v2 batches can be
# replicated"), and the refusal was not a backoff — the follower fetched the
# same offset again at once, forever, while the leader logged every message of
# every re-read. On a live cluster that burnt 3.5–5 cores per node, and those
# partitions had never been replicated: their replication factor was one, so
# losing the leader lost the data.
#
# This writes in v1 with librdkafka pinned to a 0.10 broker, then proves the
# followers hold the data the only way that matters: freeze the leader and read
# every record back from whichever follower takes over.
#
#   PRE-FIX : followers drop out of ISR; the frozen leader's partition is
#             stranded or the new leader serves nothing
#   POST-FIX: all replicas in sync; every record readable after failover
#
# Usage:
#   tests/test_legacy_format_replication.sh                     # target/release
#   CHRONIK_BIN=/path/to/old tests/test_legacy_format_replication.sh
#
# Exit: 0 pass, 1 fail, 2 setup failed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER="$PROJECT_ROOT/tests/cluster"
KAFKA_BIN="${KAFKA_BIN:-/opt/kafka/bin}"
ALL=localhost:9092,localhost:9093,localhost:9094
TOPIC="legacy$$"
RECORDS="${RECORDS:-3000}"

FROZEN=""
cleanup() {
    [ -n "$FROZEN" ] && kill -CONT "$FROZEN" 2>/dev/null
    "$CLUSTER/stop.sh" >/dev/null 2>&1
}
trap cleanup EXIT
setup_fail() { echo "SETUP FAILED: $1" >&2; exit 2; }

command -v kcat >/dev/null || setup_fail "kcat not installed (needed to write the v1 format)"
[ -x "$KAFKA_BIN/kafka-topics.sh" ] || setup_fail "Kafka CLI not at $KAFKA_BIN (set KAFKA_BIN)"

LIVE="$ALL"
live_except() { LIVE=$(echo "$ALL" | tr , "\n" | grep -v ":909$(($1 + 1))\$" | paste -sd,); }
describe() {
    timeout 20 "$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$LIVE" --describe --topic "$TOPIC" 2>/dev/null \
        | grep "Partition: 0"
}
leader_of() { describe | sed -n 's/.*Leader: \([0-9-]*\).*/\1/p' | head -1; }
isr_of()    { describe | sed -n 's/.*Isr: \([0-9,]*\).*/\1/p' | head -1; }

echo "=== legacy-format topics replicate ==="
echo "binary: ${CHRONIK_BIN:-$PROJECT_ROOT/target/release/chronik-server}"

"$CLUSTER/stop.sh" >/dev/null 2>&1
"$CLUSTER/start.sh" >/dev/null 2>&1 || setup_fail "cluster did not start"
for _ in $(seq 1 60); do
    curl -fsS -m 2 http://localhost:6092/health >/dev/null 2>&1 && break
    sleep 2
done

"$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$ALL" --create --topic "$TOPIC" \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1 || setup_fail "topic create failed"
sleep 5

# Legacy message format: librdkafka picks the message version from the broker
# version it is told. 0.9 means Produce v1 with MsgVersion 0 (verified with
# `-d protocol`); 0.10 is ignored by librdkafka 2.x, which sends v2 anyway and
# made an earlier version of this test pass against the unfixed binary.
# Uncompressed, so the leader's offset rewrite (one record per message) is exact.
seq 1 "$RECORDS" | sed 's/^/legacy-/' | timeout 120 kcat -P -b "$ALL" -t "$TOPIC" -p 0 \
    -X api.version.request=false -X broker.version.fallback=0.9.0 \
    -z none 2>/dev/null
PRODUCED=$?

END=$(timeout 30 "$KAFKA_BIN/kafka-get-offsets.sh" --bootstrap-server "$ALL" --topic "$TOPIC" 2>/dev/null \
      | awk -F: '{s+=$3} END {print s+0}')
if [ $PRODUCED -ne 0 ]; then
    # acks=all (kcat's default) completes only once followers hold the data.
    # Measured against the unfixed binary: never acknowledged, retried, and the
    # end offset reached 6,000 for 3,000 messages.
    echo "FAIL: legacy produce at acks=all was never acknowledged — followers are not replicating (end offset $END for $RECORDS messages)"
    exit 1
fi
[ "$END" -eq "$RECORDS" ] || setup_fail "end offset $END for $RECORDS legacy records"
echo "produced $RECORDS records in the v1 message format"

# Give followers ample time: replication of this volume takes well under a second.
sleep 20
LEADER=$(leader_of)
ISR=$(isr_of)
echo "leader: node$LEADER   isr: [$ISR]"

FLOOD=$(cat "$CLUSTER"/logs/node*.log 2>/dev/null | grep -c "V1→OFFSET_UPDATE")
echo "per-message V1→OFFSET_UPDATE log lines: $FLOOD"

live_except "$LEADER"
FROZEN=$(cat "$CLUSTER/data/node$LEADER.pid")
kill -STOP "$FROZEN"
echo "froze leader node$LEADER"
NEW=""
for _ in $(seq 1 90); do
    l=$(leader_of)
    if [ -n "$l" ] && [ "$l" != "$LEADER" ] && [ "$l" != "-1" ]; then NEW=$l; break; fi
    sleep 1
done

GOT=0
if [ -n "$NEW" ]; then
    echo "failed over to node$NEW"
    VALUES=$(mktemp)
    timeout 90 "$KAFKA_BIN/kafka-console-consumer.sh" --bootstrap-server "$LIVE" --topic "$TOPIC" \
        --from-beginning --max-messages "$RECORDS" --timeout-ms 45000 2>/dev/null | sort -u > "$VALUES"
    GOT=$(grep -c '^legacy-' "$VALUES")
    echo "records read from the new leader: $GOT of $RECORDS"
else
    echo "no failover: no follower was in sync to take over"
fi

FAIL=0
[ "$(echo "$ISR" | tr , '\n' | grep -c .)" -eq 3 ] || { echo "FAIL: followers not in sync (isr [$ISR])"; FAIL=1; }
[ -n "$NEW" ] || { echo "FAIL: partition stranded when its leader froze"; FAIL=1; }
[ "$GOT" -eq "$RECORDS" ] || { echo "FAIL: the new leader served $GOT of $RECORDS records"; FAIL=1; }
[ "$FLOOD" -eq 0 ] || { echo "FAIL: fetch path still logs every message ($FLOOD lines)"; FAIL=1; }
[ $FAIL = 0 ] && echo "PASS: legacy-format records replicated; all $RECORDS readable after the leader froze"
exit $FAIL
