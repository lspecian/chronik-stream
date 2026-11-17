#!/bin/bash
# Indexes left by the removed RealtimeIndexer ({data_dir}/index/<topic>) are
# retired: records the log still holds but no archive covers are re-archived,
# the rest is dropped with the old index.
#
# Setup (on node 1): topic R's middle archive (offsets 100-199) is removed from
# every node — the gap the old archiver left at offset 0 of every early topic —
# and an old-format index covers R's offsets 0-299; a second old-format index
# belongs to a topic that does not exist (a deleted topic's leftover).
#
# Checks: before retirement the dead index answers searches (that is the bug:
# deleted data is searchable); after it, both old indexes are gone, the dead
# one's records are no longer searchable, R's gap is searchable again from the
# re-archived records, R pages through all 300 records once, and a consumer
# still reads offsets 0-299 exactly once.
#
# Usage: legacy_index_retirement.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, TOOL (target/release/examples/legacy_index_tool,
# built by `cargo build --release -p chronik-search --example legacy_index_tool`).
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
TOOL=${TOOL:-$REPO/target/release/examples/legacy_index_tool}
[ -x "$TOOL" ] || { echo "FAIL: build the tool: cargo build --release -p chronik-search --example legacy_index_tool"; exit 1; }
C=tests/cluster; B=localhost:9092
cd "$REPO"
fail=0
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
total() { # topic query -> "<http code> <total>"
  local out; out=$(curl -s -m 60 -w '\n%{http_code}' -X POST "localhost:6092/$1/_search" \
    -H 'Content-Type: application/json' -d "{\"query\":$2,\"size\":50}")
  echo "${out##*$'\n'} $(grep -o '"total":{"value":[0-9]*' <<<"${out%$'\n'*}" | grep -o '[0-9]*$')"
}
archives() { find $C/data -path "*/$R/*" -name '*.tar.gz' 2>/dev/null | wc -l; }
produce() { # first last
  seq "$1" "$2" | awk '{ w = ($1 == 151) ? "gapword" : "plain"; printf "{\"id\":%d,\"w\":\"%s\"}\n", $1, w }' \
    | kcat -P -b $B -t $R -p 0 -X acks=all 2>/dev/null
}
wait_archives() { # count
  for _ in $(seq 90); do [ "$(archives)" -ge "$1" ] && return 0; sleep 5; done
  echo "  FAIL: fewer than $1 archives of $R"; fail=1; return 1
}

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
R=lir-$LABEL
$K/kafka-topics.sh --bootstrap-server $B --create --topic $R --partitions 1 --replication-factor 3 >/dev/null 2>&1
sleep 5
# Three archive runs, so each node holds archives 0-99, 100-199, 200-299.
produce 1 100; wait_archives 3
produce 101 200; wait_archives 6
produce 201 300; wait_archives 9
echo "archives of $R: $(archives)"
$C/stop.sh >/dev/null 2>&1

# The gap: the archive holding offset 150 removed on every node.
for n in 1 2 3; do
  path=$("$TOOL" drop-archive $C/data/node$n/segment_index.json $R 0 150) || { echo "  FAIL: node$n has no archive at 150"; fail=1; continue; }
  case "$path" in /*) rm -f "$path" "$path.terms" ;; *) rm -f "$C/data/node$n/$path" "$C/data/node$n/$path.terms" ;; esac
done
# The old indexes, on node 1: R's (covering the gap) and a deleted topic's.
G=ghost-$LABEL
"$TOOL" make-legacy $C/data/node1/index/$R $R 0 0 299 >/dev/null
"$TOOL" make-legacy $C/data/node1/index/$G $G 0 0 49 >/dev/null

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_LEGACY_INDEX_RETIREMENT_DELAY_SECS=45 CHRONIK_BIN=$TEST_BIN \
  $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
sleep 10
echo "before retirement:"
expect "a deleted topic's old index answers searches" "$(total $G '{"match_all":{}}')" "200 50"

for _ in $(seq 60); do [ -d $C/data/node1/index/$R ] || [ -d $C/data/node1/index/$G ] || break; sleep 2; done
sleep 5
echo "after retirement:"
expect "both old indexes deleted" "$(ls $C/data/node1/index 2>/dev/null | wc -l)" "0"
grep -h 'retired old RealtimeIndexer index' $C/logs/node1.log | sed 's/\x1b\[[0-9;]*m//g' | grep -o 'topic=[^ ]* records=[0-9]* uncovered=[0-9]* rearchived=[0-9]* dropped=[0-9]*' | sed 's/^/  log: /'
expect "the deleted topic's records are gone from search" "$(total $G '{"match_all":{}}' | cut -d' ' -f2)" "0"
expect "the gap's records are searchable from the log again" "$(total $R '{"match":{"w":"gapword"}}')" "200 1"
ids=$(curl -s -m 60 -X POST "localhost:6092/$R/_search" -H 'Content-Type: application/json' -d '{"query":{"match_all":{}},"size":300}' | grep -o '"_id":"[^"]*"' | sort -u | wc -l)
expect "all 300 records searchable, each once" "$ids" "300"
consumed=$(timeout 60 kcat -C -b $B -t $R -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | uniq | wc -l)
expect "a consumer reads offsets 0-299 once each" "$consumed" "300"
dups=$(timeout 60 kcat -C -b $B -t $R -p 0 -o beginning -e -q -f '%o\n' 2>/dev/null | sort -n | uniq -d | wc -l)
expect "no offset delivered twice" "$dups" "0"

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
