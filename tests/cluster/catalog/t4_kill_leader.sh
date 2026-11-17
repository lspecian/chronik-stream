#!/usr/bin/env bash
# T4: kill the catalog leader in the middle of a burst of creates.
# PASS: no acknowledged create is lost; no create is half-applied (every
# topic that exists has a leader for every partition, on every node); the
# new leader serves writes within the election bound.
source "$(dirname "$0")/lib.sh"
cluster_start || { fail "cluster did not start"; finish T4; }
ack="$DIR/data/t4-acked.txt"; rm -f "$ack"
leader=$(current_leader)
say "catalog leader: node $leader"
tool burst "$BOOTS" t4- 400 3 "$ack" > "$DIR/data/t4-burst.out" &
burst=$!
sleep 4
killed_at=$(date +%s.%N)
node_kill "$leader"
say "killed node $leader after $(wc -l < "$ack") acked creates"
new=""
for _ in $(seq 1 80); do
  new=$(current_leader); [ -n "$new" ] && [ "$new" != "$leader" ] && break; sleep 0.25
done
[ -n "$new" ] && [ "$new" != "$leader" ] || fail "no new leader elected"
elected=$(grep 'became leader at term' "$DIR/logs/node$new.log" | tail -1 | awk '{print $1}')
say "new leader: node $new (log time $elected)"
wait "$burst"
cat "$DIR/data/t4-burst.out"
grep -q 'gave_up=0' "$DIR/data/t4-burst.out" || fail "the burst gave up on some creates"
node_start "$leader" || fail "killed node did not restart"
sleep 5
tool check "$BOOTS" t4- "$ack" || fail "acked creates lost or catalogs differ"
finish "T4 leader killed mid-burst: nothing acked lost, nothing half-applied"
