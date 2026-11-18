#!/bin/bash
# The transactional APIs are authorized (ACLs on).
#
# Before, InitProducerId, AddPartitionsToTxn, AddOffsetsToTxn, EndTxn and
# TxnOffsetCommit were never checked: any principal could take over another
# application's transactional id (InitProducerId fences its producer) and
# commit a group's offsets through TxnOffsetCommit with no right on the group.
#
# One node, ACLs on and closed by default, every client User:ANONYMOUS (no
# SASL), allowed to write the topic but holding Write on only one
# transactional id. With the stock Java producer (kafka-producer-perf-test):
#   - a transactional producer with an id it may not use is refused with
#     TransactionalIdAuthorizationException and writes nothing;
#   - with the id it may use, it commits all its records;
#   - a plain (non-transactional) producer still works.
#
# Usage: txn_acl.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/txnacl-$LABEL; LOG=tests/cluster/logs/txnacl-$LABEL
B=localhost:9092; T=txn-acl
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
end_offset() { kcat -Q -b $B -t $T:0:-1 2>/dev/null | awk '{print $NF}'; }
perf() { # transactional id (or empty), records
  local txn=()
  [ -n "$1" ] && txn=(--transactional-id "$1" --transaction-duration-ms 200)
  timeout 90 $K/kafka-producer-perf-test.sh --topic $T --num-records "$2" --record-size 100 --throughput -1 \
    "${txn[@]}" --producer-props bootstrap.servers=$B acks=all 2>&1
}

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
A=User:ANONYMOUS
CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_ACL_ENABLED=true CHRONIK_ACL_ALLOW_IF_NO_ACL=false \
  CHRONIK_ACL_BINDINGS="$A,Topic,$T,Write,Allow;$A,Topic,$T,Describe,Allow;$A,Topic,$T,Read,Allow;$A,Topic,$T,Create,Allow;$A,Cluster,kafka-cluster,Create,Allow;$A,Cluster,kafka-cluster,Describe,Allow;$A,Cluster,kafka-cluster,IdempotentWrite,Allow;$A,TransactionalId,txn-mine,Write,Allow;$A,TransactionalId,txn-mine,Describe,Allow" \
  "$BIN" start --data-dir "$D" > "$LOG-server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
sleep 2

out=$(perf "" 1000)
expect "plain producer still writes" "$(end_offset)" "1000"

out=$(perf txn-theirs 1000)
expect "someone else's transactional id is refused" "$(grep -c -m1 'TransactionalIdAuthorizationException' <<<"$out")" "1"
expect "and nothing was written" "$(end_offset)" "1000"
expect "the denial is logged" "$(grep -c "InitProducerId DENIED for transactional id 'txn-theirs'" "$LOG-server.log" | awk '{print ($1 > 0)}')" "1"

out=$(perf txn-mine 1000)
expect "its own transactional id commits" "$(grep -c 'TransactionalIdAuthorization' <<<"$out")" "0"
committed=$(timeout 30 kcat -C -b $B -t $T -o beginning -e -q -X isolation.level=read_committed 2>/dev/null | wc -l)
expect "records readable by read_committed" "$committed" "2000"

expect "panics" "$(grep -c panicked "$LOG-server.log")" "0"
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
