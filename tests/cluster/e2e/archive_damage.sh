#!/bin/bash
# A damaged archive (a byte flipped in one of its index files) is found by its
# checksums, kept out of searches, and rebuilt from the raw tier, with nothing
# lost or doubled.
#
# One node, N records in three archives. Each case flips a byte in the
# largest index file of one archive while the node is down, then starts it:
#   1. the scrub finds it (no search touches it first); it is rebuilt: total,
#      word search and a range match the corpus; GET /admin/reindex lists
#      nothing damaged; one file per catalogue entry.
#   2. scrub off: the first search finds it (and answers without it, marked
#      incomplete); it is rebuilt; everything matches again.
#   3. its raw segments removed too: it cannot be rebuilt, so it stays listed
#      as damaged with the reason; searches still answer (the other
#      archives) and the node keeps serving.
#
# Usage: archive_damage.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, jq.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/archive-damage-$LABEL; LOG=tests/cluster/logs/archive-damage-$LABEL
B=localhost:9092; T=dmg; N=15000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
tests/search_bench/gen.sh "$D/corpus" $N $N >/dev/null
CORPUS="$D/corpus/bulk-00000.ndjson"
T0=1767225600000; LO=$((T0 + 100000)); HI=$((T0 + 400000))
want_range=$(grep -o '"ts":[0-9]*' "$CORPUS" | cut -d: -f2 | awk -v lo=$LO -v hi=$HI '$1 >= lo && $1 <= hi' | wc -l)
want_timeout=$(grep -cw timeout "$CORPUS")

start() { # extra env...
  env CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_HOT_TEXT_ENABLED=false \
    CHRONIK_ARCHIVE_MERGE=false CHRONIK_ARCHIVE_MERGE_GRACE_SECS=2 CHRONIK_ARCHIVE_REPAIR_INTERVAL_SECS=2 \
    CHRONIK_ARCHIVE_SCRUB_DELAY_SECS=1 CHRONIK_RETENTION=false "$@" \
    "$BIN" start --data-dir "$D/data" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL: server did not start"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }
search() { curl -s -m 60 -X POST "localhost:6092/$T/_search" -H 'Content-Type: application/json' -d "$1"; }
archives() { find "$D/data" -path "*/tantivy_indexes/$T/*" -name '*.tar' | sort; }
catalogued() { jq "[.[\"$T-0\"][]?] | length" "$D/data/segment_index.json"; }
damaged() { curl -s -m 10 localhost:6092/admin/reindex | jq -c '[.damaged[]?.archive | split("/") | last]'; }
# Flip one byte in the middle of the largest index file inside a tar archive.
corrupt() { # archive
  local best_off=0 best_len=0
  while read -r block size name; do
    case $name in index/*.json|index/*.lock|index/) continue;; index/*) ;; *) continue;; esac
    [ "$size" -gt "$best_len" ] && { best_len=$size; best_off=$(( (block + 1) * 512 )); }
  done < <(tar -tvRf "$1" | awk '/^block/ { b = $2; sub(":", "", b); print b, $5, $NF }')
  local at=$(( best_off + best_len / 2 ))
  local byte; byte=$(od -An -tu1 -j $at -N1 "$1" | tr -d ' ')
  printf "$(printf '\\%03o' $(( byte ^ 255 )))" | dd of="$1" bs=1 seek=$at conv=notrunc status=none
  echo "  flipped a byte at $at of $(basename "$1") (index file of $best_len bytes)"
}
wait_repaired() { # rebuilds expected in the log so far
  for _ in $(seq 30); do [ "$(damaged)" = "[]" ] && [ "$(grep -c "damaged archive rebuilt" "$LOG-server.log")" -ge "$1" ] && return; sleep 1; done
}
check() { # when
  echo "$1:"
  expect "total" "$(search '{"size":0}' | jq '.hits.total.value')" "$N"
  expect "range" "$(search "{\"size\":0,\"query\":{\"range\":{\"ts\":{\"gte\":$LO,\"lte\":$HI}}}}" | jq '.hits.total.value')" "$want_range"
  expect "word search" "$(search '{"size":0,"query":{"match":{"message":"timeout"}}}' | jq '.hits.total.value')" "$want_timeout"
  expect "nothing damaged" "$(damaged)" "[]"
  sleep 3 # grace: the replaced file goes
  expect "one file per catalogue entry" "$(archives | wc -l) $(catalogued)" "3 3"
}

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh
trap stop EXIT
start
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
for part in 1 2 3; do
  sed -n "$(( (part - 1) * N / 3 + 1 )),$(( part * N / 3 ))p" "$CORPUS" | kcat -P -b $B -t $T -p 0 2>/dev/null
  for _ in $(seq 40); do [ "$(archives | wc -l)" -ge $part ] && break; sleep 3; done
done
expect "archives" "$(archives | wc -l)" "3"
check "before any damage"
stop

echo "case 1: found by the scrub"
corrupt "$(archives | sed -n 2p)"
start
wait_repaired 1
expect "scrub found it" "$(grep -c 'archive is damaged' "$LOG-server.log")" "1"
check "after the repair"
stop

echo "case 2: found by a search (scrub off)"
corrupt "$(archives | sed -n 1p)"
start CHRONIK_ARCHIVE_SCRUB=false CHRONIK_ARCHIVE_REPAIR_INTERVAL_SECS=5
first=$(search '{"size":0}' | jq -c '[.hits.total.value < '"$N"', .hits.total.relation]')
expect "first search answers without it, as incomplete" "$first" '[true,"gte"]'
wait_repaired 2
check "after the repair"
stop

echo "case 3: cannot be rebuilt (raw segments gone)"
victim=$(archives | sed -n 3p)
corrupt "$victim"
lo=$(basename "$victim" | sed -E 's/segment-([0-9]+)-([0-9]+).*/\1/'); hi=$(basename "$victim" | sed -E 's/segment-([0-9]+)-([0-9]+).*/\2/')
for f in $(find "$D/data" -path "*/segments/segments/$T/*" -type f); do
  b=$(basename "$f" | grep -oE '[0-9]+-[0-9]+' | head -1); [ -z "$b" ] && continue
  s=${b%-*}; e=${b#*-}; [ "$e" -ge "$lo" ] && [ "$s" -le "$hi" ] && rm -f "$f"
done
start
for _ in $(seq 20); do grep -q "cannot be rebuilt" "$LOG-server.log" && break; sleep 1; done
expect "listed as damaged" "$(damaged)" "[\"$(basename "$victim")\"]"
expect "reason given" "$(curl -s localhost:6092/admin/reindex | jq -r '.damaged[0].reason' | grep -c checksum)" "1"
expect "searches still answer, as incomplete" "$(search '{"size":0}' | jq -c '[.hits.total.value > 0, .hits.total.relation]')" '[true,"gte"]'
expect "still serving" "$(curl -s -o /dev/null -w '%{http_code}' localhost:6092/health)" "200"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
