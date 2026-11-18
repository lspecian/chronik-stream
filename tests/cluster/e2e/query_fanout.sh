#!/bin/bash
# `/_query` answers for the whole cluster, from any node.
#
# Its backends read only the node that received the request — for SQL, only
# the partitions that node leads — and the answer came back as if complete. Its
# SQL source also looked for `offset` / `partition` columns no table has, so
# every SQL hit read as partition 0 at offset = its rank.
#
# 3-node cluster (columnar on by default), a 3-partition topic, 301 records.
# Asked through each node, a SQL source selecting every record must return 301
# distinct (partition, offset) hits spanning all three partitions, with no node
# reported missing.
#
# Usage: query_fanout.sh <binary> <label>
# Needs: kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
B=localhost:9092,localhost:9093,localhost:9094
T=qfan_$$
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
query() { # node
  curl -s -m 60 -X POST localhost:$((6091 + $1))/_query -H 'Content-Type: application/json' -d "{
    \"sources\": [{\"topic\": \"$T\", \"modes\": [\"sql\"]}],
    \"q\": {\"sql\": \"SELECT _partition, _offset FROM $T ORDER BY _partition, _offset\"},
    \"k\": 1000}"
}

echo "=== $LABEL ($BIN)"
CHRONIK_BIN=$BIN ./tests/cluster/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
for _ in $(seq 90); do kcat -L -b $B >/dev/null 2>&1 && break; sleep 2; done
sleep 10
echo '{"p":0,"n":0}' | timeout 30 kcat -P -b $B -t $T -p 0 2>/dev/null # auto-creates 3 partitions
sleep 5
for p in 0 1 2; do
  seq 1 100 | sed "s/.*/{\"p\":$p,\"n\":&}/" | timeout 60 kcat -P -b $B -t $T -p $p -X acks=all 2>/dev/null
done
sleep 5
parts=$(kcat -L -b $B -t $T 2>/dev/null | grep -c "partition [0-9]*, leader")
expect "partitions" "$parts" "3"

for n in 1 2 3; do
  r=""
  for _ in $(seq 30); do
    r=$(query $n)
    [ "$(grep -o '"offset":[0-9]*,"partition":[0-9]*' <<<"$r" | sort -u | wc -l)" = 301 ] && break
    sleep 3
  done
  hits=$(grep -o '"offset":[0-9]*,"partition":[0-9]*' <<<"$r" | sort -u | wc -l)
  spread=$(grep -o '"partition":[0-9]*' <<<"$r" | sort -u | wc -l)
  expect "node$n: distinct records" "$hits" "301"
  expect "node$n: partitions covered" "$spread" "3"
  expect "node$n: nodes reported missing" "$(grep -c '"incomplete_nodes"' <<<"$r")" "0"
done

panics=$(cat tests/cluster/logs/node*.log | grep -c panicked)
expect "panics" "$panics" "0"
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
