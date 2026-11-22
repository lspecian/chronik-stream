#!/bin/bash
# Consumer groups in a cluster are coordinated by one broker, with offsets
# replicated in the catalog.
#
# Before: every broker answered FindCoordinator with itself and kept offsets
# locally, so members of one group reaching different brokers each consumed
# every partition, and a group's offsets depended on the broker asked.
#
#   1. Three nodes; the catalog enables group offsets once all declare them.
#   2. Every broker names the same coordinator.
#   3. Two members of one group, one bootstrapping only node 2, the other only
#      node 3: they split the partitions (both consume, nothing twice) and
#      together read all 6,000 records.
#   4. Committed offsets read the same through every broker.
#   5. The coordinator is killed: another broker coordinates, and a member
#      resumes exactly at the committed offsets (only the 1,000 new records).
#
# Usage: group_coordination.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
source "$(dirname "$0")/../catalog/lib.sh"
T=gc_topic; G=gc_group; N=6000
OUT="$DIR/data/gc-$LABEL"
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
coordinator_via() { # port -> coordinator node id as that broker reports it
  "$K"/kafka-consumer-groups.sh --bootstrap-server "localhost:$1" --describe --group $G --state 2>/dev/null \
    | awk -v g=$G 'NR>1 && $1 == g' | grep -oE '\([0-9]+\)' | tr -d '()' | head -1
}
committed_via() { # port -> total committed offset of the group on the topic
  "$K"/kafka-consumer-groups.sh --bootstrap-server "localhost:$1" --describe --group $G 2>/dev/null \
    | awk -v t=$T '$2 == t && $4 ~ /^[0-9]+$/ { s += $4 } END { print s+0 }'
}
member() { # port outfile
  kcat -u -C -b "localhost:$1" -G $G $T -X auto.offset.reset=earliest -X enable.auto.commit=true \
    -X auto.commit.interval.ms=300 -d cgrp -q -f '%s\n' > "$2" 2>"$2.err" &
  echo $!
}

echo "=== $LABEL ($CHRONIK_BIN)"
cluster_start || { echo "FAIL: cluster did not start"; exit 1; }
trap 'kill $m1 $m2 $m3 2>/dev/null; cluster_stop' EXIT
m1=; m2=; m3=
rm -rf "$OUT"; mkdir -p "$OUT"
enabled=0
for _ in $(seq 60); do
  grep -q 'catalog consumer-group offsets enabled' "$DIR"/logs/node*.log && { enabled=1; break; }
  sleep 1
done
expect "catalog group offsets enabled" "$enabled" "1"
"$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic $T --partitions 6 --replication-factor 3 >/dev/null 2>&1
sleep 3

m1=$(member 9093 "$OUT/m1")
m2=$(member 9094 "$OUT/m2")
sleep 15 # both join and the group settles
c1=$(coordinator_via 9092); c2=$(coordinator_via 9093); c3=$(coordinator_via 9094)
echo "  coordinator as reported by nodes 1/2/3: $c1/$c2/$c3"
members=$("$K"/kafka-consumer-groups.sh --bootstrap-server localhost:9092 --describe --group $G --members 2>/dev/null | awk -v g=$G '$1 == g { print $NF }' | sort | tr '\n' ' ')
expect "two members, three partitions each" "$members" "3 3 "
expect "every broker names the same coordinator" "$( [ -n "$c1" ] && [ "$c1" = "$c2" ] && [ "$c2" = "$c3" ] && echo yes || echo no)" "yes"

seq 1 $N | awk '{ printf "%d|{\"n\":%d}\n", $1, $1 }' | kcat -P -K "|" -b "$BOOTS" -t $T -X acks=all 2>/dev/null
for _ in $(seq 60); do
  [ $(( $(wc -l < "$OUT/m1") + $(wc -l < "$OUT/m2") )) -ge $N ] && break
  sleep 1
done
sleep 3
kill -INT $m1 $m2 2>/dev/null; wait $m1 $m2 2>/dev/null; m1=; m2=
a=$(wc -l < "$OUT/m1"); b=$(wc -l < "$OUT/m2")
echo "  member via node 2 read $a, member via node 3 read $b"
expect "both members consumed (the partitions were split)" "$( [ "$a" -gt 0 ] && [ "$b" -gt 0 ] && echo yes || echo no)" "yes"
expect "records read in total" "$((a + b))" "$N"
expect "records read twice" "$(cat "$OUT/m1" "$OUT/m2" | sort | uniq -d | wc -l)" "0"
sleep 2
expect "committed offsets via node 1" "$(committed_via 9092)" "$N"
expect "committed offsets via node 2" "$(committed_via 9093)" "$N"
expect "committed offsets via node 3" "$(committed_via 9094)" "$N"

echo "-- the coordinator (node $c1) is killed"
node_kill "$c1"
survivor=9092; [ "$c1" = 1 ] && survivor=9093
survivors="localhost:9092,localhost:9093,localhost:9094"
sleep 8
seq $((N + 1)) $((N + 1000)) | awk '{ printf "%d|{\"n\":%d}\n", $1, $1 }' | kcat -P -K "|" -b "$survivors" -t $T -X acks=all 2>/dev/null
m3=$(member $survivor "$OUT/m3")
for _ in $(seq 60); do [ "$(wc -l < "$OUT/m3")" -ge 1000 ] && break; sleep 1; done
sleep 3
kill -INT $m3 2>/dev/null; wait $m3 2>/dev/null; m3=
new_c=$(coordinator_via $survivor)
echo "  new coordinator: node $new_c"
expect "another broker coordinates" "$( [ -n "$new_c" ] && [ "$new_c" != "$c1" ] && echo yes || echo no)" "yes"
expect "resumed at the committed offsets (only the new records)" "$(wc -l < "$OUT/m3")" "1000"
expect "only records produced after the commit" "$(grep -oE '"n":[0-9]+' "$OUT/m3" | cut -d: -f2 | awk -v n=$N '$1 <= n { bad++ } END { print bad+0 }')" "0"
node_start "$c1" || echo "  (node $c1 did not restart)"
expect "panics" "$(panics)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
