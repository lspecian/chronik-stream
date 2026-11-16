#!/usr/bin/env bash
# T7: leader failover for 10,000 partitions (kill a node).
# PASS: every partition the dead node led has a new leader, committed and
# visible, within 2 s of the controller detecting the death.
source "$(dirname "$0")/lib.sh"
T="${T7_TOPICS:-100}"; P="${T7_PARTITIONS:-100}"
cluster_start || { fail "cluster did not start"; finish T7; }
seq 1 "$T" | xargs -P 8 -I{} "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic t7-{} \
  --partitions "$P" --replication-factor 3 >/dev/null 2>&1
sleep 15  # let ISRs be published
L=$(current_leader); victim=$(( L % 3 + 1 ))
led=$("$K/kafka-topics.sh" --bootstrap-server localhost:9092 --describe 2>/dev/null | grep -c "Topic: t7-.*Leader: $victim	")
say "$((T * P)) partitions; node $victim leads $led; catalog leader is node $L"
node_kill "$victim"
for _ in $(seq 1 240); do grep -q 'is unreachable — failed over' "$DIR/logs/node$L.log" && break; sleep 0.5; done
first=$(grep 'is unreachable — failed over' "$DIR/logs/node$L.log" | head -1 | awk '{print $1}')
sleep 3
last=$(grep 'is unreachable — failed over' "$DIR/logs/node$L.log" | tail -1 | awk '{print $1}')
moved=$(grep -c 'is unreachable — failed over' "$DIR/logs/node$L.log")
span=$(echo "$(date -d "$last" +%s.%N) - $(date -d "$first" +%s.%N)" | bc)
say "failovers: $moved, first $first, last $last, span ${span}s"
[ "$moved" -ge "$led" ] || fail "only $moved of $led partitions failed over"
awk -v s="$span" 'BEGIN{exit !(s <= 2.0)}' || fail "failover took ${span}s (> 2 s)"
left=$("$K/kafka-topics.sh" --bootstrap-server "localhost:$(port_of "$L")" --describe 2>/dev/null | grep -c "Topic: t7-.*Leader: $victim	")
[ "$left" = 0 ] || fail "$left partitions still name the dead node as leader"
finish "T7 10k-partition failover within 2 s of detection"
