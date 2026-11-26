#!/bin/bash
# The same text scores the same in every archive, so equal matches rank
# newest first across archives.
#
# Up to v2.20.1 each Tantivy index (every archive, every hot partition) scored
# with its own statistics. An archive holding many records without the query
# term gave it a higher IDF, so the identical message scored higher there: on
# 10M records `timeout` ranked the newest 20,136 matches below a million-
# offset-older archive's, and `level: ERROR` answered with records 3.5M
# offsets old. Every index now scores with the topic's statistics
# (crate::corpus_stats).
#
# One partition, filled in 4 batches, each archived before the next: batch k
# holds F_k filler records ("routine heartbeat ok") then 20 "disk timeout"
# records, with F = 2000, 1000, 500, 100, so per-archive statistics favour the
# OLDEST copies. A search for "timeout" must return batch 4's 20 matches first,
# newest first, all with one score; page 2 continues from there. Checked with
# the hot index on, and after a restart with it off (archives only).
#
# Usage: search_scoring_across_archives.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092
cd "$REPO"
fail=0
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
expect() { # what actual expected
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
archives() { find $C/data -path "*/$T/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l; }
search() { # from size -> "_id score" per line
  curl -s -m 60 -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' \
    -d "{\"query\":{\"match\":{\"message\":\"timeout\"}},\"from\":$1,\"size\":$2}" \
    | grep -o '"_id":"[^"]*"[^}]*"_score":[0-9.e-]*' | sed -E 's/"_id":"([^"]*)".*"_score":(.*)/\1 \2/'
}

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
T=sc-$LABEL
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 3 >/dev/null 2>&1
sleep 5

offset=0
k=0
for fillers in 2000 1000 500 100; do
  k=$((k + 1))
  {
    seq $fillers | awk -v b=$k '{ printf "{\"message\":\"routine heartbeat ok\",\"batch\":%d}\n", b }'
    seq 20 | awk -v b=$k '{ printf "{\"message\":\"disk timeout\",\"batch\":%d}\n", b }'
  } | kcat -P -b $B -t $T -p 0 -X acks=all 2>/dev/null
  offset=$((offset + fillers + 20))
  # Every node archives its copy of the batch before the next is produced.
  for _ in $(seq 90); do [ "$(archives)" -ge $((k * 3)) ] && break; sleep 2; done
  echo "batch $k: $fillers fillers + 20 matches, archives on disk: $(archives)"
done
[ "$(archives)" -ge 12 ] || { echo "  FAIL: expected 4 archives per node"; fail=1; }

newest=$((offset - 1)) # the last "disk timeout" record
want_p1=$(seq $newest -1 $((newest - 9)) | sed 's/^/0-/' | paste -sd' ' -)
want_p2=$(seq $((newest - 10)) -1 $((newest - 19)) | sed 's/^/0-/' | paste -sd' ' -)
round() { # label
  echo "$1:"
  local p1 p2
  p1=$(search 0 10); p2=$(search 10 10)
  expect "page 1 = the 10 newest matches" "$(cut -d' ' -f1 <<<"$p1" | paste -sd' ' -)" "$want_p1"
  expect "page 2 = the next 10" "$(cut -d' ' -f1 <<<"$p2" | paste -sd' ' -)" "$want_p2"
  expect "one score for the same text" "$(cat <(echo "$p1") <(echo "$p2") | cut -d' ' -f2 | sort -u | wc -l)" "1"
}
sleep 10
round "archived, hot index on"

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
sleep 15
round "restarted, hot index off"

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
