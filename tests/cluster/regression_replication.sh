#!/usr/bin/env bash
# Replication placement regression: produced data must physically reach EVERY
# replica, at every acks level.
#
# Guards the bug fixed in #29 (commit 0b4e871): `produce_to_partition` returned
# from the async-response path before reaching the WAL replication hook, and
# that path is taken whenever `acks != 0`. So `acks=1` and `acks=all` replicated
# nothing while `acks=0` replicated normally — the durability contract exactly
# inverted. It survived ~9 months across a dozen releases because:
#   - ISR is reported from the partition assignment, so /admin/status showed
#     isr:[1,2,3] with zero follower copies on disk;
#   - every performance benchmark used acks=0, the one mode that worked.
#
# Reproduced on v2.2.25 too, so it was never a recent regression. The ONLY check
# that catches it is looking at what is physically on each node. Metadata lies.
#
# Usage:
#   ./tests/cluster/regression_replication.sh                 # local tests/cluster
#   REPL_MODE=k8s REPL_NS=chronik-perf REPL_PODS=chronik-thunderbird \
#     REPL_KUBECTL="ssh user@host sudo microk8s kubectl" \
#     ./tests/cluster/regression_replication.sh
#
# Env:
#   REPL_MODE         local (default) | k8s
#   REPL_RECORDS      records produced per acks mode (default 600)
#   REPL_SETTLE_SECS  max wait for replication to converge (default 60)
#   REPL_NS           k8s namespace                     (k8s mode)
#   REPL_PODS         broker pod name prefix, 1..3      (k8s mode)
#   REPL_KUBECTL      kubectl invocation, e.g. an ssh wrapper (default: kubectl)
#   REPL_CLIENT       client pod name (k8s mode, created if absent)
#
# Requires: local mode — kcat + target/release/chronik-server;
#           k8s mode  — a running 3-node ChronikCluster reachable via REPL_KUBECTL.
#
# PASS criteria, for each of acks=0, acks=1, acks=all:
#   - the topic is RF=3
#   - EVERY node's WAL holds EVERY partition that any node holds
#   - consumed record count equals produced count (acks=0 exempt: no guarantee)
#
# Plus, in k8s mode only (both need a replica held genuinely down, which takes
# cordoning the node — see the notes at each):
#   RP-0.3  ISR shrinks when a replica dies
#   RP-0.4  a replica that loses leadership converges after rejoining, without
#           losing anything acknowledged (the RP-3.3 truncation path)
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${REPL_MODE:-local}"
RECORDS="${REPL_RECORDS:-600}"
SETTLE="${REPL_SETTLE_SECS:-60}"
PARTITIONS=3
FAIL=0

KUBECTL="${REPL_KUBECTL:-kubectl}"
NS="${REPL_NS:-}"
PODS="${REPL_PODS:-}"
CLIENT="${REPL_CLIENT:-replcheck}"

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

# ---------------------------------------------------------------- local mode --
local_setup() {
  command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }
  [ -x "$SCRIPT_DIR/../../target/release/chronik-server" ] \
    || { say "SKIP: target/release/chronik-server not built"; exit 0; }
  "$SCRIPT_DIR/stop.sh" >/dev/null 2>&1
  "$SCRIPT_DIR/start.sh" >/dev/null 2>&1 &
  sleep 25
  local alive; alive=$(pgrep -cf 'chronik-server.*node[123].toml')
  [ "$alive" -eq 3 ] || { say "SKIP: cluster failed to start ($alive/3) — is :9092 in use?"; exit 1; }
}
local_teardown() { "$SCRIPT_DIR/stop.sh" >/dev/null 2>&1; }
local_placement() { ls "$SCRIPT_DIR/data/node$1/wal/$2" 2>/dev/null | sort -n | tr '\n' ' ' | sed 's/ $//'; }
local_produce() { # $1=topic $2=acks — explicit partitions so all three carry data
  local p
  for p in $(seq 0 $((PARTITIONS - 1))); do
    seq 1 $((RECORDS / PARTITIONS)) | sed "s/^/acks=$2 p=$p rec /" \
      | kcat -b localhost:9092 -t "$1" -p "$p" -P -X acks="$2" 2>/dev/null
  done
}
local_consume() { timeout 60 kcat -b localhost:9092 -t "$1" -C -e -q 2>/dev/null | wc -l; }
# Replication factor, or 0 if it cannot be determined.
local_rf() {
  local commas
  commas=$(timeout 20 kcat -b localhost:9092 -L -t "$1" 2>/dev/null \
    | grep -oE 'replicas: [0-9,]+' | head -1 | awk -F: '{print $2}' | tr -cd ',' | wc -c)
  [ "$commas" -gt 0 ] && echo $((commas + 1)) || echo 0
}

# ------------------------------------------------------------------ k8s mode --
# Run a shell script inside a pod.
#
# REPL_KUBECTL is frequently an ssh wrapper (it is the documented way to reach a
# MicroK8s lab). ssh flattens its arguments into one string and hands them to
# the remote login shell, which re-parses them — so a payload written as
#   kubectl exec pod -- bash -c "a | b"
# arrives as `bash -c a` with `| b` running on the SSH HOST instead of in the
# pod. Everything after the first metacharacter silently executes somewhere
# else, and the command appears to do nothing.
#
# That is not hypothetical: it made every produce in this suite a no-op, and the
# suite then reported "no partitions on any node" — a broken harness perfectly
# imitating broken replication, on the one test whose whole job is to tell those
# apart. QUOTE_LEVEL is probed at setup rather than guessed from the string
# "ssh", so it is correct for any wrapper.
sh_quote() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

pod_sh() { # $1=pod  $2=script
  local payload="$2"
  [ "${QUOTE_LEVEL:-0}" -ge 1 ] && payload=$(sh_quote "$2")
  $KUBECTL exec -n "$NS" "$1" -- bash -c "$payload"
}

# Determine how many levels of shell quoting the payload has to survive, by
# running something whose correct output cannot be produced by accident.
detect_quote_level() {
  local want='probe-2-ok'
  local script='echo probe-$((1+1))-ok'

  QUOTE_LEVEL=0
  [ "$(pod_sh "$CLIENT" "$script" 2>/dev/null | tr -d '\r\n')" = "$want" ] && return 0

  QUOTE_LEVEL=1
  [ "$(pod_sh "$CLIENT" "$script" 2>/dev/null | tr -d '\r\n')" = "$want" ] && return 0

  say "ABORT: cannot run a shell pipeline inside $CLIENT via REPL_KUBECTL."
  say "       Neither direct nor single-quoted payloads survived. Without this,"
  say "       produce silently does nothing and every result below is meaningless."
  exit 1
}

k8s_setup() {
  [ -n "$NS" ] && [ -n "$PODS" ] || { say "SKIP: k8s mode needs REPL_NS and REPL_PODS"; exit 0; }
  local ready; ready=$($KUBECTL get pods -n "$NS" --no-headers 2>/dev/null | grep -c "^$PODS.* 1/1 *Running")
  [ "$ready" -ge 3 ] || { say "SKIP: need 3 running $PODS pods in $NS (found $ready)"; exit 1; }

  # A pod that is Terminating still answers `get`, and reports phase Running
  # right up until it disappears. Treating that as "the client is ready" runs
  # the whole suite against a pod that dies underneath it: produce and consume
  # silently return nothing, and every result is meaningless. Wait it out.
  local waited=0
  while [ -n "$($KUBECTL get pod "$CLIENT" -n "$NS" -o jsonpath='{.metadata.deletionTimestamp}' 2>/dev/null)" ]; do
    [ "$waited" -eq 0 ] && say "-- waiting for a terminating $CLIENT to go away"
    waited=$((waited + 1))
    [ "$waited" -gt 60 ] && { say "SKIP: $CLIENT stuck terminating"; exit 1; }
    sleep 2
  done

  if ! $KUBECTL get pod "$CLIENT" -n "$NS" >/dev/null 2>&1; then
    say "-- creating client pod $CLIENT"
    $KUBECTL run "$CLIENT" -n "$NS" --restart=Never --image=docker.io/apache/kafka:3.7.0 \
      --image-pull-policy=IfNotPresent --command -- sleep 7200 >/dev/null 2>&1
  fi
  local i=0
  until [ "$($KUBECTL get pod "$CLIENT" -n "$NS" -o jsonpath='{.status.phase}' 2>/dev/null)" = Running ]; do
    i=$((i + 1)); [ "$i" -gt 60 ] && { say "SKIP: client pod never became Running"; exit 1; }
    sleep 3
  done
  BOOT="${PODS}-headless:9092"

  detect_quote_level
  say "-- client pod ready (shell quote level $QUOTE_LEVEL)"
}
k8s_teardown() { $KUBECTL delete pod "$CLIENT" -n "$NS" --wait=false >/dev/null 2>&1; }
k8s_placement() { $KUBECTL exec -n "$NS" "${PODS}-$1" -- ls "/data/wal/$2" 2>/dev/null | sort -n | tr '\n' ' ' | sed 's/ $//'; }
k8s_produce() { # $1=topic $2=acks — keyed records so the hash partitioner spreads
  $KUBECTL exec -n "$NS" "$CLIENT" -- /opt/kafka/bin/kafka-topics.sh \
    --bootstrap-server "$BOOT" --create --topic "$1" \
    --partitions "$PARTITIONS" --replication-factor 3 >/dev/null 2>&1
  pod_sh "$CLIENT" \
    "seq 1 $RECORDS | awk '{print \$1\":acks=$2 rec \"\$1}' | /opt/kafka/bin/kafka-console-producer.sh \
       --bootstrap-server $BOOT --topic $1 --producer-property acks=$2 \
       --property parse.key=true --property key.separator=:" >/dev/null 2>&1
}
k8s_consume() {
  pod_sh "$CLIENT" \
    "/opt/kafka/bin/kafka-console-consumer.sh --bootstrap-server $BOOT --topic $1 \
       --from-beginning --timeout-ms 30000 2>/dev/null | wc -l" 2>/dev/null | tr -cd '0-9'
}
# Replication factor, or 0 if it cannot be determined.
#
# NOTE: `kafka-topics.sh --describe` currently fails against Chronik with
#   "non-nullable field clusterId was serialized as null"
# because DescribeCluster returns a null cluster id, which the Java AdminClient
# refuses to deserialize. So RF is best-effort here and never fails the run —
# physical placement below is the assertion that matters.
k8s_rf() {
  local commas
  commas=$($KUBECTL exec -n "$NS" "$CLIENT" -- /opt/kafka/bin/kafka-topics.sh \
    --bootstrap-server "$BOOT" --describe --topic "$1" 2>/dev/null \
    | grep -oE 'Replicas: [0-9,]+' | head -1 | tr -cd ',' | wc -c)
  [ "${commas:-0}" -gt 0 ] && echo $((commas + 1)) || echo 0
}

case "$MODE" in
  local) setup=local_setup; teardown=local_teardown; placement=local_placement
         produce=local_produce; consume=local_consume; rf_commas=local_rf ;;
  k8s)   setup=k8s_setup;   teardown=k8s_teardown;   placement=k8s_placement
         produce=k8s_produce;   consume=k8s_consume;   rf_commas=k8s_rf ;;
  *)     say "unknown REPL_MODE=$MODE"; exit 2 ;;
esac

say "== regression_replication ($MODE): $RECORDS records per acks mode, RF=3 =="
$setup

for acks in 0 1 all; do
  topic="replplace-$acks-$$"
  say ""
  say "-- acks=$acks → $topic"
  $produce "$topic" "$acks"

  rf=$($rf_commas "$topic")
  if [ "$rf" -eq 0 ]; then
    say "   NOTE: replication factor undetermined — relying on physical placement"
  else
    [ "$rf" -eq 3 ] || fail "acks=$acks: topic is RF=$rf, expected 3"
  fi

  # Converged when every node holds the same partition set, and it is non-empty.
  # Asserting against the UNION rather than a fixed 0..N keeps the check honest
  # whichever way the client's partitioner distributed the records.
  deadline=$((SECONDS + SETTLE))
  while [ "$SECONDS" -lt "$deadline" ]; do
    union=$(for n in 1 2 3; do $placement "$n" "$topic"; echo; done | tr ' ' '\n' | grep -v '^$' | sort -nu | tr '\n' ' ' | sed 's/ $//')
    converged=1
    [ -z "$union" ] && converged=0
    for n in 1 2 3; do
      [ "$($placement "$n" "$topic")" = "$union" ] || converged=0
    done
    [ "$converged" -eq 1 ] && break
    sleep 2
  done

  [ -n "$union" ] || fail "acks=$acks: no partitions found on any node — did produce fail?"
  for n in 1 2 3; do
    got=$($placement "$n" "$topic")
    say "   node$n: [$got]"
    [ "$got" = "$union" ] \
      || fail "acks=$acks: node$n holds [$got] but the cluster holds [$union] — data exists on fewer nodes than RF=3 promises"
  done

  consumed=$($consume "$topic")
  say "   consumed $consumed / produced $RECORDS"
  if [ "$acks" = "0" ]; then
    [ "$consumed" -eq "$RECORDS" ] || say "   NOTE: acks=0 shortfall is not a failure (fire-and-forget, no delivery guarantee)"
  else
    [ "$consumed" -eq "$RECORDS" ] || fail "acks=$acks: consumed $consumed of $RECORDS records"
  fi
done

# ---------------------------------------------------- RP-0.3: ISR honesty --
# ISR must shrink when a replica dies. This is the assertion that would have
# caught the original outage: /admin/status reported isr:[1,2,3] for nine months
# while zero follower copies existed on disk.
#
# It also guards the two ways the honest version can go wrong, both of which we
# hit while building it:
#   - reporting the assignment whenever the tracker is empty (everything looks
#     healthy precisely when nothing is replicating);
#   - never evicting a follower that was caught up when it died (a node killed
#     for 60s still showed isr=[1,2,3]).
if [ "$MODE" = "k8s" ] && [ "$FAIL" -eq 0 ]; then
  say ""
  say "-- ISR honesty: killing a replica, ISR must shrink"

  # curl from a BROKER pod, not the Kafka client pod: the apache/kafka image has
  # no curl, while the chronik image installs it (Dockerfile.binary).
  #
  # Only inspect partitions this node LEADS. Followers ACK to the partition
  # leader, so only the leader's tracker has real data; a non-leader legitimately
  # falls back to reporting the assignment. Asserting against a non-leader's view
  # would be testing the fallback, not ISR.
  isr_of_led() { # $1=topic — ISR of the partitions node 1 leads
    $KUBECTL exec -n "$NS" "${PODS}-1" -- \
      curl -s -m 15 http://localhost:6092/admin/status 2>/dev/null \
      | tr '{' '\n' | grep "\"topic\":\"$1\"" | grep '"leader":1,' \
      | grep -o '"isr":\[[^]]*\]' | tr '\n' ' '
  }

  topic="replplace-all-$$"
  before=$(isr_of_led "$topic")
  say "   before: $before"

  # The replica has to STAY down, and deleting the pod does not achieve that.
  #
  # The operator recreates it within ~4s and the broker resumes fetching almost
  # immediately — long before the pod reports Ready, so "0/1 Running" looks like
  # an outage while the replica is in fact fully caught up. The ISR liveness
  # window is 30s, so a blip that short must NOT evict, and keeping the replica
  # in ISR through it is correct: Kafka's replica.lag.time.max.ms defaults to
  # 30s for the same reason.
  #
  # Under push this test passed anyway, because the leader had to re-establish
  # its own outbound connection before the follower looked alive again, which
  # stretched the outage past the window. Pull recovers on the follower's
  # schedule instead, in seconds — better behaviour that silently invalidated
  # the test. Three separate attempts (delete once, delete in a loop, SIGSTOP on
  # PID 1) all failed to keep the replica down, and each looked exactly like
  # "ISR is over-reporting" while the broker was in fact correct.
  #
  # Cordoning the node the replica lives on is what actually works: the pod goes
  # Pending and stays there. Running pods elsewhere are unaffected, and it is
  # undone below.
  node3=$($KUBECTL get pod "${PODS}-3" -n "$NS" -o jsonpath='{.spec.nodeName}' 2>/dev/null)
  if [ -z "$node3" ]; then
    fail "cannot determine which node ${PODS}-3 runs on — skipping the ISR assertion rather than guessing"
  else
    say "   holding ${PODS}-3 down (cordoning $node3)"
    $KUBECTL cordon "$node3" >/dev/null 2>&1
    $KUBECTL delete pod "${PODS}-3" -n "$NS" --wait=false >/dev/null 2>&1

    shrunk=0
    deadline=$((SECONDS + 120))
    while [ "$SECONDS" -lt "$deadline" ]; do
      sleep 5
      now=$(isr_of_led "$topic")
      # Node 3 gone from every partition's ISR that still reports.
      if [ -n "$now" ] && ! echo "$now" | grep -q '3'; then shrunk=1; break; fi
    done

    after=$(isr_of_led "$topic")
    say "   after:  $after"
    [ "$shrunk" -eq 1 ] \
      || fail "ISR still lists a replica held down for 120s — /admin/status is over-reporting health"

    $KUBECTL uncordon "$node3" >/dev/null 2>&1
  fi

  # Let the pod come back so the cluster is left usable.
  for i in $(seq 1 30); do
    r=$($KUBECTL get pods -n "$NS" --no-headers 2>/dev/null | grep -c "^${PODS}-.* 1/1 *Running")
    [ "$r" -ge 3 ] && break
    sleep 5
  done
fi

# ------------------------- RP-0.4: convergence after a leader change (RP-3.3) --
# A replica that was leader, accepted writes, and then lost the election holds
# records the new leader never committed. It must discard them and converge —
# and must not, in doing so, drop anything that was acknowledged.
#
# Both directions matter and they fail differently. Keeping the divergent tail
# leaves two logs that agree on offsets and disagree on records, which no later
# check can detect. Over-truncating loses acknowledged data outright. The
# assertions below are the observable form of each: every node holds the
# partition, and every acknowledged record is still readable.
#
# Same methodology trap as RP-0.3: the old leader has to genuinely stay down
# while the new one takes writes, and deleting a pod does not achieve that —
# it is back in ~4s. Cordon the node.
if [ "$MODE" = "k8s" ] && [ "$FAIL" -eq 0 ]; then
  say ""
  say "-- convergence after a leader change (RP-3.3)"

  trunc_topic="repltrunc-$$"
  BEFORE_N=200
  AFTER_N=200

  produce_range() { # $1=first $2=last — acks=all, so every record here is a promise
    pod_sh "$CLIENT" \
      "seq $1 $2 | awk '{print \$1\":rec \"\$1}' | /opt/kafka/bin/kafka-console-producer.sh \
         --bootstrap-server $BOOT --topic $trunc_topic --producer-property acks=all \
         --property parse.key=true --property key.separator=:" >/dev/null 2>&1
  }

  # Leader of ONE named partition, observed from a node that is not the one
  # being killed.
  #
  # This used to take `head -1` of every partition line for the topic, which
  # silently watched a different partition than the one whose leader was
  # unseated: a topic created with `--partitions 1` came back with three, so
  # the probe reported "no new leader" while failover had worked perfectly.
  # Never assume the partition count you asked for.
  leader_of() { # $1 = partition
    $KUBECTL exec -n "$NS" "$OBSERVER" -- \
      curl -s -m 15 http://localhost:6092/admin/status 2>/dev/null \
      | tr '{' '\n' | grep "\"topic\":\"$trunc_topic\"" \
      | grep "\"partition\":$1," \
      | grep -o '"leader":[0-9]*' | head -1 | tr -cd '0-9'
  }

  $KUBECTL exec -n "$NS" "$CLIENT" -- /opt/kafka/bin/kafka-topics.sh \
    --bootstrap-server "$BOOT" --create --topic "$trunc_topic" \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1

  produce_range 1 "$BEFORE_N"
  produced_total="$BEFORE_N"
  sleep 5

  # Track partition 0 specifically, and observe from node 1 unless node 1 is
  # the one we are about to unseat (a status endpoint on a dead node reports
  # nothing, which reads exactly like "no leader" — a trap this suite fell into).
  OBSERVER="${PODS}-1"
  target_partition=0
  old_leader=$(leader_of "$target_partition")
  if [ "$old_leader" = "1" ]; then
    OBSERVER="${PODS}-2"
  fi
  if [ -z "$old_leader" ]; then
    fail "RP-3.3: no leader reported for $trunc_topic — cannot stage a leader change"
  else
    say "   leader before: node $old_leader"
    victim_node=$($KUBECTL get pod "${PODS}-${old_leader}" -n "$NS" -o jsonpath='{.spec.nodeName}' 2>/dev/null)

    if [ -z "$victim_node" ]; then
      fail "RP-3.3: cannot determine which k8s node ${PODS}-${old_leader} runs on — refusing to guess"
    else
      say "   holding ${PODS}-${old_leader} down (cordoning $victim_node)"
      $KUBECTL cordon "$victim_node" >/dev/null 2>&1
      $KUBECTL delete pod "${PODS}-${old_leader}" -n "$NS" --wait=false >/dev/null 2>&1

      # Wait for a different node to take the partition (RP-5 failover).
      new_leader=""
      deadline=$((SECONDS + 180))
      while [ "$SECONDS" -lt "$deadline" ]; do
        sleep 5
        now=$(leader_of "$target_partition")
        if [ -n "$now" ] && [ "$now" != "$old_leader" ]; then new_leader="$now"; break; fi
      done

      if [ -z "$new_leader" ]; then
        fail "RP-3.3: partition $target_partition still led by node $old_leader 180s after it went down — no failover"
      else
        say "   leader after:  node $new_leader"
        produce_range $((BEFORE_N + 1)) $((BEFORE_N + AFTER_N))
        # Only count what was actually written. Asserting against a total that
        # includes records a failed branch never produced turns one failure
        # into two, and the second one is fiction.
        produced_total=$((BEFORE_N + AFTER_N))
      fi

      # Bring the old leader back as a follower. This is the moment under test.
      say "   restoring ${PODS}-${old_leader}"
      $KUBECTL uncordon "$victim_node" >/dev/null 2>&1
      for i in $(seq 1 60); do
        r=$($KUBECTL get pods -n "$NS" --no-headers 2>/dev/null | grep -c "^${PODS}-.* 1/1 *Running")
        [ "$r" -ge 3 ] && break
        sleep 5
      done

      # Converged when every node holds the partition again.
      deadline=$((SECONDS + SETTLE + 60))
      while [ "$SECONDS" -lt "$deadline" ]; do
        union=$(for n in 1 2 3; do $placement "$n" "$trunc_topic"; echo; done \
                | tr ' ' '\n' | grep -v '^$' | sort -nu | tr '\n' ' ' | sed 's/ $//')
        ok=1
        [ -z "$union" ] && ok=0
        for n in 1 2 3; do
          [ "$($placement "$n" "$trunc_topic")" = "$union" ] || ok=0
        done
        [ "$ok" -eq 1 ] && break
        sleep 5
      done

      for n in 1 2 3; do
        got=$($placement "$n" "$trunc_topic")
        say "   node$n: [$got]"
        [ "$got" = "$union" ] \
          || fail "RP-3.3: node$n holds [$got] but the cluster holds [$union] — the returning replica did not converge"
      done

      # Nothing acknowledged may be lost. Over-truncation shows up here.
      #
      # Compared on DISTINCT records, not raw lines: acks=1 without idempotence
      # permits duplicates, and a subscribing consumer here has been observed
      # re-reading a partition after a rebalance (issue #36). Neither is data
      # loss, which is what this assertion is for. A surplus is reported, not
      # failed.
      got=$(pod_sh "$CLIENT" \
        "/opt/kafka/bin/kafka-console-consumer.sh --bootstrap-server $BOOT --topic $trunc_topic \
           --from-beginning --timeout-ms 30000 2>/dev/null | sort -u | wc -l" 2>/dev/null | tr -cd '0-9')
      say "   distinct consumed $got / acknowledged $produced_total"
      [ "${got:-0}" -ge "$produced_total" ] \
        || fail "RP-3.3: only $got of $produced_total acknowledged records survived — the failover lost data"

      # Witness that the reconcile path actually ran. Informational: whether a
      # divergent tail existed at all depends on timing, and a run where the old
      # leader's log was already a prefix is a pass, not a miss. But a run where
      # the handshake never happened proves nothing about truncation, and saying
      # so is the difference between "tested" and "did not crash".
      say "   reconcile evidence on the returning replica:"
      $KUBECTL logs -n "$NS" "${PODS}-${old_leader}" --tail=4000 2>/dev/null \
        | grep -E "truncat|diverged|reconcil|epoch ended|prefix of the leader" \
        | tail -5 | sed 's/^/     /' \
        || say "     (none found — the handshake may not have been exercised this run)"
    fi
  fi
fi

$teardown
say ""
if [ "$FAIL" -eq 0 ]; then
  say "== PASS: every replica holds every partition at acks=0, 1 and all; ISR tracks reality =="
else
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
