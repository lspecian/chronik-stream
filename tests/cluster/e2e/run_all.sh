#!/bin/bash
# Every end-to-end cluster check, against one binary. One line per check.
#
# Usage: run_all.sh <binary> <label>
# Needs: KAFKA_HOME (a Kafka distribution), LEGACY_BIN (a v2.17.0 binary, for
# search_after_archive), SETUP_BIN (a v2.16.7 binary, for
# straddle's setup), kcat and a JDK. Logs go to tests/cluster/data/e2e/<label>/.
# Each check starts and stops the local 3-node cluster (tests/cluster/), so
# nothing else may use it meanwhile.
set -u
BIN=${1:?binary}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
E2E="$REPO/tests/cluster/e2e"
export KAFKA_BIN="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin" CHRONIK_BIN=$BIN
: "${SETUP_BIN:?set SETUP_BIN to a v2.16.7 chronik-server (straddle setup)}"
: "${LEGACY_BIN:?set LEGACY_BIN to a v2.17.0 chronik-server (search_after_archive upgrade case)}"
export LEGACY_BIN
OUT="$REPO/tests/cluster/data/e2e/$LABEL"; mkdir -p "$OUT"
cd "$REPO"
failed=0
run() { # name, command...
  local name=$1; shift
  local start=$SECONDS
  "$@" > "$OUT/$name.log" 2>&1; local rc=$?
  ./tests/cluster/stop.sh >/dev/null 2>&1
  # Some checks exit 0 when a prerequisite is missing; a skip is not a pass.
  grep -q 'SKIP:' "$OUT/$name.log" && rc=skip
  [ "$rc" = 0 ] || failed=$((failed + 1))
  printf '%-28s rc=%-4s %4ss  %s\n' "$name" "$rc" "$((SECONDS-start))" \
    "$(sed 's/\x1b\[[0-9;]*m//g' "$OUT/$name.log" | grep -E 'RESULT|== PASS|== FAIL|^PASS|^FAIL|PASS:|FAIL:|SKIP:' | tail -1 | cut -c1-100)"
}
run divergence_archived env DIVERGE_ARCHIVED=1 ./tests/cluster/local_divergence.sh
run divergence_plain ./tests/cluster/local_divergence.sh
run planted_archive "$E2E/planted_archive.sh" "$BIN" "$LABEL"
run recreate "$E2E/recreate.sh" "$BIN" "$LABEL"
run reclaim_restart "$E2E/reclaim_restart.sh" "$BIN" "$LABEL"
run single_copy "$E2E/single_copy.sh" "$BIN" "$LABEL"
run straddle "$E2E/straddle.sh" "$BIN" "$LABEL"
run produce_order "$E2E/produce_order.sh" "$BIN" "$LABEL"
run search_after_archive "$E2E/search_after_archive.sh" "$BIN" "$LABEL"
run search_many_segments "$E2E/search_many_segments.sh" "$BIN" "$LABEL"
run search_pagination "$E2E/search_pagination.sh" "$BIN" "$LABEL"
run regression_replication ./tests/cluster/regression_replication.sh
run follower_catchup ./tests/cluster/follower_catchup.sh
run concurrent_produce_order ./tests/test_concurrent_produce_order.sh
run retry_offset_gaps ./tests/test_retry_offset_gaps.sh
run legacy_format_replication ./tests/test_legacy_format_replication.sh
run preferred_leader_return ./tests/test_preferred_leader_return.sh
run tantivy_index_archive ./tests/test_tantivy_index_archive.sh
echo "failed: $failed"
[ $failed = 0 ]
