#!/bin/bash
# The operator's topic/user/ACL client against a real broker.
#
# ChronikTopic and ChronikUser reconcile through the Kafka admin protocol
# (crates/chronik-operator/src/kafka_admin.rs). Its earlier HTTP client was
# tested against a mock of endpoints the broker never had, so it passed while
# never working; this runs the client against the broker instead, twice:
#   1. no SASL;
#   2. SASL required and ACLs enforced, the client logging in as the broker
#      user, as the operator does on a SASL cluster.
#
# Usage: operator_kafka_admin.sh <binary> <label>
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/opadmin-$LABEL; LOG=tests/cluster/logs/opadmin-$LABEL
SECRET="opadmin-broker-secret-$RANDOM$RANDOM"
fail=0
pid=
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT

cargo build -q -p chronik-operator --tests 2>/dev/null || { echo "FAIL: operator tests do not build"; exit 1; }
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
mkdir -p "$(dirname "$LOG")"

run() { # <name> <test env...> -- <broker env...>
  local name=$1; shift
  local test_env=() broker_env=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do test_env+=("$1"); shift; done
  shift
  broker_env=("$@")
  rm -rf "$D"; mkdir -p "$D"
  env CHRONIK_ADVERTISED_ADDR=localhost "${broker_env[@]}" "$BIN" start --data-dir "$D" > "$LOG-$name.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done
  sleep 3
  echo "-- $name"
  if env CHRONIK_ADMIN_TEST_BOOTSTRAP=localhost:9092 "${test_env[@]}" \
      cargo test -q -p chronik-operator --test kafka_admin_test -- --ignored --test-threads 1 2>&1 \
      | tee "$LOG-$name.test" | grep -E '^test |test result|panicked|assert' ; then :; fi
  grep -q 'test result: ok. 2 passed' "$LOG-$name.test" || { echo "  FAIL $name"; fail=1; }
  grep -q panicked "$LOG-$name.log" && { echo "  FAIL broker panicked ($name)"; fail=1; }
  kill $pid; wait $pid 2>/dev/null; pid=
}

echo "=== $LABEL ($BIN)"
run plain -- CHRONIK_SASL_ENABLED=false
run sasl-acl CHRONIK_ADMIN_TEST_SECRET="$SECRET" -- \
  CHRONIK_SASL_ENABLED=true CHRONIK_INTER_BROKER_SECRET="$SECRET" \
  CHRONIK_ACL_ENABLED=true CHRONIK_ACL_ALLOW_IF_NO_ACL=false

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
