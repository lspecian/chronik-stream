#!/usr/bin/env bash
# T1: explicit `--partitions 1` via node 1 while producers on nodes 2 and 3
# auto-create the same topics, with catalog delivery to node 3 delayed.
# PASS: exactly one version of each topic on every node, and every acked
# record is consumable by a group consumer.
source "$(dirname "$0")/lib.sh"
TOPICS="${T1_TOPICS:-10}"; MSGS="${T1_MSGS:-200}"
cluster_start || { fail "cluster did not start"; finish T1; }
fault 3 "delay 1500"
work="$DIR/data/t1"; rm -rf "$work"; mkdir -p "$work"
for i in $(seq 1 "$TOPICS"); do
  t="t1-$i"
  "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic "$t" --partitions 1 --replication-factor 3 > "$work/$t.create" 2>&1 &
  for p in 9093 9094; do
    "$K/kafka-verifiable-producer.sh" --bootstrap-server "localhost:$p" --topic "$t" --max-messages "$MSGS" --acks -1 \
      --producer.config /dev/null > "$work/$t.$p.prod" 2>/dev/null &
  done
done
wait
heal 3
sleep 5
acked_total=0
for i in $(seq 1 "$TOPICS"); do
  t="t1-$i"
  acked=$(cat "$work/$t".*.prod | grep -c '"producer_send_success"')
  acked_total=$((acked_total + acked))
  views=$(for n in 1 2 3; do topics_on "$n" "$t\$"; done | sort -u | wc -l)
  [ "$views" = 1 ] || fail "$t: nodes disagree: $(for n in 1 2 3; do topics_on "$n" "$t\$"; done | tr '\n' ';')"
  end=$("$K/kafka-get-offsets.sh" --bootstrap-server localhost:9092 --topic "$t" 2>/dev/null | awk -F: '{s+=$3} END{print s+0}')
  got=$(timeout 90 "$K/kafka-console-consumer.sh" --bootstrap-server localhost:9093 --topic "$t" --group "t1-g-$i" \
        --from-beginning --timeout-ms 20000 2>/dev/null | wc -l)
  say "$t: $(topics_on 1 "$t\$" | awk '{print $2}') partition(s), acked=$acked end_offsets=$end group_consumed=$got"
  [ "$end" -ge "$acked" ] || fail "$t: acked $acked but only $end in visible partitions"
  [ "$got" -ge "$acked" ] || fail "$t: group consumer read $got of $acked acked records"
done
say "acked total: $acked_total"
[ "$acked_total" -gt 0 ] || fail "nothing was acked"
finish "T1 auto-create crossover: one version per topic, every acked record consumable"
