#!/usr/bin/env bash
# T3: create, delete, alter, ACL and SCRAM changes while node 3 is down.
# PASS: node 3 converges with no anti-entropy, and a SCRAM user revoked while
# it was down is refused by node 3.
source "$(dirname "$0")/lib.sh"
export EXTRA_ENV="CHRONIK_SASL_ENABLED=optional CHRONIK_SASL_USERS=admin:admin-secret CHRONIK_INTER_BROKER_SECRET=t3-inter-broker-secret-value"
cluster_start || { fail "cluster did not start"; finish T3; }
kt() { "$K/kafka-topics.sh" --bootstrap-server localhost:9092 "$@" >/dev/null 2>&1; }
scram_ok() { # $1 port $2 user $3 password
  kcat -b "localhost:$1" -X security.protocol=SASL_PLAINTEXT -X sasl.mechanisms=SCRAM-SHA-512 \
    -X sasl.username="$2" -X sasl.password="$3" -L -t __none -m 10 >/dev/null 2>&1
}
kt --create --topic t3-old --partitions 1 --replication-factor 3
kt --create --topic t3-keep --partitions 1 --replication-factor 3
"$K/kafka-configs.sh" --bootstrap-server localhost:9092 --alter --entity-type users --entity-name revokee \
  --add-config 'SCRAM-SHA-512=[iterations=4096,password=revokee-pw]' >/dev/null 2>&1
sleep 2
scram_ok 9094 revokee revokee-pw && pass "revokee authenticates on node 3 before" || fail "revokee could not authenticate on node 3 before revocation"
node_kill 3
kt --create --topic t3-new --partitions 2 --replication-factor 3
kt --delete --topic t3-old
"$K/kafka-configs.sh" --bootstrap-server localhost:9092 --alter --entity-type topics --entity-name t3-keep --add-config retention.ms=123456 >/dev/null 2>&1
"$K/kafka-acls.sh" --bootstrap-server localhost:9092 --add --allow-principal User:alice --operation Read --topic t3-keep >/dev/null 2>&1
"$K/kafka-configs.sh" --bootstrap-server localhost:9092 --alter --entity-type users --entity-name revokee --delete-config SCRAM-SHA-512 >/dev/null 2>&1
node_start 3 || fail "node 3 did not restart"
sleep 8
topics_on 3 t3- | grep -q '^t3-new 2$' && pass "node 3 has the topic created while it was down" || fail "node 3 lacks t3-new"
topics_on 3 t3- | grep -q '^t3-old ' && fail "node 3 still has the deleted t3-old" || pass "node 3 dropped the deleted topic"
"$K/kafka-configs.sh" --bootstrap-server localhost:9094 --describe --entity-type topics --entity-name t3-keep 2>/dev/null | grep -q 'retention.ms=123456' \
  && pass "node 3 sees the altered config" || fail "node 3 does not see retention.ms=123456"
"$K/kafka-acls.sh" --bootstrap-server localhost:9094 --list --topic t3-keep 2>/dev/null | grep -q 'User:alice' \
  && pass "node 3 has the ACL" || fail "node 3 lacks the ACL"
scram_ok 9094 revokee revokee-pw && fail "node 3 still accepts the revoked SCRAM user" || pass "node 3 refuses the revoked SCRAM user"
scram_ok 9094 admin admin-secret || say "(admin is a CHRONIK_SASL_USERS user, PLAIN/SCRAM via env)"
tool check "$BOOTS" t3- || fail "catalogs differ"
finish "T3 offline node converges from the log; revoked SCRAM user refused"
