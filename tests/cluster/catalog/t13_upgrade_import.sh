#!/usr/bin/env bash
# T13: upgrade a running pre-catalog cluster in place.
# Build state on OLD_BIN (a release before the Raft catalog), restart the same
# data directories on CHRONIK_BIN, and check the catalog imported it.
# PASS: every topic with its partition count and config, the ACL and SCRAM
# user, and every acknowledged record survive; produce keeps working; all
# nodes hold one identical catalog.
source "$(dirname "$0")/lib.sh"
OLD="${OLD_BIN:?set OLD_BIN to a pre-catalog release binary}"
export EXTRA_ENV="CHRONIK_SASL_ENABLED=optional CHRONIK_SASL_USERS=admin:admin-secret CHRONIK_INTER_BROKER_SECRET=t13-inter-broker-secret-value CHRONIK_ACL_ENABLED=true"
NEW="$BIN"
BIN="$OLD" cluster_start || { fail "old cluster did not start"; finish T13; }
kt() { "$K/kafka-topics.sh" --bootstrap-server localhost:9092 "$@" >/dev/null 2>&1; }
kt --create --topic u-one --partitions 1 --replication-factor 3
kt --create --topic u-six --partitions 6 --replication-factor 3
kt --create --topic u-rf1 --partitions 3 --replication-factor 1
"$K/kafka-configs.sh" --bootstrap-server localhost:9092 --alter --entity-type topics --entity-name u-six --add-config retention.ms=4242424 >/dev/null 2>&1
"$K/kafka-acls.sh" --bootstrap-server localhost:9092 --add --allow-principal User:alice --operation Read --topic u-one >/dev/null 2>&1
"$K/kafka-configs.sh" --bootstrap-server localhost:9092 --alter --entity-type users --entity-name keeper \
  --add-config 'SCRAM-SHA-512=[iterations=4096,password=keeper-pw]' >/dev/null 2>&1
for t in u-one u-six u-rf1; do
  "$K/kafka-producer-perf-test.sh" --topic "$t" --num-records 3000 --record-size 100 --throughput -1 \
    --producer-props bootstrap.servers="$BOOTS" acks=all >/dev/null 2>&1
done
# Move some leadership so epochs are not all zero.
node_kill 2; sleep 45; node_start 2; sleep 20
for t in u-one u-six u-rf1; do
  "$K/kafka-producer-perf-test.sh" --topic "$t" --num-records 1000 --record-size 100 --throughput -1 \
    --producer-props bootstrap.servers="$BOOTS" acks=all >/dev/null 2>&1
done
sleep 5
before=$(for t in u-one u-six u-rf1; do "$K/kafka-get-offsets.sh" --bootstrap-server "$BOOTS" --topic "$t" 2>/dev/null; done | sort)
say "before upgrade:"; echo "$before" | sed 's/^/  /'
"$DIR/stop.sh" >/dev/null 2>&1; sleep 2

# Upgrade: same data directories, new binary.
CHRONIK_BIN="$NEW" env $(node_env) $EXTRA_ENV "$DIR/restart.sh" >/dev/null 2>&1
for n in 1 2 3; do wait_port "$(port_of $n)" || fail "node $n did not start on the new binary"; done
for _ in $(seq 1 60); do grep -q 'Catalog import complete' "$DIR/logs/node1.log" && break; sleep 1; done
for n in 1 2 3; do say "node $n: $(grep -h 'Catalog import complete\|Catalog import: no pre' "$DIR/logs/node$n.log" | tail -1 | sed 's/.*Catalog import/Catalog import/' | cut -c1-160)"; done
sleep 10
after=$(for t in u-one u-six u-rf1; do "$K/kafka-get-offsets.sh" --bootstrap-server "$BOOTS" --topic "$t" 2>/dev/null; done | sort)
say "after upgrade:"; echo "$after" | sed 's/^/  /'
[ "$before" = "$after" ] && pass "every partition's end offset survived" || fail "end offsets changed across the upgrade"
for n in 1 2 3; do
  v=$(topics_on "$n" u- | tr '\n' ' ')
  [ "$v" = "u-one 1 u-rf1 3 u-six 6 " ] || fail "node $n topics after upgrade: $v"
done
"$K/kafka-configs.sh" --bootstrap-server localhost:9093 --describe --entity-type topics --entity-name u-six 2>/dev/null | grep -q 'retention.ms=4242424' \
  && pass "topic config imported" || fail "retention.ms lost"
"$K/kafka-acls.sh" --bootstrap-server localhost:9094 --list --topic u-one 2>/dev/null | grep -q 'User:alice' && pass "ACL imported" || fail "ACL lost"
kcat -b localhost:9094 -X security.protocol=SASL_PLAINTEXT -X sasl.mechanisms=SCRAM-SHA-512 -X sasl.username=keeper \
  -X sasl.password=keeper-pw -L -t __none -m 10 >/dev/null 2>&1 && pass "SCRAM user imported" || fail "SCRAM user lost"
total=0
for t in u-one u-six u-rf1; do
  got=$(timeout 120 "$K/kafka-console-consumer.sh" --bootstrap-server "$BOOTS" --topic "$t" --group "u13-$t" --from-beginning --timeout-ms 20000 2>/dev/null | wc -l)
  total=$((total + got))
  [ "$got" -ge 4000 ] || fail "$t: consumed $got of 4000 after upgrade"
done
say "consumed after upgrade: $total"
"$K/kafka-producer-perf-test.sh" --topic u-six --num-records 2000 --record-size 100 --throughput -1 \
  --producer-props bootstrap.servers="$BOOTS" acks=all 2>&1 | grep -q '2000 records sent' && pass "produce works after upgrade" || fail "produce after upgrade"
tool check "$BOOTS" u- || fail "catalogs differ after upgrade"
finish "T13 in-place upgrade imports the pre-catalog metadata"
