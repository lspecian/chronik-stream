#!/bin/bash
# An acks=0 Produce gets no response (#66).
#
# A Kafka broker writes nothing back for acks=0, and the Java client reads
# nothing: it completes the request as soon as it is sent. Chronik answered
# every acks=0 produce anyway, so the Java client found a response with no
# request waiting for it ("IllegalStateException: There are no in-flight
# requests for node 1"), dropped the connection and reconnected — 936 times
# in 200,000 records on v2.18.1, at a quarter of the throughput; whatever was
# in flight when a connection dropped was lost without an error, which acks=0
# cannot report.
#
# Here, on one node: the Java producer at acks=0 must log no protocol error
# and land every record; librdkafka at acks=0 must land every record; and
# acks=1 and acks=all must still be answered (their producers report success
# per record).
#
# Usage: acks0_no_response.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/acks0-$LABEL; LOG=tests/cluster/logs/acks0-$LABEL
B=localhost:9092
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
end_offset() { $K/kafka-get-offsets.sh --bootstrap-server $B --topic "$1" 2>/dev/null | cut -d: -f3; }

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
CHRONIK_ADVERTISED_ADDR=localhost "$BIN" start --data-dir "$D" > "$LOG-server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
for t in a0java a0rdk a1 aall; do
  $K/kafka-topics.sh --bootstrap-server $B --create --topic $t --partitions 1 --replication-factor 1 >/dev/null 2>&1
done
sleep 2

$K/kafka-producer-perf-test.sh --topic a0java --num-records 200000 --record-size 100 --throughput -1 \
  --producer-props bootstrap.servers=$B acks=0 linger.ms=5 > "$LOG-java.log" 2>&1
seq 1 50000 | kcat -P -b $B -t a0rdk -p 0 -X acks=0 2>"$LOG-kcat.log"
for a in 1 all; do
  $K/kafka-producer-perf-test.sh --topic a$a --num-records 20000 --record-size 100 --throughput -1 \
    --producer-props bootstrap.servers=$B acks=$a request.timeout.ms=10000 delivery.timeout.ms=20000 > "$LOG-acks$a.log" 2>&1
done
sleep 3

expect "Java acks=0: protocol errors" "$(grep -ciE 'IllegalState|correlation id' "$LOG-java.log")" "0"
expect "Java acks=0: records landed" "$(end_offset a0java)" "200000"
expect "librdkafka acks=0: records landed" "$(end_offset a0rdk)" "50000"
for a in 1 all; do
  expect "acks=$a: answered (producer completed all)" "$(grep -c '^20000 records sent' "$LOG-acks$a.log")" "1"
  expect "acks=$a: records landed" "$(end_offset a$a)" "20000"
done

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
