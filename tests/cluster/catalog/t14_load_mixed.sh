#!/usr/bin/env bash
# T14: mixed load — sustained acks=all produce + consumers, a catalog storm
# (topic creates and deletes) and node kills (the catalog leader, then a data
# node), all at once.
# PASS: every acknowledged record is readable afterwards; the catalog ends
# identical on every node with no leaderless partition; nothing panics.
# Reports produce latency under load against a quiet baseline.
source "$(dirname "$0")/lib.sh"
RATE="${T14_RATE:-1200}"; SECS="${T14_SECS:-150}"; STORM="${T14_STORM:-1000}"
N=$((RATE * SECS))
work="$DIR/data/t14"
cluster_start || { fail "cluster did not start"; finish T14; }
rm -rf "$work"; mkdir -p "$work"
for t in load-a load-b; do
  "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic "$t" --partitions 12 --replication-factor 3 >/dev/null 2>&1
done
"$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic load-lat --partitions 3 --replication-factor 3 >/dev/null 2>&1
lat() { "$K/kafka-producer-perf-test.sh" --topic load-lat --num-records "$1" --record-size 200 --throughput 500 \
  --producer-props bootstrap.servers="$BOOTS" acks=all 2>/dev/null | tail -1; }
say "quiet baseline: $(lat 15000)"

# Load. Client pids are collected so the end waits for them, not for brokers.
clients=""
for t in load-a load-b; do
  "$K/kafka-verifiable-producer.sh" --bootstrap-server "$BOOTS" --topic "$t" --max-messages "$N" --throughput "$RATE" \
    --acks -1 --producer.config /dev/null > "$work/$t.prod" 2>/dev/null & clients="$clients $!"
done
lat $((500 * SECS)) > "$work/lat.out" & clients="$clients $!"
timeout $((SECS + 60)) "$K/kafka-console-consumer.sh" --bootstrap-server "$BOOTS" --topic load-a --group t14-live >/dev/null 2>&1 & clients="$clients $!"

# Catalog storm: creates, then deletes of half of them.
( tool burst "$BOOTS" storm- "$STORM" 3 "$work/storm-acked.txt" > "$work/storm.out"
  for i in $(seq 0 2 $((STORM - 1))); do echo "storm-$i"; done | xargs -P 8 -I{} "$K/kafka-topics.sh" --bootstrap-server "$BOOTS" --delete --topic {} >/dev/null 2>&1
) &
storm=$!

sleep 20
L=$(current_leader); say "t+20s: killing catalog leader node $L"; node_kill "$L"
sleep 30; say "t+50s: restarting node $L"; node_start "$L"
sleep 20
D=$(( (L % 3) + 1 )); [ "$D" = "$(current_leader)" ] && D=$(( (D % 3) + 1 ))
say "t+70s: killing data node $D"; node_kill "$D"
sleep 30; say "t+100s: restarting node $D"; node_start "$D"
wait "$storm"
# Not a bare `wait`: it would also wait for the brokers node_start launched.
wait $clients
say "under load:     $(cat "$work/lat.out")"
cat "$work/storm.out"

# Deletes issued during a kill may have been refused; repeat them once settled.
for i in $(seq 0 2 $((STORM - 1))); do echo "storm-$i"; done | xargs -P 8 -I{} "$K/kafka-topics.sh" --bootstrap-server "$BOOTS" --delete --topic {} >/dev/null 2>&1
sleep 15
for t in load-a load-b; do
  acked=$(grep -c '"producer_send_success"' "$work/$t.prod")
  failed=$(grep -c '"producer_send_error"' "$work/$t.prod")
  end=$("$K/kafka-get-offsets.sh" --bootstrap-server "$BOOTS" --topic "$t" 2>/dev/null | awk -F: '{s+=$3} END{print s+0}')
  # Every acked value (0..N-1) must be readable; count distinct values read.
  got=$(timeout 180 "$K/kafka-console-consumer.sh" --bootstrap-server "$BOOTS" --topic "$t" --group "t14-check-$t" \
        --from-beginning --timeout-ms 30000 2>/dev/null | sort -u | wc -l)
  acked_distinct=$(grep '"producer_send_success"' "$work/$t.prod" | grep -o '"value":"[0-9]*"' | sort -u | wc -l)
  say "$t: sent $N, acked $acked (distinct $acked_distinct), errors $failed, end offsets $end, distinct read $got"
  [ "$got" -ge "$acked_distinct" ] || fail "$t: read $got distinct of $acked_distinct acked"
  [ "$acked_distinct" -gt $((N * 9 / 10)) ] || fail "$t: only $acked_distinct of $N acked"
done
tool check "$BOOTS" storm- || fail "catalog differs or has leaderless partitions after the storm"
remaining=$(topics_on 1 storm- | wc -l)
say "storm topics remaining: $remaining (expected $((STORM / 2)))"
[ "$remaining" = $((STORM / 2)) ] || fail "storm left $remaining topics, expected $((STORM / 2))"
finish "T14 mixed load: no acked record lost, catalog consistent"
