#!/usr/bin/env bash
# T8: wipe node 3's data.
# A voter that lost its log must not rejoin as if nothing happened (raft
# safety): it stops its catalog with the remedy. Following the remedy —
# remove it, add it back, start it with CHRONIK_CLUSTER_JOIN=true — it
# rebuilds its catalog from a snapshot plus the log tail.
# PASS: the wiped node stops cleanly (no panic) and names the remedy; after the
# procedure it installs a snapshot and holds the full, identical catalog.
source "$(dirname "$0")/lib.sh"
KEY="t8-admin-key"
export EXTRA_ENV="CHRONIK_CATALOG_SNAPSHOT_EVERY=50 CHRONIK_ADMIN_API_KEY=$KEY"
admin() { # $1 node $2 path $3 json
  curl -s -X POST "http://localhost:$((6091 + $1))$2" -H "X-API-Key: $KEY" -H 'Content-Type: application/json' -d "$3"
}
cluster_start || { fail "cluster did not start"; finish T8; }
seq 1 300 | xargs -P 8 -I{} "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic t8-{} \
  --partitions 1 --replication-factor 3 >/dev/null 2>&1
node_kill 3
rm -rf "$DIR/data/node3"; mkdir -p "$DIR/data/node3"
seq 301 320 | xargs -P 4 -I{} "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic t8-{} \
  --partitions 1 --replication-factor 3 >/dev/null 2>&1

# 1. Restarted as if nothing happened: the catalog refuses, with the remedy.
: > "$DIR/logs/node3.log"
node_start 3
for _ in $(seq 1 40); do grep -q 'lost raft state' "$DIR/logs/node3.log" && break; sleep 0.5; done
grep -q 'lost raft state' "$DIR/logs/node3.log" && pass "wiped node stopped its catalog and named the remedy" \
  || fail "wiped node did not report its lost log"
node_kill 3

# 2. The remedy.
r=$(admin 1 /admin/remove-node '{"node_id": 3, "force": true}'); say "remove-node: $r"
echo "$r" | grep -q '"success":true' || fail "remove-node failed"
r=$(admin 1 /admin/add-node '{"node_id": 3, "kafka_addr": "localhost:9094", "wal_addr": "localhost:9293", "raft_addr": "localhost:5003"}'); say "add-node: $r"
echo "$r" | grep -q '"success":true' || fail "add-node failed"
rm -rf "$DIR/data/node3"; mkdir -p "$DIR/data/node3"
EXTRA_ENV="$EXTRA_ENV CHRONIK_CLUSTER_JOIN=true" node_start 3 || fail "node 3 did not restart"
for _ in $(seq 1 60); do [ "$(topics_on 3 t8- | wc -l)" = 320 ] && break; sleep 1; done
grep -q 'installed snapshot at index' "$DIR/logs/node3.log" && pass "node 3 installed a snapshot" || fail "node 3 did not install a snapshot"
[ "$(topics_on 3 t8- | wc -l)" = 320 ] || fail "node 3 has $(topics_on 3 t8- | wc -l) of 320 topics"
tool check "$BOOTS" t8- || fail "catalogs differ"
finish "T8 wiped node: stops safely, then rebuilds from snapshot after remove/re-add"
