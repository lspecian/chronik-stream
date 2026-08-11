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
k8s_setup() {
  [ -n "$NS" ] && [ -n "$PODS" ] || { say "SKIP: k8s mode needs REPL_NS and REPL_PODS"; exit 0; }
  local ready; ready=$($KUBECTL get pods -n "$NS" --no-headers 2>/dev/null | grep -c "^$PODS.* 1/1 *Running")
  [ "$ready" -ge 3 ] || { say "SKIP: need 3 running $PODS pods in $NS (found $ready)"; exit 1; }

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
}
k8s_teardown() { $KUBECTL delete pod "$CLIENT" -n "$NS" --wait=false >/dev/null 2>&1; }
k8s_placement() { $KUBECTL exec -n "$NS" "${PODS}-$1" -- ls "/data/wal/$2" 2>/dev/null | sort -n | tr '\n' ' ' | sed 's/ $//'; }
k8s_produce() { # $1=topic $2=acks — keyed records so the hash partitioner spreads
  $KUBECTL exec -n "$NS" "$CLIENT" -- /opt/kafka/bin/kafka-topics.sh \
    --bootstrap-server "$BOOT" --create --topic "$1" \
    --partitions "$PARTITIONS" --replication-factor 3 >/dev/null 2>&1
  $KUBECTL exec -n "$NS" "$CLIENT" -- bash -c \
    "seq 1 $RECORDS | awk '{print \$1\":acks=$2 rec \"\$1}' | /opt/kafka/bin/kafka-console-producer.sh \
       --bootstrap-server $BOOT --topic $1 --producer-property acks=$2 \
       --property parse.key=true --property key.separator=:" >/dev/null 2>&1
}
k8s_consume() {
  $KUBECTL exec -n "$NS" "$CLIENT" -- bash -c \
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

  admin_ip=$($KUBECTL get pod "${PODS}-1" -n "$NS" -o jsonpath='{.status.podIP}' 2>/dev/null)
  isr_of() { # $1=topic
    $KUBECTL exec -n "$NS" "$CLIENT" -- sh -c \
      "curl -s -m 15 http://$admin_ip:6092/admin/status" 2>/dev/null \
      | tr '{' '\n' | grep "\"topic\":\"$1\"" | grep -o '"isr":\[[^]]*\]' | tr '\n' ' '
  }

  topic="replplace-all-$$"
  before=$(isr_of "$topic")
  say "   before: $before"

  $KUBECTL delete pod "${PODS}-3" -n "$NS" --wait=false >/dev/null 2>&1

  shrunk=0
  deadline=$((SECONDS + 90))
  while [ "$SECONDS" -lt "$deadline" ]; do
    now=$(isr_of "$topic")
    # Node 3 gone from every partition's ISR that still reports.
    if [ -n "$now" ] && ! echo "$now" | grep -q '3'; then shrunk=1; break; fi
    sleep 5
  done

  after=$(isr_of "$topic")
  say "   after:  $after"
  [ "$shrunk" -eq 1 ] \
    || fail "ISR still lists the dead replica after 90s — /admin/status is over-reporting health"

  # Let the pod come back so the cluster is left usable.
  for i in $(seq 1 30); do
    r=$($KUBECTL get pods -n "$NS" --no-headers 2>/dev/null | grep -c "^${PODS}-.* 1/1 *Running")
    [ "$r" -ge 3 ] && break
    sleep 5
  done
fi

$teardown
say ""
if [ "$FAIL" -eq 0 ]; then
  say "== PASS: every replica holds every partition at acks=0, 1 and all; ISR tracks reality =="
else
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
