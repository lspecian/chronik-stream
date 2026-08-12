#!/usr/bin/env bash
# RP-3.3: manufacture a genuinely divergent log, and prove the follower discards it.
#
# Every other failure this suite stages ends in "log is a prefix — nothing to
# truncate", which is the *correct* outcome and therefore proves nothing about
# the branch that deletes data. Killing a leader cannot produce divergence when
# writes are acknowledged by the full ISR: the survivors already hold everything
# the dead node had.
#
# Real divergence needs a leader that accepted writes its followers never
# received, and which then loses the election. That is a network partition, not
# a node failure — so this isolates the leader with a NetworkPolicy (Calico
# enforces them here) rather than killing it:
#
#   1. produce a common prefix at acks=all      → all three replicas agree
#   2. cut the leader off from the other brokers, leaving the client reachable
#   3. produce at acks=1 to the isolated leader → records that exist ONLY there
#   4. the surviving two hold quorum, RP-5 moves leadership to one of them
#   5. produce at acks=all to the new leader    → different records, SAME offsets
#   6. heal the partition
#
# The old leader now holds a tail that the new leader never committed, at
# offsets the new leader has filled with something else. Two logs that agree on
# offsets and disagree on records — the exact condition leader epochs exist to
# detect. It must truncate to the divergence point and re-replicate.
#
# PASS:
#   - the returning leader logs a truncation for the partition
#   - every record acknowledged by the NEW leader is readable afterwards
#   - the records only the isolated leader ever had are GONE (they were never
#     committed; keeping them is the corruption this phase prevents)
#
# Usage:
#   REPL_NS=rp3-test REPL_PODS=rp3 \
#     REPL_KUBECTL="ssh ubuntu@host sudo microk8s kubectl" \
#     ./tests/cluster/divergence_truncation.sh
set -u

KUBECTL="${REPL_KUBECTL:-kubectl}"
NS="${REPL_NS:-}"
PODS="${REPL_PODS:-rp3}"
CLIENT="${REPL_CLIENT:-replcheck}"
PREFIX_N="${DIVERGE_PREFIX:-100}"   # common prefix, acks=all
ORPHAN_N="${DIVERGE_ORPHAN:-60}"    # written only to the isolated leader
WINNER_N="${DIVERGE_WINNER:-60}"    # written to the new leader at the same offsets
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

[ -n "$NS" ] || { say "SKIP: set REPL_NS"; exit 0; }
BOOT="${PODS}-headless:9092"
TOPIC="diverge-$$"
POLICY="isolate-leader-$$"

cleanup() {
  $KUBECTL delete networkpolicy "$POLICY" -n "$NS" --wait=false >/dev/null 2>&1
}
trap cleanup EXIT

# Run a shell pipeline inside the client pod. Quoting through
# ssh → kubectl → sh is where this suite has silently produced nothing before,
# so the payload is passed as a single argument to sh -c.
pod_sh() { $KUBECTL exec -n "$NS" "$CLIENT" -- sh -c "$1"; }

status_of() { # $1 = pod to ask
  $KUBECTL exec -n "$NS" "$1" -- curl -s -m 15 http://localhost:6092/admin/status 2>/dev/null
}

leader_of() { # $1 = pod to ask; leader of TOPIC partition 0
  status_of "$1" | tr '{' '\n' | grep "\"topic\":\"$TOPIC\"" \
    | grep '"partition":0,' | grep -o '"leader":[0-9]*' | head -1 | tr -cd '0-9'
}

produce() { # $1=first $2=last $3=acks $4=tag
  pod_sh "seq $1 $2 | awk '{print \$1\":$4-\"\$1}' | \
    /opt/kafka/bin/kafka-console-producer.sh --bootstrap-server $BOOT --topic $TOPIC \
      --producer-property acks=$3 --property parse.key=true --property key.separator=:" \
    >/dev/null 2>&1
}

say "== RP-3.3 divergence test: $TOPIC =="

$KUBECTL exec -n "$NS" "$CLIENT" -- /opt/kafka/bin/kafka-topics.sh \
  --bootstrap-server "$BOOT" --create --topic "$TOPIC" \
  --partitions 1 --replication-factor 3 >/dev/null 2>&1

# 1. Common prefix. acks=all, so all three replicas hold it.
produce 1 "$PREFIX_N" all prefix
sleep 6

# Ask a node we are NOT about to isolate — a status endpoint behind a partition
# reports nothing, which reads exactly like "no leader".
OLD=$(leader_of "${PODS}-1")
[ -z "$OLD" ] && OLD=$(leader_of "${PODS}-2")
if [ -z "$OLD" ]; then fail "no leader for $TOPIC"; exit 1; fi
OBS="${PODS}-1"; [ "$OLD" = "1" ] && OBS="${PODS}-2"
say "-- leader is node $OLD (observing from $OBS)"

# 2. Isolate the leader from the other brokers, leaving the client reachable.
#
#    BOTH directions, and then RESTART the node — that second part is what makes
#    this work at all.
#
#    **A NetworkPolicy does not partition an existing cluster.** Calico allows
#    established connections, so the pre-existing gRPC channels between brokers
#    keep carrying Raft traffic straight through a policy applied afterwards.
#    Only *new* connections are blocked, which is why a `/dev/tcp` probe reports
#    the peer as unreachable while the cluster carries on talking normally.
#
#    That cost two wasted runs and produced a false conclusion — with the
#    "isolated" leader still reachable over its old channels, no election
#    happened, and the obvious reading was that Raft leader election was broken.
#    It is not: killing the leader's pod elects a new one in about three seconds.
#
#    So: apply the policy first, then delete the pod. It comes back with every
#    broker-to-broker connection denied from birth, while the client is still
#    allowed in. Its WAL survives on the PVC, and its metadata still names it
#    leader — which is exactly the state needed to accept writes nobody else
#    will ever see.
say "-- isolating node $OLD from its peers, both directions (NetworkPolicy)"
cat <<YAML | $KUBECTL apply -f - >/dev/null 2>&1
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: $POLICY
  namespace: $NS
spec:
  podSelector:
    matchLabels:
      chronik.io/node-id: "$OLD"
  policyTypes: [Ingress, Egress]
  ingress:
    - from:
        - podSelector:
            matchLabels:
              run: $CLIENT
  egress:
    - to:
        - podSelector:
            matchLabels:
              run: $CLIENT
    - ports:
        - protocol: UDP
          port: 53
        - protocol: TCP
          port: 53
YAML

# 2b. Restart the isolated node so its connections are re-made under the policy.
say "-- restarting node $OLD so its peer connections are denied from birth"
$KUBECTL delete pod "${PODS}-${OLD}" -n "$NS" --wait=false >/dev/null 2>&1
for i in $(seq 1 40); do
  phase=$($KUBECTL get pod "${PODS}-${OLD}" -n "$NS" -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$phase" = "Running" ] && break
  sleep 5
done
sleep 20   # let it finish recovery and settle into believing it still leads

# 3. Records that exist ONLY on the isolated leader.
#
#    Bootstrapped directly at that pod, not through the headless service: the
#    service would hand us a surviving node's metadata, which by now names a
#    different leader, and the write would go there instead — the opposite of
#    what this test needs. Talking to the isolated node makes it answer with its
#    own (stale) view, so it accepts the write as leader. acks=1 means it
#    acknowledges alone, and being cut off, nothing else can ever have them.
say "-- writing $ORPHAN_N record(s) only node $OLD can have"
pod_sh "seq $((PREFIX_N + 1)) $((PREFIX_N + ORPHAN_N)) | awk '{print \$1\":orphan-\"\$1}' | \
  timeout 90 /opt/kafka/bin/kafka-console-producer.sh \
    --bootstrap-server ${PODS}-${OLD}.${PODS}-headless:9092 --topic $TOPIC \
    --producer-property acks=1 --property parse.key=true --property key.separator=:" \
  >/dev/null 2>&1
orphans_written=$?
say "   (producer exit $orphans_written)"

# 4. The surviving two hold quorum; RP-5 should move leadership.
NEW=""
deadline=$((SECONDS + 240))
while [ "$SECONDS" -lt "$deadline" ]; do
  sleep 10
  now=$(leader_of "$OBS")
  if [ -n "$now" ] && [ "$now" != "$OLD" ]; then NEW="$now"; break; fi
done

if [ -z "$NEW" ]; then
  fail "leadership never moved off the isolated node $OLD — cannot create divergence"
  exit 1
fi
say "-- leadership moved to node $NEW"

# 5. Different records at the SAME offsets the orphans occupy.
produce $((PREFIX_N + 1)) $((PREFIX_N + WINNER_N)) all winner
say "-- wrote $WINNER_N committed record(s) over those offsets"
sleep 5

# 6. Heal.
say "-- healing the partition"
cleanup
sleep 5

# The returning leader must discard its uncommitted tail.
say "-- waiting for node $OLD to reconcile"
truncated=0
deadline=$((SECONDS + 300))
while [ "$SECONDS" -lt "$deadline" ]; do
  sleep 10
  if $KUBECTL logs -n "$NS" "${PODS}-${OLD}" --since=15m 2>/dev/null \
     | grep -qE "truncated to|diverged from the leader"; then
    truncated=1; break
  fi
done

say ""
say "-- evidence on node $OLD:"
$KUBECTL logs -n "$NS" "${PODS}-${OLD}" --since=15m 2>/dev/null \
  | grep -E "diverged from the leader|truncated to|log is a prefix|not truncating" \
  | tail -5 | sed 's/^/     /'

[ "$truncated" -eq 1 ] \
  || fail "node $OLD never truncated — it is still holding records the cluster never committed"

# Committed records must survive; uncommitted ones must not.
survived=$(pod_sh "/opt/kafka/bin/kafka-console-consumer.sh --bootstrap-server $BOOT \
  --topic $TOPIC --from-beginning --timeout-ms 30000 2>/dev/null | sort -u > /tmp/d.txt; \
  grep -c '^winner-' /tmp/d.txt" 2>/dev/null | tr -cd '0-9')
orphans=$(pod_sh "grep -c '^orphan-' /tmp/d.txt" 2>/dev/null | tr -cd '0-9')

say ""
say "   committed (winner) records readable: ${survived:-0} / $WINNER_N"
say "   uncommitted (orphan) records still readable: ${orphans:-0} (must be 0)"

[ "${survived:-0}" -ge "$WINNER_N" ] \
  || fail "only ${survived:-0} of $WINNER_N committed records survived — truncation took too much"
[ "${orphans:-0}" -eq 0 ] \
  || fail "${orphans} record(s) the cluster never committed are still readable — the divergent tail survived"

say ""
if [ "$FAIL" -eq 0 ]; then
  say "== PASS: the divergent tail was discarded and every committed record survived =="
else
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
