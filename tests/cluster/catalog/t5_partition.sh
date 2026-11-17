#!/usr/bin/env bash
# T5: network partition {leader} | {other two}.
# PASS: the minority refuses catalog writes; the majority elects a leader and
# keeps accepting them; after healing, every node holds one identical catalog.
source "$(dirname "$0")/lib.sh"
cluster_start || { fail "cluster did not start"; finish T5; }
L=$(current_leader); others=$(for n in 1 2 3; do [ "$n" != "$L" ] && echo $n; done | tr '\n' ' ')
set -- $others; A=$1; B=$2
say "isolating leader node $L from nodes $A and $B"
fault "$L" "drop $A" "drop $B"; fault "$A" "drop $L"; fault "$B" "drop $L"
# Minority: a create through the isolated node must not succeed.
if timeout 40 "$K/kafka-topics.sh" --bootstrap-server "localhost:$(port_of $L)" --create --topic t5-minority \
     --partitions 1 --replication-factor 1 >/dev/null 2>&1; then
  fail "the isolated node accepted a catalog write"
else
  pass "isolated node refused the write"
fi
# Majority: a new leader among A/B accepts writes.
ok=0
for _ in $(seq 1 30); do
  if "$K/kafka-topics.sh" --bootstrap-server "localhost:$(port_of $A)" --create --topic t5-majority \
       --partitions 3 --replication-factor 2 >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] && pass "majority accepted a write" || fail "majority did not accept writes"
heal "$L"; heal "$A"; heal "$B"
sleep 8
tool check "$BOOTS" t5- || fail "catalogs differ after healing"
for n in 1 2 3; do
  topics_on "$n" t5- | grep -q '^t5-minority ' && fail "node $n has the minority's topic"
  topics_on "$n" t5- | grep -q '^t5-majority 3$' || fail "node $n lacks the majority's topic"
done
finish "T5 partition: minority refuses, majority continues, heal converges"
