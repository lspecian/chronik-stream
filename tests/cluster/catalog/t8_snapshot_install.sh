#!/usr/bin/env bash
# T8: wipe node 3's data and restart it.
# PASS: node 3 rebuilds its catalog from a snapshot plus the log tail.
source "$(dirname "$0")/lib.sh"
export EXTRA_ENV="CHRONIK_CATALOG_SNAPSHOT_EVERY=50"
cluster_start || { fail "cluster did not start"; finish T8; }
seq 1 300 | xargs -P 8 -I{} "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic t8-{} \
  --partitions 1 --replication-factor 3 >/dev/null 2>&1
node_kill 3
rm -rf "$DIR/data/node3"; mkdir -p "$DIR/data/node3"
seq 301 320 | xargs -P 4 -I{} "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic t8-{} \
  --partitions 1 --replication-factor 3 >/dev/null 2>&1
node_start 3 || fail "node 3 did not restart"
sleep 8
grep -q 'installed snapshot at index' "$DIR/logs/node3.log" && pass "node 3 installed a snapshot" || fail "node 3 did not install a snapshot"
[ "$(topics_on 3 t8- | wc -l)" = 320 ] || fail "node 3 has $(topics_on 3 t8- | wc -l) of 320 topics"
tool check "$BOOTS" t8- || fail "catalogs differ"
finish "T8 wiped node rebuilds from snapshot + tail"
