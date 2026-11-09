#!/bin/bash
# Issue #57, defect 2: a distributed query whose peers do not answer must not
# report a merged total as if the cluster agreed.
#
# The reported symptom was `SELECT COUNT(*)` returning 0 at HTTP 200 for a topic
# holding 2.55M records, on a cluster whose nodes were too busy to answer in
# time. The merge math was right; the set of answers it merged was silently
# short.
#
# This drives the real thing: a 3-node local cluster, a node made unresponsive
# with SIGSTOP (sockets stay open, so requests hang exactly as they do on an
# overloaded node rather than failing fast), and a COUNT checked against ground
# truth taken from Kafka itself.
#
#   PRE-FIX  (v2.15.1): HTTP 200 with a total short by the stopped node's share
#   POST-FIX           : HTTP 503 naming the node, and `allow_partial` returns
#                        the short total LABELLED as partial
#
# Usage:
#   tests/test_incomplete_fanout.sh                 # the build in target/release
#   CHRONIK_BIN=/path/to/old tests/test_incomplete_fanout.sh --expect-prefix
#
# Exit: 0 pass, 1 fail, 2 could not set the scenario up (never a silent pass).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER="$PROJECT_ROOT/tests/cluster"

EXPECT_PREFIX=0
[ "${1:-}" = "--expect-prefix" ] && EXPECT_PREFIX=1

TOPIC="i57fanout$$"
RECORDS=30000
PARTITIONS=3
# Node 1 coordinates; node 3 is the one we silence.
API1=6092
BOOT=localhost:9092
KAFKA_BIN="${KAFKA_BIN:-/opt/kafka/bin}"

pass=0; fail=0
ok()   { echo "  PASS: $1"; pass=$((pass+1)); }
bad()  { echo "  FAIL: $1"; fail=$((fail+1)); }
setup_fail() { echo "SETUP FAILED: $1" >&2; cleanup; exit 2; }

STOPPED_PID=""
cleanup() {
    [ -n "$STOPPED_PID" ] && kill -CONT "$STOPPED_PID" 2>/dev/null
    "$CLUSTER/stop.sh" >/dev/null 2>&1
}
trap cleanup EXIT

if [ ! -x "$KAFKA_BIN/kafka-topics.sh" ]; then
    setup_fail "Kafka CLI not found at $KAFKA_BIN (set KAFKA_BIN)"
fi

echo "=== Issue #57 defect 2: an incomplete fan-out must not answer silently ==="
echo "binary: ${CHRONIK_BIN:-$PROJECT_ROOT/target/release/chronik-server}"
echo "mode:   $([ $EXPECT_PREFIX = 1 ] && echo 'expecting the PRE-FIX bug' || echo 'expecting the FIX')"
echo

# --- 1. cluster up -----------------------------------------------------------
"$CLUSTER/stop.sh" >/dev/null 2>&1
if ! "$CLUSTER/start.sh" >/dev/null 2>&1; then
    setup_fail "cluster did not start (see $CLUSTER/logs)"
fi

for _ in $(seq 1 60); do
    curl -fsS -m 2 "http://localhost:$API1/health" >/dev/null 2>&1 && break
    sleep 2
done
curl -fsS -m 5 "http://localhost:$API1/health" >/dev/null 2>&1 \
    || setup_fail "node 1 unified API never came up on $API1"

# --- 2. load and take ground truth from Kafka --------------------------------
"$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$BOOT" --create --topic "$TOPIC" \
    --partitions "$PARTITIONS" --replication-factor 3 >/dev/null 2>&1

seq 1 "$RECORDS" \
  | awk '{printf "{\"id\":%d,\"amount\":%d}\n", $1, $1*7%1000}' \
  | "$KAFKA_BIN/kafka-console-producer.sh" --bootstrap-server "$BOOT" --topic "$TOPIC" \
        --producer-property acks=all >/dev/null 2>&1

# Ground truth is the sum of Kafka's own end offsets, never the system we are
# testing: asking /_sql what it holds and then checking /_sql against it would
# agree with any bug.
TRUTH=$("$KAFKA_BIN/kafka-get-offsets.sh" --bootstrap-server "$BOOT" --topic "$TOPIC" 2>/dev/null \
        | awk -F: '{s+=$3} END {print s+0}')
[ "$TRUTH" -eq "$RECORDS" ] || setup_fail "produced $RECORDS but Kafka reports $TRUTH"
echo "ground truth from Kafka end offsets: $TRUTH records"

# Give the indexer time to land the data where SQL reads it.
sleep 20

count_of() { # $1 = json body -> the single count value, or empty
    sed -n 's/.*"count(\*)":\([0-9-]*\).*/\1/p' <<<"$1"
}

q() { # $1 = sql, $2 = extra json -> "HTTPSTATUS\nbody"
    local extra="${2:-}"
    curl -s -m 60 -w '\n%{http_code}' -X POST "http://localhost:$API1/_sql" \
        -H 'Content-Type: application/json' \
        -d "{\"query\":\"$1\"${extra:+,$extra}}"
}

# --- 3. healthy baseline -----------------------------------------------------
resp=$(q "SELECT COUNT(*) FROM $TOPIC")
body=$(sed '$d' <<<"$resp"); code=$(tail -1 <<<"$resp")
base=$(count_of "$body")
if [ "$code" = 200 ] && [ "$base" = "$TRUTH" ]; then
    ok "healthy cluster counts $base, matching Kafka"
else
    setup_fail "baseline COUNT was '$base' (HTTP $code), expected $TRUTH — body: $body"
fi

# --- 4. silence node 3 -------------------------------------------------------
STOPPED_PID=$(cat "$CLUSTER/data/node3.pid" 2>/dev/null)
[ -n "$STOPPED_PID" ] || setup_fail "no pid file for node 3"
kill -STOP "$STOPPED_PID" || setup_fail "could not SIGSTOP node 3 ($STOPPED_PID)"
echo "node 3 (pid $STOPPED_PID) stopped — its requests now hang rather than refuse"

# --- 5. the query that mattered ---------------------------------------------
resp=$(q "SELECT COUNT(*) FROM $TOPIC")
body=$(sed '$d' <<<"$resp"); code=$(tail -1 <<<"$resp")
got=$(count_of "$body")
echo "  COUNT with a node silenced -> HTTP $code, count='${got:-none}'"

if [ $EXPECT_PREFIX = 1 ]; then
    # Documenting the bug: a 200 carrying a number lower than the truth.
    if [ "$code" = 200 ] && [ -n "$got" ] && [ "$got" -lt "$TRUTH" ]; then
        ok "pre-fix reproduces: HTTP 200 with $got of $TRUTH records, no indication"
    elif [ "$code" = 200 ] && [ "$got" = "$TRUTH" ]; then
        bad "pre-fix did NOT undercount (the stopped node led no partitions?)"
    else
        bad "pre-fix behaved unexpectedly: HTTP $code, body $body"
    fi
else
    if [ "$code" = 503 ] && grep -q 'IncompleteClusterResult' <<<"$body"; then
        ok "refused with 503 IncompleteClusterResult instead of a short total"
        grep -q 'node 3' <<<"$body" \
            && ok "the error names the node that did not answer" \
            || bad "the error does not name node 3: $body"
        grep -q 'allow_partial' <<<"$body" \
            && ok "the error says how to get a partial answer deliberately" \
            || bad "the error does not mention allow_partial: $body"
    elif [ "$code" = 200 ] && [ "$got" = "$TRUTH" ]; then
        bad "answered $TRUTH with a node down — leadership moved before the query, scenario void"
    else
        bad "expected HTTP 503 IncompleteClusterResult, got HTTP $code: $body"
    fi

    # --- 6. and the deliberate partial answer --------------------------------
    resp=$(q "SELECT COUNT(*) FROM $TOPIC" '"allow_partial":true')
    body=$(sed '$d' <<<"$resp"); code=$(tail -1 <<<"$resp")
    got=$(count_of "$body")
    echo "  COUNT with allow_partial -> HTTP $code, count='${got:-none}'"
    if [ "$code" = 200 ] && [ -n "$got" ]; then
        ok "allow_partial returns an answer ($got of $TRUTH)"
        grep -q '"partial":true' <<<"$body" \
            && ok "and it is labelled partial" \
            || bad "a partial answer must carry \"partial\":true: $body"
        grep -q 'incomplete_nodes' <<<"$body" \
            && ok "and it lists the nodes it is missing" \
            || bad "a partial answer must list the missing nodes: $body"
    else
        bad "allow_partial should still answer, got HTTP $code: $body"
    fi
fi

kill -CONT "$STOPPED_PID" 2>/dev/null; STOPPED_PID=""

echo
echo "=== $pass passed, $fail failed ==="
[ "$fail" -eq 0 ] || exit 1
exit 0
