#!/bin/bash
# A non-idempotent producer's pipelined records are stored in the order sent.
#
# librdkafka (kcat) keeps many requests in flight on one connection and, with
# idempotence off, has no sequence numbers the broker could use to repair the
# order. Each broker request runs in its own task; before produce_order, a later
# request could reach the partition's append lock first and take the lower
# offsets. v2.16.11 stored 1M numbered records with up to 5 inversions per run
# and no duplicates, i.e. with no retries involved.
#
# Usage: produce_order.sh <binary> <label> [records per run, default 1000000]
# PASS = every record consumed once, in the order produced, at acks=1 and all.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}; N=${3:-1000000}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
B=localhost:9092,localhost:9093,localhost:9094
cd "$REPO"
up() { for _ in $(seq 240); do (exec 3<>/dev/tcp/127.0.0.1/$1) 2>/dev/null && return 0; sleep 0.5; done; return 1; }

echo "=== $LABEL ($TEST_BIN)"
CHRONIK_BIN=$TEST_BIN ./tests/cluster/start.sh >/dev/null 2>&1
for p in 9092 9093 9094; do up $p || { echo "FAIL: broker on $p did not start"; exit 1; }; done
sleep 10

fail=0
for acks in 1 all; do
  t="order-$LABEL-acks$acks"
  seq -f '%010g' 1 "$N" | kcat -P -b $B -t "$t" -p 0 -X acks=$acks 2>/dev/null
  sleep 3
  line=$(kcat -C -b $B -t "$t" -p 0 -o beginning -e -q -f '%s\n' 2>/dev/null | awk '
      { v = $1 + 0; n++; if (v < prev) inv++; prev = v; seen[v]++ }
      END { dup = 0; for (k in seen) if (seen[k] > 1) dup += seen[k] - 1
            printf "consumed=%d inversions=%d duplicates=%d", n, inv, dup }')
  echo "acks=$acks $line"
  [ "$line" = "consumed=$N inversions=0 duplicates=0" ] || fail=1
done
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
