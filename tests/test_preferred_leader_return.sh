#!/bin/bash
# Leadership must return to the preferred replica once it is back in sync.
#
# Failover used to be one-way. A partition whose leader went away moved to
# another replica and stayed there forever, so every transient failure
# permanently skewed leadership. On a live cluster it did worse: two partitions
# failed over to replicas that held only part of their history, and the full
# history sat unread on the preferred replica — in sync and replicating — for
# three months, because nothing ever handed leadership back.
#
# This freezes the preferred leader long enough to be failed over, keeps
# producing through the failover, thaws it, and requires that:
#   - leadership comes back to the preferred replica on its own, and
#   - every record produced across all three phases is consumable from 0.
#
#   PRE-FIX : leadership never returns
#   POST-FIX: it returns within the rebalance interval, nothing is lost
#
# Usage:
#   tests/test_preferred_leader_return.sh                       # target/release
#   CHRONIK_BIN=/path/to/old tests/test_preferred_leader_return.sh
#
# Exit: 0 pass, 1 fail, 2 setup failed (including "failover never happened").

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER="$PROJECT_ROOT/tests/cluster"
KAFKA_BIN="${KAFKA_BIN:-/opt/kafka/bin}"
ALL=localhost:9092,localhost:9093,localhost:9094
TOPIC="prefer$$"
PER_PHASE="${PER_PHASE:-5000}"
FREEZE_SECS="${FREEZE_SECS:-75}"

# Short interval so the test does not wait Kafka's default five minutes.
export CHRONIK_LEADER_REBALANCE_INTERVAL_SECS="${CHRONIK_LEADER_REBALANCE_INTERVAL_SECS:-20}"

FROZEN=""
cleanup() {
    [ -n "$FROZEN" ] && kill -CONT "$FROZEN" 2>/dev/null
    "$CLUSTER/stop.sh" >/dev/null 2>&1
}
trap cleanup EXIT
setup_fail() { echo "SETUP FAILED: $1" >&2; exit 2; }

[ -x "$KAFKA_BIN/kafka-producer-perf-test.sh" ] || setup_fail "Kafka CLI not at $KAFKA_BIN (set KAFKA_BIN)"

# Brokers to ask. While a node is frozen it accepts connections and never
# answers, so a client that includes it hangs for its whole timeout — asked of
# the live ones only, and every call is time-boxed regardless.
LIVE="$ALL"
live_except() {
    LIVE=$(echo "$ALL" | tr , "\n" | grep -v ":909$(($1 + 1))\$" | paste -sd,)
}

# Leader and preferred replica as the cluster reports them.
describe() {
    timeout 20 "$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$LIVE" --describe --topic "$TOPIC" 2>/dev/null \
        | grep "Partition: 0"
}
leader_of()    { describe | sed -n 's/.*Leader: \([0-9-]*\).*/\1/p' | head -1; }
preferred_of() { describe | sed -n 's/.*Replicas: \([0-9]*\).*/\1/p' | head -1; }

produce() {
    "$KAFKA_BIN/kafka-producer-perf-test.sh" --topic "$TOPIC" --num-records "$PER_PHASE" \
        --record-size 100 --throughput 2000 \
        --producer-props bootstrap.servers="$LIVE" acks=all enable.idempotence=true \
            delivery.timeout.ms=120000 2>&1 | tail -1
}

echo "=== leadership returns to the preferred replica ==="
echo "binary: ${CHRONIK_BIN:-$PROJECT_ROOT/target/release/chronik-server}  interval=${CHRONIK_LEADER_REBALANCE_INTERVAL_SECS}s"

"$CLUSTER/stop.sh" >/dev/null 2>&1
"$CLUSTER/start.sh" >/dev/null 2>&1 || setup_fail "cluster did not start"
for _ in $(seq 1 60); do
    curl -fsS -m 2 http://localhost:6092/health >/dev/null 2>&1 && break
    sleep 2
done

"$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$ALL" --create --topic "$TOPIC" \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1 || setup_fail "topic create failed"
sleep 5

PREFERRED=$(preferred_of)
[ -n "$PREFERRED" ] || setup_fail "could not read the replica list"
echo "preferred replica: node$PREFERRED   leader: node$(leader_of)"

echo "phase 1 (all up):      $(produce)"

live_except "$PREFERRED"
FROZEN=$(cat "$CLUSTER/data/node$PREFERRED.pid")
kill -STOP "$FROZEN"
echo "froze node$PREFERRED for ${FREEZE_SECS}s"
MOVED=""
for _ in $(seq 1 "$FREEZE_SECS"); do
    l=$(leader_of)
    if [ -n "$l" ] && [ "$l" != "$PREFERRED" ] && [ "$l" != "-1" ]; then MOVED=$l; break; fi
    sleep 1
done
[ -n "$MOVED" ] || setup_fail "leadership never failed over off frozen node$PREFERRED — nothing to return"
echo "failed over to node$MOVED"
echo "phase 2 (failed over): $(produce)"
kill -CONT "$FROZEN"; FROZEN=""
LIVE="$ALL"
echo "thawed node$PREFERRED"

RETURNED=""
for i in $(seq 1 240); do
    if [ "$(leader_of)" = "$PREFERRED" ]; then RETURNED=$i; break; fi
    sleep 1
done
if [ -n "$RETURNED" ]; then
    echo "leadership returned to node$PREFERRED ${RETURNED}s after thaw"
else
    echo "leadership did NOT return to node$PREFERRED within 240s (leader: node$(leader_of))"
fi

echo "phase 3 (after return): $(produce)"
sleep 5

EXPECT=$((PER_PHASE * 3))
END=$("$KAFKA_BIN/kafka-get-offsets.sh" --bootstrap-server "$ALL" --topic "$TOPIC" 2>/dev/null \
      | awk -F: '{s+=$3} END {print s+0}')
OFFS=$(mktemp)
"$KAFKA_BIN/kafka-console-consumer.sh" --bootstrap-server "$ALL" --topic "$TOPIC" \
    --from-beginning --max-messages "$END" --timeout-ms 60000 \
    --property print.offset=true --property print.value=false 2>/dev/null \
  | sed -n 's/.*Offset:\([0-9]*\).*/\1/p' | sort -n -u > "$OFFS"
GOT=$(wc -l < "$OFFS")
FIRST=$(head -1 "$OFFS")
echo "produced: $EXPECT   end offset: $END   distinct offsets consumed: $GOT   first: ${FIRST:-none}"

FAIL=0
[ -n "$RETURNED" ] || { echo "FAIL: leadership stayed on node$MOVED"; FAIL=1; }
[ "$END" -eq "$EXPECT" ] || { echo "FAIL: end offset $END for $EXPECT acknowledged records"; FAIL=1; }
[ "$GOT" -eq "$END" ] && [ "${FIRST:-x}" = "0" ] || { echo "FAIL: consumed $GOT of $END offsets from ${FIRST:-none}"; FAIL=1; }
[ $FAIL = 0 ] && echo "PASS: leadership returned to the preferred replica and all $EXPECT records are readable from 0"
exit $FAIL
