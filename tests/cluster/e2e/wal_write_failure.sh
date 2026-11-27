#!/bin/bash
# A failed WAL write stops the broker, and every acknowledged record survives.
#
# The data directory is a small tmpfs, filled until a WAL write fails with
# ENOSPC (a real kernel error, not an injected one). The space then comes
# back at once, as it does when another process frees it, and a Java
# producer (kafka-verifiable-producer, which reports every acknowledged
# record; idempotent at acks=all) keeps producing for a while.
#
# Up to v2.20.4 this lost acknowledged records two ways. tokio's
# `File::sync_all` swallows the error of the write before it, so a batch cut
# short by the full disk was fsynced and acknowledged anyway. And the
# partition kept writing after a failure: later batches landed behind the
# partial record, where recovery and fetch cannot reach them (v2.20.4,
# acks=all: 2,443,506 acknowledged, end offset 1,776,531 after the restart,
# and consumers stuck after 57,710 records, behind which even the records
# written after the restart were unreadable). Now the write error is caught
# before the fsync, the partition stops at the first failure, and the broker
# exits 74.
#
# Checks: the broker exits 74 with the WAL failure in its log; after a restart
# with space the partition takes writes again, the whole log reads back with
# no gap, and every record the producer saw acknowledged is in it.
#
# Usage: [ACKS=1] wal_write_failure.sh <binary> <label>   (default acks=all)
# Needs: KAFKA_HOME, passwordless sudo (to mount the tmpfs).
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}; ACKS=${ACKS:--1}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
sudo -n true 2>/dev/null || { echo "SKIP: needs passwordless sudo to mount a tmpfs"; exit 77; }
D=$REPO/tests/cluster/data/walfail-$LABEL; LOG=$REPO/tests/cluster/logs/walfail-$LABEL
B=localhost:9092; T=walfail
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
start_broker() {
  CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_HOT_VECTOR_ENABLED=false \
    "$BIN" start --data-dir "$D" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL broker did not come up"; return 1
}
cleanup() {
  kill $pid $ppid 2>/dev/null; wait $pid 2>/dev/null
  sudo umount "$D" 2>/dev/null
}

echo "=== $LABEL ($BIN, acks=$ACKS)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
mountpoint -q "$D" && sudo umount "$D"
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
sudo mount -t tmpfs -o size=128m,uid=$(id -u),gid=$(id -g) tmpfs "$D"
pid=; ppid=
trap cleanup EXIT

start_broker || exit 1
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
sleep 2

# Leave a few MB: the producer fills the rest.
free_kb=$(df --output=avail -k "$D" | tail -1)
fallocate -l $(( (free_kb - 4096) * 1024 )) "$D/filler"
echo "  free before producing: $(df --output=avail -h "$D" | tail -1 | tr -d ' ')"

idem=true; [ "$ACKS" = -1 ] || idem=false   # idempotence requires acks=all
printf "enable.idempotence=$idem\nretries=2147483647\nlinger.ms=5\nrequest.timeout.ms=5000\ndelivery.timeout.ms=15000\nmax.block.ms=5000\n" > "$LOG-producer.properties"
$K/kafka-verifiable-producer.sh --bootstrap-server $B --topic $T --max-messages 5000000 \
  --throughput -1 --acks "$ACKS" --value-prefix 1 \
  --producer.config "$LOG-producer.properties" \
  > "$LOG-producer.json" 2> "$LOG-producer.err" &
ppid=$!

# Wait for the first failed WAL write, then give the space back at once.
for _ in $(seq 120); do
  grep -q "Commit batch failed" "$LOG-server.log" && break
  sleep 0.5
done
grep -q "Commit batch failed" "$LOG-server.log" || { echo "  FAIL the disk never filled"; exit 1; }
rm -f "$D/filler"
echo "  first failed write; space freed"

# Keep producing for a while on whatever is left running.
sleep 10
kill $ppid 2>/dev/null; wait $ppid 2>/dev/null; ppid=

if kill -0 $pid 2>/dev/null; then
  exit_code=running
  kill $pid; wait $pid 2>/dev/null
else
  wait $pid; exit_code=$?
fi
expect "broker stopped on the failed write (exit code)" "$exit_code" "74"
expect "log says why" "$(grep -c 'WAL write failed; stopping the broker' "$LOG-server.log" | awk '{print ($1>0)?"yes":"no"}')" "yes"

acked=$(grep -c '"name":"producer_send_success"' "$LOG-producer.json")
echo "  acknowledged records: $acked"
[ "$acked" -gt 0 ] || { echo "  FAIL nothing was acknowledged"; fail=1; }

# Restart with room and read everything back.
sudo mount -o remount,size=512m "$D"
start_broker || exit 1
sleep 3
echo "  end offset after restart: $($K/kafka-get-offsets.sh --bootstrap-server $B --topic $T 2>/dev/null | cut -d: -f3)"

# The partition must take writes again, and they must be readable: a partial
# record left by the failed write must not hide them.
$K/kafka-verifiable-producer.sh --bootstrap-server $B --topic $T --max-messages 20000 \
  --throughput -1 --acks "$ACKS" --value-prefix 2 --producer.config "$LOG-producer.properties" \
  > "$LOG-producer2.json" 2>> "$LOG-producer.err"
expect "records acknowledged after restart" "$(grep -c '"name":"producer_send_success"' "$LOG-producer2.json")" "20000"

end=$($K/kafka-get-offsets.sh --bootstrap-server $B --topic $T 2>/dev/null | cut -d: -f3)
$K/kafka-console-consumer.sh --bootstrap-server $B --topic $T --from-beginning \
  --max-messages "${end:-0}" --timeout-ms 60000 2>/dev/null > "$LOG-consumed-raw.txt"
expect "whole log readable (records read = end offset ${end:-none})" "$(wc -l < "$LOG-consumed-raw.txt")" "${end:-none}"
sort -u "$LOG-consumed-raw.txt" > "$LOG-consumed.txt"
cat "$LOG-producer.json" "$LOG-producer2.json" | grep '"name":"producer_send_success"' \
  | sed -E 's/.*"value":"([^"]*)".*/\1/' | sort -u > "$LOG-acked.txt"
missing=$(comm -23 "$LOG-acked.txt" "$LOG-consumed.txt" | wc -l)
expect "acknowledged records missing" "$missing" "0"

[ $fail = 0 ] && echo "RESULT $LABEL: PASS" || echo "RESULT $LABEL: FAIL"
exit $fail
