#!/bin/bash
# Every acknowledged record of a searchable topic is searchable: in the hot
# index until an archive covers it, in the archive after.
#
# Up to v2.19.2 the hot index dropped records by count, not by coverage:
#   - each commit trimmed it to the newest CHRONIK_HOT_TEXT_MAX_DOCS offsets;
#   - once trims left as many deleted documents as twice that, it threw the
#     whole partition away, newest records included;
#   - after a restart it reloaded only the newest CHRONIK_HOT_TEXT_MAX_DOCS
#     records of the log.
# Archives trail the log by up to a minute (a segment is sealed after 30 s
# idle, then indexed), so under any burst larger than the cap, records in
# between answered nothing until the archive caught up.
#
# Here, on one node with a 2,000-record cap (50,000 hard): 20,000 records,
# each with its own word, then five of them spread over the range must be found
# within seconds, before any archive exists, with nothing counted as dropped
# unarchived; a second burst right after a crash restart; and both once
# archived.
#
# Usage: search_coverage.sh <binary> <label>
# Needs: kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/coverage-$LABEL; LOG=tests/cluster/logs/coverage-$LABEL
B=localhost:9092; T=coverage
N=20000; PROBES="0 5000 10000 15000 19999"
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
# found <n>: hits for the word only record n holds
found() {
  curl -s -m 30 -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' \
    -d "{\"query\":{\"match\":{\"value\":\"rec$1\"}},\"size\":5}" \
    | grep -o '"total":{"value":[0-9]*' | grep -o '[0-9]*$'
}
probes() { for p in $PROBES; do printf '%s=%s ' "$p" "$(found "$p")"; done | sed 's/ $//'; }
want=$(for p in $PROBES; do printf '%s=1 ' "$p"; done | sed 's/ $//')
unarchived() {
  curl -s -m 5 localhost:13092/metrics | grep '^chronik_hot_text_docs_total{op="evicted_unarchived"}' | awk '{ print $2 }'
}
archives() { find "$D" \( -name '*.tar' -o -name '*.tar.gz' \) -path "*$T*" 2>/dev/null | wc -l; }
start() {
  CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_HOT_TEXT_MAX_DOCS=2000 CHRONIK_HOT_TEXT_HARD_MAX_DOCS=50000 \
    "$BIN" start --data-dir "$D" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL: server did not start"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
start
trap stop EXIT
echo "warmup" | kcat -P -b $B -t $T -p 0 2>/dev/null # auto-create
sleep 3
seq 0 $((N - 1)) | awk '{ printf "gapword rec%d\n", $1 }' \
  | kcat -P -b $B -t $T -p 0 -X acks=1 -X batch.num.messages=200 -X linger.ms=20 2>/dev/null
sleep 3

echo "before any archive:"
expect "archives" "$(archives)" "0"
expect "records spread over the burst" "$(probes)" "$want"

expect "records dropped before archived (metric)" "$(unarchived)" "0"

# A second burst, then a crash within a second, then every record of it must
# answer as soon as the node is back. (On one small node the recovered segment
# is archived before the node is serving, so this does not isolate the boot
# warm-up; its start offset is unit-tested in the server.)
seq $N $((2 * N - 1)) | awk '{ printf "gapword rec%d\n", $1 }' \
  | kcat -P -b $B -t $T -p 0 -X acks=1 -X batch.num.messages=200 -X linger.ms=20 2>/dev/null
kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
start
PROBES="$N $((N + 5000)) $((N + 10000)) $((N + 15000)) $((2 * N - 1))"
want=$(for p in $PROBES; do printf '%s=1 ' "$p"; done | sed 's/ $//')
echo "right after a crash restart ($(archives) archive(s)):"
expect "second burst" "$(probes)" "$want"
PROBES="0 5000 10000 15000 $((2 * N - 1))"
want=$(for p in $PROBES; do printf '%s=1 ' "$p"; done | sed 's/ $//')

for _ in $(seq 36); do [ "$(archives)" -ge 1 ] && break; sleep 5; done
sleep 10 # the archived range is evicted from the hot index
echo "archived:"
expect "archive written" "$([ "$(archives)" -ge 1 ] && echo yes || echo no)" "yes"
expect "records spread over the burst" "$(probes)" "$want"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
