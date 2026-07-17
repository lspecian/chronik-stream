#!/usr/bin/env bash
# Scale regression test: bulk topic creation must not OOM nodes, must not lose
# topic creations, and the catalog must converge across all nodes after restart.
#
# Guards two fixed bugs:
#   1. Per-topic Tantivy writer OOM (commit 0e37747): nodes were kernel-OOM-killed
#      mid-load at ~1200 topics (17.9GB peak); the failing tail of creations was
#      simply the dead broker refusing connections.
#   2. Metadata catalog divergence (commit 954dec9): the 1000-slot event bus
#      dropped TopicCreated during bulk re-broadcast, so nodes recovered wildly
#      different catalogs after restart (observed 15 / 4695 / 882 on k8s).
#
# Usage: ./tests/cluster/regression_scale.sh [NTOPICS]
# Env:   REG_MAX_NODE_RSS_MB   per-node peak RSS budget (default 6000)
#        REG_TOPICS            same as argv1 (default 2000)
#
# Requires: kcat, a 3-node-capable box (~4GB free RAM at default size).
# PASS criteria:
#   - all N topics created (no creation loss)
#   - 3/3 nodes alive after the load (no OOM)
#   - per-node peak RSS below budget
#   - after restart: every node's catalog converges to exactly N
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
N="${1:-${REG_TOPICS:-2000}}"
MAX_RSS_MB="${REG_MAX_NODE_RSS_MB:-6000}"
PREFIX="regscale"
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }

say "== regression_scale: $N topics, per-node RSS budget ${MAX_RSS_MB}MB =="
"$SCRIPT_DIR/stop.sh" >/dev/null 2>&1
"$SCRIPT_DIR/start.sh" >/dev/null 2>&1 &
sleep 25
alive=$(pgrep -cf 'chronik-server.*node[123].toml')
[ "$alive" -eq 3 ] || { say "SKIP: cluster failed to start ($alive/3)"; exit 1; }

# RSS sampler (per-node peaks)
RSS_LOG=$(mktemp)
( while true; do
    ps -o rss=,args= -C chronik-server 2>/dev/null | grep -E 'node[123].toml' \
      | awk '{print $1}' | paste -sd' ' >> "$RSS_LOG"
    sleep 5
  done ) &
SAMPLER=$!

say "-- creating $N topics (4 msgs each, 16-way parallel)"
seq 1 "$N" | xargs -P 16 -I {} sh -c \
  'printf "a\nb\nc\nd\n" | kcat -b localhost:9092 -t '"$PREFIX"'-{} -P 2>/dev/null'
sleep 15
kill "$SAMPLER" 2>/dev/null

# 1. No OOM: all nodes alive
alive=$(pgrep -cf 'chronik-server.*node[123].toml')
[ "$alive" -eq 3 ] || fail "nodes alive after load: $alive/3 (OOM regression?)"

# 2. RSS budget
peaks=$(awk '{for(i=1;i<=NF;i++) if($i>m[i]) m[i]=$i} END{for(i=1;i<=3;i++) printf "%.0f ", m[i]/1024}' "$RSS_LOG")
say "-- peak RSS per node (MB): $peaks"
for p in $peaks; do
  [ "${p%.*}" -le "$MAX_RSS_MB" ] || fail "node peak RSS ${p}MB > budget ${MAX_RSS_MB}MB"
done
rm -f "$RSS_LOG"

# 3. No creation loss
created=$(timeout 20 kcat -b localhost:9092 -L 2>/dev/null | grep -cE "^\s+topic \"$PREFIX-")
say "-- created: $created/$N"
[ "$created" -eq "$N" ] || fail "creation loss: $created/$N topics registered"

# 4. Catalog convergence after restart
say "-- restarting cluster (cold-start recovery)"
"$SCRIPT_DIR/restart.sh" >/dev/null 2>&1
say "-- waiting past both catch-up broadcasts (150s)"
sleep 150
for port in 9092 9093 9094; do
  c=$(timeout 20 kcat -b localhost:$port -L 2>/dev/null | grep -cE "^\s+topic \"$PREFIX-")
  say "-- post-restart catalog on :$port = $c"
  [ "$c" -eq "$N" ] || fail "catalog divergence on :$port after restart: $c/$N"
done

"$SCRIPT_DIR/stop.sh" >/dev/null 2>&1
if [ "$FAIL" -eq 0 ]; then
  say "== PASS: no OOM, no creation loss, catalog converged on all nodes =="
else
  say "== FAIL: see messages above =="
fi
exit "$FAIL"
