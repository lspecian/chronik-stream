#!/bin/bash
# Every end-to-end cluster check, against one binary. One line per check.
#
# Usage: run_all.sh <binary> <label>
# Needs: KAFKA_HOME (a Kafka distribution), LEGACY_BIN (a v2.17.0 binary, for
# search_after_archive), SETUP_BIN (a v2.16.7 binary, for
# straddle's setup), PREV_BIN (the previous release, for tantivy_cross_version),
# FORMAT_OLD_BIN (a v2.19.x binary, for raw_segment_compression and reindex: it
# must write plain raw segments and archives without JSON columns, which no
# v2.20 release does; those checks refuse a newer one), GROUPS_OLD_BIN
# (a v2.18.x binary, for group_upgrade), kcat and a JDK, passwordless sudo (wal_write_failure
# mounts a tmpfs), and the legacy_index_tool example built
# (cargo build --release -p chronik-search --example legacy_index_tool).
# Logs go to tests/cluster/data/e2e/<label>/.
# Each check starts and stops the local 3-node cluster (tests/cluster/), so
# nothing else may use it meanwhile.
# E2E_FROM=<check> resumes an interrupted run at that check, skipping the ones
# before it.
set -u
BIN=${1:?binary}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
E2E="$REPO/tests/cluster/e2e"
export KAFKA_BIN="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin" CHRONIK_BIN=$BIN
: "${SETUP_BIN:?set SETUP_BIN to a v2.16.7 chronik-server (straddle setup)}"
: "${LEGACY_BIN:?set LEGACY_BIN to a v2.17.0 chronik-server (search_after_archive upgrade case)}"
: "${PREV_BIN:?set PREV_BIN to the chronik-server of the previous release (tantivy_cross_version)}"
: "${FORMAT_OLD_BIN:?set FORMAT_OLD_BIN to a v2.19.x chronik-server (raw_segment_compression, reindex: plain raw segments, archives without JSON columns)}"
: "${GROUPS_OLD_BIN:?set GROUPS_OLD_BIN to a v2.18.x chronik-server (group_upgrade: an upgrade from before catalog group offsets)}"
export LEGACY_BIN
OUT="$REPO/tests/cluster/data/e2e/$LABEL"; mkdir -p "$OUT"
cd "$REPO"
failed=0
run() { # name, command...
  local name=$1; shift
  if [ -n "${E2E_FROM:-}" ]; then
    [ "$name" = "$E2E_FROM" ] || return 0
    E2E_FROM=
  fi
  local start=$SECONDS
  "$@" > "$OUT/$name.log" 2>&1; local rc=$?
  ./tests/cluster/stop.sh >/dev/null 2>&1
  # Some checks exit 0 when a prerequisite is missing; a skip is not a pass.
  grep -q 'SKIP:' "$OUT/$name.log" && rc=skip
  [ "$rc" = 0 ] || failed=$((failed + 1))
  printf '%-28s rc=%-4s %4ss  %s\n' "$name" "$rc" "$((SECONDS-start))" \
    "$(sed 's/\x1b\[[0-9;]*m//g' "$OUT/$name.log" | grep -E 'RESULT|== PASS|== FAIL|^PASS|^FAIL|PASS:|FAIL:|SKIP:|WRONG BASELINE' | tail -1 | cut -c1-100)"
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
run legacy_index_retirement "$E2E/legacy_index_retirement.sh" "$BIN" "$LABEL"
run archive_merge "$E2E/archive_merge.sh" "$BIN" "$LABEL"
run archive_merge_crash "$E2E/archive_merge_crash.sh" "$BIN" "$LABEL"
run archive_merge_tiers "$E2E/archive_merge_tiers.sh" "$BIN" "$LABEL"
run archive_upgrade "$E2E/archive_upgrade.sh" "$BIN" "$LABEL"
run tantivy_cross_version_up "$E2E/tantivy_cross_version.sh" "$PREV_BIN" "$BIN" "$LABEL-up"
run tantivy_cross_version_down "$E2E/tantivy_cross_version.sh" "$BIN" "$PREV_BIN" "$LABEL-down"
run raw_segment_compression "$E2E/raw_segment_compression.sh" "$BIN" "$FORMAT_OLD_BIN" "$LABEL"
run epoch_without_history "$E2E/epoch_without_history.sh" "$BIN" "$LABEL"
run acks0_no_response "$E2E/acks0_no_response.sh" "$BIN" "$LABEL"
run wal_write_failure "$E2E/wal_write_failure.sh" "$BIN" "$LABEL"
run wal_write_failure_acks1 env ACKS=1 "$E2E/wal_write_failure.sh" "$BIN" "$LABEL-acks1"
run hot_buffer_limit "$E2E/hot_buffer_limit.sh" "$BIN" "$LABEL"
run search_coverage "$E2E/search_coverage.sh" "$BIN" "$LABEL"
run search_aggregations "$E2E/search_aggregations.sh" "$BIN" "$LABEL"
run reindex "$E2E/reindex.sh" "$BIN" "$FORMAT_OLD_BIN" "$LABEL"
run archive_damage "$E2E/archive_damage.sh" "$BIN" "$LABEL"
run search_coverage_metric "$E2E/search_coverage_metric.sh" "$BIN" "$LABEL"
run search_throttle "$E2E/search_throttle.sh" "$BIN" "$LABEL"
run retention "$E2E/retention.sh" "$BIN" "$LABEL"
run retention_cluster "$E2E/retention_cluster.sh" "$BIN" "$LABEL"
run projected_columns "$E2E/projected_columns.sh" "$BIN" "$LABEL"
run columnar_catchup "$E2E/columnar_catchup.sh" "$BIN" "$LABEL"
run vector_catchup "$E2E/vector_catchup.sh" "$BIN" "$LABEL"
run create_race "$E2E/create_race.sh" "$BIN" "$LABEL"
run txn_acl "$E2E/txn_acl.sh" "$BIN" "$LABEL"
run query_fanout "$E2E/query_fanout.sh" "$BIN" "$LABEL"
run s3_roundtrip "$E2E/s3_roundtrip.sh" "$BIN" "$LABEL"
run dr_restore "$E2E/dr_restore.sh" "$BIN" "$LABEL"
run dr_cluster_restore "$E2E/dr_cluster_restore.sh" "$BIN" "$LABEL"
run group_coordination "$E2E/group_coordination.sh" "$BIN" "$LABEL"
run group_upgrade "$E2E/group_upgrade.sh" "$GROUPS_OLD_BIN" "$BIN" "$LABEL"
run node_removal "$E2E/node_removal.sh" "$BIN" "$LABEL"
run metadata_compaction "$E2E/metadata_compaction.sh" "$BIN" "$LABEL"
run unified_api_routes "$E2E/unified_api_routes.sh" "$BIN" "$LABEL"
run operator_kafka_admin "$E2E/operator_kafka_admin.sh" "$BIN" "$LABEL"
run tls_cluster "$E2E/tls_cluster.sh" "$BIN" "$LABEL"
run regression_replication ./tests/cluster/regression_replication.sh
run follower_catchup ./tests/cluster/follower_catchup.sh
run concurrent_produce_order ./tests/test_concurrent_produce_order.sh
run retry_offset_gaps ./tests/test_retry_offset_gaps.sh
run legacy_format_replication ./tests/test_legacy_format_replication.sh
run preferred_leader_return ./tests/test_preferred_leader_return.sh
run tantivy_index_archive ./tests/test_tantivy_index_archive.sh
echo "failed: $failed"
[ $failed = 0 ]
