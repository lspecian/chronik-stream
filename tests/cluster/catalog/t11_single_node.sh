#!/usr/bin/env bash
# T11: single-node mode end to end (no cluster config).
# PASS: create, auto-create on produce, produce/consume, config change,
# delete and recreate all work, and survive a restart.
source "$(dirname "$0")/lib.sh"
"$DIR/stop.sh" >/dev/null 2>&1
data="$DIR/data/single"; rm -rf "$data"; mkdir -p "$data"
start_single() {
  CHRONIK_ADVERTISED_ADDR=localhost "$BIN" start --data-dir "$data" > "$DIR/logs/single.log" 2>&1 &
  echo $! > "$data.pid"; wait_port 9092
}
stop_single() { kill "$(cat "$data.pid")" 2>/dev/null; sleep 2; kill -9 "$(cat "$data.pid")" 2>/dev/null; }
start_single || { fail "single node did not start"; stop_single; exit 1; }
kt() { "$K/kafka-topics.sh" --bootstrap-server localhost:9092 "$@" 2>&1; }
kt --create --topic s-explicit --partitions 3 --replication-factor 1 | grep -q Created || fail "explicit create"
kt --create --topic s-explicit --partitions 3 --replication-factor 1 | grep -q 'already exists' || fail "duplicate not refused"
seq 1 500 | "$K/kafka-console-producer.sh" --bootstrap-server localhost:9092 --topic s-auto >/dev/null 2>&1
seq 1 500 | "$K/kafka-console-producer.sh" --bootstrap-server localhost:9092 --topic s-explicit >/dev/null 2>&1
got=$(timeout 60 "$K/kafka-console-consumer.sh" --bootstrap-server localhost:9092 --topic s-auto --group s-g --from-beginning --timeout-ms 10000 2>/dev/null | wc -l)
[ "$got" = 500 ] || fail "auto-created topic: consumed $got of 500"
"$K/kafka-configs.sh" --bootstrap-server localhost:9092 --alter --entity-type topics --entity-name s-explicit --add-config retention.ms=777777 >/dev/null 2>&1
kt --create --topic s-gone --partitions 1 --replication-factor 1 >/dev/null
kt --delete --topic s-gone >/dev/null
stop_single; start_single || fail "restart"
kt --describe --topic s-explicit | grep -q 'PartitionCount: 3' || fail "s-explicit lost across restart"
"$K/kafka-configs.sh" --bootstrap-server localhost:9092 --describe --entity-type topics --entity-name s-explicit 2>/dev/null | grep -q 'retention.ms=777777' || fail "config lost across restart"
kt --list | grep -qx s-gone && fail "deleted topic came back"
kt --create --topic s-gone --partitions 2 --replication-factor 1 | grep -q Created || fail "recreate after delete"
end=$("$K/kafka-get-offsets.sh" --bootstrap-server localhost:9092 --topic s-explicit 2>/dev/null | awk -F: '{s+=$3} END{print s+0}')
[ "$end" = 500 ] || fail "s-explicit end offsets $end after restart (want 500)"
grep -q panicked "$DIR/logs/single.log" && fail "panic in single-node log"
stop_single
if [ "$FAIL" = 0 ]; then say "== PASS: T11 single-node =="; else say "== FAIL: T11 single-node =="; fi
exit "$FAIL"
