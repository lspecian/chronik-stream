#!/bin/bash
# Archives merge by size tier while data keeps arriving, and merged archives
# merge again: nothing lost, nothing doubled.
#
# Runs of `min_run` archives of one size tier merge into one of the next tier
# (up to v2.19.2 only archives under half a 64 MB target merged, so a busy
# partition's ~50 MB archives never did; that, the archive count and rewrite
# volume are covered by archive_merge's unit tests). This is the end-to-end
# safety check: merges running while new archives keep arriving, merges of
# merged archives, every record kept exactly once.
#
# Here, one node, a 4 KB tier base (so these small archives span several
# tiers), a merge pass every 5 s: 16 bursts of 2,000 records, each archived
# separately while earlier ones are being merged. Checks:
#   - fewer archives than bursts, and at least one merge of merged archives
#     (a merge whose sources came from more than one earlier merge);
#   - every burst's own word is found exactly once, wherever its archive went;
#   - a consumer reads every offset once.
#
# Usage: archive_merge_tiers.sh <binary> <label>
# Needs: kcat.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/tiers-$LABEL; LOG=tests/cluster/logs/tiers-$LABEL
B=localhost:9092; T=tiers; BURSTS=16; PER=2000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
archives() { find "$D" -path "*/$T/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l; }
found() { # word -> total hits
  curl -s -m 30 -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' \
    -d "{\"query\":{\"match\":{\"value\":\"$1\"}},\"size\":$PER,\"track_total_hits\":true}" | grep -o '"total":{"value":[0-9]*' | grep -o '[0-9]*$'
}

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_HOT_TEXT_ENABLED=false \
  CHRONIK_ARCHIVE_MERGE_TIER_BASE_BYTES=4096 CHRONIK_ARCHIVE_MERGE_DELAY_SECS=5 \
  CHRONIK_ARCHIVE_MERGE_INTERVAL_SECS=5 CHRONIK_ARCHIVE_MERGE_GRACE_SECS=2 \
  "$BIN" start --data-dir "$D" > "$LOG-server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT
for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && break; sleep 1; done

total=0
for b in $(seq 0 $((BURSTS - 1))); do
  seq 1 $PER | awk -v b=$b '{ printf "burstword%d line %d of the burst with some filler text %d\n", b, $1, $1 * 7 }' \
    | kcat -P -b $B -t $T -p 0 -X acks=1 2>/dev/null
  total=$((total + PER))
  # Wait for this burst's archive (the segment is sealed after 30 s idle).
  for _ in $(seq 40); do
    [ "$(found burstword$b)" = "$PER" ] && [ "$(sed 's/\x1b\[[0-9;]*m//g' "$LOG-server.log" | grep -c 'Created Tantivy index')" -gt "$b" ] && break
    sleep 3
  done
done
sleep 20 # last merge passes
n=$(archives)
merges=$(sed 's/\x1b\[[0-9;]*m//g' "$LOG-server.log" | grep -c 'merged archives')
echo "after $BURSTS bursts: $n archive(s), $merges merge(s)"
expect "fewer archives than bursts" "$([ "$n" -lt $BURSTS ] && echo yes || echo "no: $n")" "yes"
# A merge of merged archives spans more than 4 bursts' offsets.
second=$(sed 's/\x1b\[[0-9;]*m//g' "$LOG-server.log" | grep 'merged archives' \
  | grep -o 'min_offset=[0-9]* max_offset=[0-9]*' \
  | awk -F'[= ]' -v per=$PER '{ if ($4 - $2 + 1 > 4 * per) n++ } END { print n + 0 }')
expect "merged archives merged again" "$([ "$second" -ge 1 ] && echo yes || echo "no merge spanned more than 4 bursts")" "yes"

words=$(for b in $(seq 0 $((BURSTS - 1))); do printf '%s ' "$(found burstword$b)"; done | sed 's/ $//')
want=$(for b in $(seq 0 $((BURSTS - 1))); do printf '%s ' $PER; done | sed 's/ $//')
expect "each burst's word, every record once" "$words" "$want"
consumed=$(timeout 60 kcat -C -b $B -t $T -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | uniq | wc -l)
expect "a consumer reads every offset once" "$consumed" "$total"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
