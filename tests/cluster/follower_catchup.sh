#!/usr/bin/env bash
# A follower that misses writes catches up on its own — the last unproven claim
# in docs/ROADMAP_REPLICATION.md.
#
# RP-2.4 says "on restart, resume from local LEO → catch-up, for free", and the
# mechanism is TESTED in the sense that clusters converge in practice. But
# nothing asserted it: `regression_replication.sh` kills a replica only to watch
# ISR shrink and never brings it back, so every harness we had would pass on a
# broker where a returning follower silently held a truncated log forever.
#
#   1. produce a prefix                → all three replicas hold it on disk
#   2. SIGKILL one FOLLOWER            → a real outage, not a freeze
#   3. produce more at acks=1          → records it cannot possibly have
#   4. assert the gap is real          → it must be measurably behind
#   5. restart it, touch nothing else  → no reassignment, no operator action
#   6. assert it converges             → every record, on its own disk
#
# Step 4 is the one that makes the rest mean anything. Without it a broker that
# replicated nothing during the outage and a broker that replicated everything
# look identical at the end, and the test passes either way.
#
# SIGKILL rather than SIGSTOP: a frozen process resumes with its sockets and
# in-memory state intact, which is not what a restarted broker faces. This must
# exercise recovery from disk.
#
# Runs on shifted ports (9392-9394) so it does not collide with a server on
# 9092. Requires a release build and kcat.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/chronik-server"
DIR="$ROOT/tests/cluster"
LOGS="$DIR/logs"
PREFIX_N="${CATCHUP_PREFIX:-100}"
GAP_N="${CATCHUP_GAP:-120}"
# How long the follower stays down.
#
# The roadmap box says "5 minutes"; catch-up does not depend on the duration — a
# follower resumes from its local LEO whether that is seconds or hours stale — so
# the default keeps the suite runnable and CATCHUP_OUTAGE_SECS raises it for a
# long soak.
#
# But it must comfortably exceed the ISR liveness window, or the ISR assertions
# below are not deterministic. A replica is judged silent after
# CHRONIK_REPLICA_LAG_TIME_MAX_MS (10s) and dead after 3x that (30s). The first
# version of this test sampled ISR ~10s after the kill, printed the unshrunk
# `isr:[1,2,3]`, and looked like it had caught a bug when it had only asked too
# early.
OUTAGE_SECS="${CATCHUP_OUTAGE_SECS:-40}"
TOPIC="catchup-$$"
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

[ -x "$BIN" ] || { say "SKIP: build first (cargo build --release --bin chronik-server)"; exit 0; }
command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }

pid_of() { cat "$LOGS/alt-node$1.pid" 2>/dev/null; }

start_node() { # $1 = node id
  mkdir -p "$LOGS" "$DIR/data/alt-node$1"
  CHRONIK_UNIFIED_API_PORT=$((6391 + $1)) \
  RUST_LOG=info \
  "$BIN" start --config "$DIR/altport-node$1.toml" \
    > "$LOGS/alt-node$1.log" 2>&1 &
  echo $! > "$LOGS/alt-node$1.pid"
}

stop_all() {
  for n in 1 2 3; do
    p=$(pid_of "$n")
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

broker_of() { echo "localhost:$((9391 + $1))"; }

leader_of() { # $1 = api port to ask
  curl -s -m 5 "http://localhost:$1/admin/status" 2>/dev/null \
    | tr '{' '\n' | grep "\"topic\":\"$TOPIC\"" | grep '"partition":0,' \
    | grep -o '"leader":[0-9]*' | head -1 | tr -cd '0-9'
}

isr_of() { # $1 = api port
  curl -s -m 5 "http://localhost:$1/admin/status" 2>/dev/null \
    | tr '{' '\n' | grep "\"topic\":\"$TOPIC\"" | grep '"partition":0,' \
    | grep -o '"isr":\[[^]]*\]' | head -1
}

# Explicit broker + hard timeouts on every kcat call: pointing a client at a
# broker that cannot answer costs the whole run otherwise.
KCAT_TIMEOUTS=(-X socket.timeout.ms=4000 -X metadata.request.timeout.ms=4000)

produce() { # $1=first $2=last $3=acks $4=tag $5=brokers
  seq "$1" "$2" | sed "s/^/$4-/" \
    | timeout 60 kcat -P -b "$5" -t "$TOPIC" -p 0 -X request.required.acks="$3" \
        "${KCAT_TIMEOUTS[@]}" 2>/dev/null
}

# Counts DISTINCT records on one node's disk, not marker occurrences: a record's
# payload appears more than once in the WAL (the canonical batch and the
# preserved wire bytes both carry it), so a raw grep -c over-counts by ~3x and
# would satisfy a threshold while the replica held a third of the data.
records_on_node() { # $1 = node, $2 = tag
  grep -aoE "$2-[0-9]+" "$DIR/data/alt-node$1/wal/$TOPIC"/0/*.log 2>/dev/null \
    | sort -u | wc -l
}

wait_for_all_replicas() { # $1 = tag, $2 = expected count
  for _ in $(seq 1 60); do
    behind=""
    for n in 1 2 3; do
      have=$(records_on_node "$n" "$1")
      [ "${have:-0}" -lt "$2" ] && behind="$behind node$n=${have:-0}"
    done
    [ -z "$behind" ] && return 0
    sleep 1
  done
  say "   still behind after 60s:$behind"
  return 1
}

say "== follower catch-up test: $TOPIC =="
rm -rf "$DIR/data/alt-node"{1,2,3}
mkdir -p "$LOGS"

for n in 1 2 3; do start_node "$n"; done
for p in 9392 9393 9394; do
  wait_for_port "$p" || { fail "node on port $p never came up"; exit 1; }
done
sleep 12

# ---------------------------------------------------------------- 1. prefix
say "-- producing $PREFIX_N prefix record(s) at acks=all"
produce 1 "$PREFIX_N" -1 pre "$(broker_of 1),$(broker_of 2),$(broker_of 3)"

wait_for_all_replicas pre "$PREFIX_N" \
  || { fail "replicas never converged on the prefix — nothing after this is meaningful"; exit 1; }
say "   all three replicas hold the prefix on disk"

LEADER=$(leader_of 6392)
[ -n "$LEADER" ] || { fail "could not determine the leader of $TOPIC-0"; exit 1; }

# Any node that is not the leader. Killing the leader would test failover
# (RP-5), which local_divergence.sh already covers; this is about a follower.
VICTIM=""
for n in 1 2 3; do [ "$n" != "$LEADER" ] && VICTIM="$n" && break; done
say "   leader=node$LEADER, taking down follower node$VICTIM"

# ---------------------------------------------------------- 2. kill follower
kill -9 "$(pid_of "$VICTIM")" 2>/dev/null
rm -f "$LOGS/alt-node$VICTIM.pid"
sleep 2

# ------------------------------------------------- 3. produce into the gap
#
# acks=1, not acks=all: with a replica down and min_insync_replicas=2 the
# quorum is still reachable, but acks=1 is what a producer that does not want
# to wait on the missing replica actually uses, and it is the case where the
# gap is guaranteed.
say "-- producing $GAP_N record(s) at acks=1 while node$VICTIM is down"
LIVE=""
for n in 1 2 3; do [ "$n" != "$VICTIM" ] && LIVE="$LIVE$(broker_of "$n"),"; done
LIVE="${LIVE%,}"
produce 1 "$GAP_N" 1 gap "$LIVE"

# The survivors must hold the gap records before we judge anything.
for _ in $(seq 1 60); do
  ok=1
  for n in 1 2 3; do
    [ "$n" = "$VICTIM" ] && continue
    [ "$(records_on_node "$n" gap)" -lt "$GAP_N" ] && ok=0
  done
  [ "$ok" = 1 ] && break
  sleep 1
done

# ------------------------------------------------------ 4. the gap is real
missed=$(records_on_node "$VICTIM" gap)
say "   gap records on node$VICTIM's disk while down: ${missed:-0} (must be 0)"
[ "${missed:-0}" -eq 0 ] || fail "node$VICTIM somehow holds gap records while dead — the outage was not real, and convergence below proves nothing"

say "-- leaving node$VICTIM down for ${OUTAGE_SECS}s"
sleep "$OUTAGE_SECS"

# The dead replica must have left ISR by now. If it has not, `acks=all` is
# acknowledging against a set that includes a node holding none of these
# records, which is the failure mode RP-1.2 exists to prevent.
isr_down=$(isr_of $((6391 + LEADER)))
say "   ISR after ${OUTAGE_SECS}s down: ${isr_down:-<none>}"
case "$isr_down" in
  *"$VICTIM"*) fail "node$VICTIM is still in ISR after ${OUTAGE_SECS}s dead" ;;
  "")          fail "could not read ISR for $TOPIC-0" ;;
esac

# ------------------------------------------------- 5. restart, touch nothing
#
# No reassignment, no admin call, no partition move: "without operator action"
# is the property, so the only thing that happens here is the process starting.
say "-- restarting node$VICTIM"
start_node "$VICTIM"
wait_for_port "$((9391 + VICTIM))" || { fail "node$VICTIM never came back up"; exit 1; }

# ----------------------------------------------------- 6. it must converge
say "-- waiting for node$VICTIM to catch up on its own"
converged=0
for _ in $(seq 1 90); do
  have_pre=$(records_on_node "$VICTIM" pre)
  have_gap=$(records_on_node "$VICTIM" gap)
  if [ "${have_pre:-0}" -ge "$PREFIX_N" ] && [ "${have_gap:-0}" -ge "$GAP_N" ]; then
    converged=1
    break
  fi
  sleep 1
done

have_pre=$(records_on_node "$VICTIM" pre)
have_gap=$(records_on_node "$VICTIM" gap)
say "   node$VICTIM now holds prefix=${have_pre:-0}/$PREFIX_N gap=${have_gap:-0}/$GAP_N"
[ "$converged" = 1 ] || fail "node$VICTIM did not converge unattended"

# It must also be readable through the cluster, not merely present on disk —
# a replica whose bytes landed but whose watermark did not is still broken.
readable=$(timeout 60 kcat -C -b "$(broker_of "$VICTIM")" -t "$TOPIC" -o beginning -e -q \
  "${KCAT_TIMEOUTS[@]}" 2>/dev/null | grep -c -E '^(pre|gap)-')
want=$((PREFIX_N + GAP_N))
say "   records readable via node$VICTIM: ${readable:-0} / $want"
[ "${readable:-0}" -ge "$want" ] || fail "node$VICTIM holds the records but cannot serve them"

# And it must be readmitted to ISR. A replica that holds every record but is
# never counted as in-sync leaves the partition permanently under-replicated in
# the eyes of `acks=all` and of failover, which elects from ISR.
say "-- waiting for node$VICTIM to rejoin ISR"
rejoined=0
for _ in $(seq 1 60); do
  isr_back=$(isr_of $((6391 + LEADER)))
  case "$isr_back" in *"$VICTIM"*) rejoined=1; break;; esac
  sleep 1
done
say "   ISR after node$VICTIM returned: ${isr_back:-<none>}"
[ "$rejoined" = 1 ] || fail "node$VICTIM caught up but was never readmitted to ISR"

if [ "$FAIL" = 0 ]; then
  say ""
  say "== PASS: a follower that missed $GAP_N writes rejoined and converged with no operator action =="
else
  say ""
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
