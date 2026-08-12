#!/usr/bin/env bash
# RP-3.3 end-to-end on three local processes: prove a divergent tail is cut.
#
# The Kubernetes version of this test failed six times, every time in the
# harness rather than the product — quoting mangled in transit, a NetworkPolicy
# that cannot partition an already-connected cluster, an isolated leader that
# then served the client only partially, a reader returning 272 records or 0 for
# the same command. Local processes remove that entire layer and add the one
# thing k8s would not give: **SIGSTOP**.
#
# Freezing a process is a perfect, instant, reversible partition. A stopped
# follower cannot fetch, cannot answer, and cannot campaign — no CNI, no
# conntrack, no established-connection loophole.
#
#   1. produce a common prefix          → all three replicas agree
#   2. SIGSTOP both followers           → they physically cannot fetch
#   3. produce at acks=1 to the leader  → records only the leader can have
#   4. SIGKILL the leader, SIGCONT the followers → they elect without those records
#   5. produce at acks=all to the new leader → different records, same offsets
#   6. restart the old leader           → it must discard its tail
#
# PASS: every record the new leader committed is readable, and none of the
# leader-only records survive anywhere.
#
# Runs on shifted ports (9392-9394) so it does not collide with a server already
# on 9092. Requires a release build and kcat.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/chronik-server"
DIR="$ROOT/tests/cluster"
LOGS="$DIR/logs"
BOOT="localhost:9392,localhost:9393,localhost:9394"
PREFIX_N="${DIVERGE_PREFIX:-100}"
ORPHAN_N="${DIVERGE_ORPHAN:-40}"
WINNER_N="${DIVERGE_WINNER:-40}"
TOPIC="localdiv-$$"
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

[ -x "$BIN" ] || { say "SKIP: build first (cargo build --release --bin chronik-server)"; exit 0; }
command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }

pid_of() { cat "$LOGS/alt-node$1.pid" 2>/dev/null; }

start_node() { # $1 = node id
  mkdir -p "$LOGS" "$DIR/data/alt-node$1"
  CHRONIK_REPLICATION_MODE=pull \
  CHRONIK_UNIFIED_API_PORT=$((6391 + $1)) \
  RUST_LOG=info \
  "$BIN" start --config "$DIR/altport-node$1.toml" \
    > "$LOGS/alt-node$1.log" 2>&1 &
  echo $! > "$LOGS/alt-node$1.pid"
}

stop_all() {
  for n in 1 2 3; do
    p=$(pid_of "$n")
    [ -n "$p" ] && kill -CONT "$p" 2>/dev/null
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
    rm -f "$LOGS/alt-node$n.pid"
  done
}
trap stop_all EXIT

wait_for_port() { # $1 = port
  for _ in $(seq 1 60); do
    (exec 3<>/dev/tcp/127.0.0.1/$1) 2>/dev/null && { exec 3<&- ; return 0; }
    sleep 1
  done
  return 1
}

leader_of() { # $1 = api port to ask
  curl -s -m 5 "http://localhost:$1/admin/status" 2>/dev/null \
    | tr '{' '\n' | grep "\"topic\":\"$TOPIC\"" | grep '"partition":0,' \
    | grep -o '"leader":[0-9]*' | head -1 | tr -cd '0-9'
}

produce() { # $1=first $2=last $3=acks $4=tag
  seq "$1" "$2" | sed "s/^/$4-/" \
    | kcat -P -b "$BOOT" -t "$TOPIC" -X request.required.acks="$3" 2>/dev/null
}

count_tag() { # $1 = tag
  kcat -C -b "$BOOT" -t "$TOPIC" -o beginning -e -q 2>/dev/null | grep -c "^$1-"
}

say "== RP-3.3 local divergence test: $TOPIC =="
rm -rf "$DIR/data/alt-node"{1,2,3}
mkdir -p "$LOGS"

for n in 1 2 3; do start_node "$n"; done
for p in 9392 9393 9394; do
  wait_for_port "$p" || { fail "node on port $p never came up"; exit 1; }
done
sleep 12
say "-- three nodes up on 9392-9394"

# 1. Common prefix, acknowledged by the full ISR.
produce 1 "$PREFIX_N" -1 prefix
sleep 5
seen=$(count_tag prefix)
if [ "${seen:-0}" -lt "$PREFIX_N" ]; then
  fail "setup did not take: only ${seen:-0} of $PREFIX_N prefix records readable"
  exit 1
fi
say "-- prefix confirmed: $seen record(s)"

OLD=$(leader_of 6392)
[ -z "$OLD" ] && OLD=$(leader_of 6393)
[ -z "$OLD" ] && { fail "no leader for $TOPIC"; exit 1; }
say "-- leader is node $OLD"

# 2. Freeze the followers. A stopped process cannot fetch, answer or campaign.
say "-- freezing the followers (SIGSTOP)"
for n in 1 2 3; do
  [ "$n" = "$OLD" ] && continue
  kill -STOP "$(pid_of "$n")" 2>/dev/null
done
sleep 2

# 3. Records only the leader can have.
say "-- writing $ORPHAN_N record(s) only node $OLD can have (acks=1)"
produce $((PREFIX_N + 1)) $((PREFIX_N + ORPHAN_N)) 1 orphan
sleep 3
orphans_here=$(count_tag orphan)
say "   leader holds ${orphans_here:-0} orphan record(s)"
[ "${orphans_here:-0}" -ge 1 ] || { fail "no orphans landed — no divergence to test"; exit 1; }

# 4. Kill the leader, thaw the followers. They hold quorum and must elect.
say "-- killing node $OLD and thawing the followers"
kill -9 "$(pid_of "$OLD")" 2>/dev/null
rm -f "$LOGS/alt-node$OLD.pid"
for n in 1 2 3; do
  [ "$n" = "$OLD" ] && continue
  kill -CONT "$(pid_of "$n")" 2>/dev/null
done

OBS=6392; [ "$OLD" = "1" ] && OBS=6393
NEW=""
deadline=$((SECONDS + 180))
while [ "$SECONDS" -lt "$deadline" ]; do
  sleep 5
  now=$(leader_of "$OBS")
  if [ -n "$now" ] && [ "$now" != "$OLD" ]; then NEW="$now"; break; fi
done
[ -n "$NEW" ] || { fail "leadership never moved off node $OLD"; exit 1; }
say "-- leadership moved to node $NEW"

# 5. Different records over the same offsets.
produce $((PREFIX_N + 1)) $((PREFIX_N + WINNER_N)) -1 winner
sleep 5
say "-- wrote $WINNER_N committed record(s) over those offsets"

# 6. The old leader returns and must discard its tail.
say "-- restarting node $OLD"
start_node "$OLD"
wait_for_port $((9391 + OLD)) || fail "node $OLD did not come back"
sleep 30

say ""
say "-- reconciliation on node $OLD:"
grep -aE "truncated to|diverged from the leader|log is a prefix|not truncating|Reconciling" \
  "$LOGS/alt-node$OLD.log" 2>/dev/null | tail -5 | sed 's/^/     /'

truncated=0
grep -qaE "truncated to|diverged from the leader" "$LOGS/alt-node$OLD.log" 2>/dev/null && truncated=1

winners=$(count_tag winner)
orphans=$(count_tag orphan)
say ""
say "   committed (winner) records readable: ${winners:-0} / $WINNER_N"
say "   uncommitted (orphan) records readable: ${orphans:-0} (must be 0)"

[ "$truncated" -eq 1 ] || fail "node $OLD never truncated — it still holds records the cluster never committed"
[ "${winners:-0}" -ge "$WINNER_N" ] || fail "only ${winners:-0} of $WINNER_N committed records survived — truncation took too much"
[ "${orphans:-0}" -eq 0 ] || fail "${orphans} record(s) the cluster never committed are still readable"

say ""
if [ "$FAIL" -eq 0 ]; then
  say "== PASS: the divergent tail was discarded and every committed record survived =="
else
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
