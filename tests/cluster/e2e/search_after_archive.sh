#!/bin/bash
# Full-text search keeps answering once records are archived.
#
# v2.17.0 and earlier, on a searchable topic of JSON records:
#   - a field query (`match: {marker: x}`) matched while the record sat in the
#     hot index (which ignored the field name) and found nothing once the
#     record was archived (the archive had no per-field index);
#   - `hits.total` counted every replica: RF=3 reported 3 for one record;
#   - with the hot index off, an archived topic with no match for the query
#     answered 404 "no such index".
#   - match_all returned 0 hits on a hot topic (#65).
# And an archive written by an older binary must stay searchable after the
# upgrade (it has no per-field index; field queries fall back to full text).
#
# Usage: search_after_archive.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, LEGACY_BIN (a v2.17.0 chronik-server, whose archives
# predate the per-field index).
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
LEGACY_BIN="${LEGACY_BIN:?set LEGACY_BIN to a v2.17.0 chronik-server (archives without the per-field index)}"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092
cd "$REPO"
fail=0
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }

# search <topic> <query-json> -> "<http code> <total> <relation> <offsets,...>"
search() {
  local body
  body=$(curl -s -m 30 -w '\n%{http_code}' -X POST "localhost:6092/$1/_search" \
    -H 'Content-Type: application/json' -d "{\"query\":$2,\"size\":50}")
  local code=${body##*$'\n'} json=${body%$'\n'*}
  local total rel offs
  total=$(grep -o '"total":{"value":[0-9]*' <<<"$json" | grep -o '[0-9]*$')
  rel=$(grep -o '"relation":"[a-z]*"' <<<"$json" | head -1 | cut -d'"' -f4)
  offs=$(grep -o '"offset":[0-9]*' <<<"$json" | cut -d: -f2 | sort -n | uniq | paste -sd, -)
  echo "$code ${total:--} ${rel:--} ${offs:--}"
}

# expect <what> <actual> <expected>
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}

# A topic of 3000 JSON records, RF3: record 42 (offset 41) holds the word in
# `marker`, record 43 (offset 42) holds it in `note`.
make_topic() { # topic
  $K/kafka-topics.sh --bootstrap-server $B --create --topic "$1" --partitions 1 --replication-factor 3 >/dev/null 2>&1
  $K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name "$1" --add-config searchable=true >/dev/null 2>&1
  sleep 5
  seq 1 3000 | awk '{
      m = ($1 == 42) ? "quixoticbellwether" : "ordinary";
      n = ($1 == 43) ? "the quixoticbellwether was mentioned" : "nothing to see";
      printf "{\"id\":%d,\"marker\":\"%s\",\"note\":\"%s\"}\n", $1, m, n }' \
    | kcat -P -b $B -t "$1" -p 0 -X acks=all 2>/dev/null
}

wait_archived() { # topic: every node has archived the segment, plus time for hot eviction
  for _ in $(seq 60); do
    [ "$(find $C/data -path "*/$1/*" -name '*.tar.gz' 2>/dev/null | wc -l)" -ge 3 ] && { sleep 20; return 0; }
    sleep 5
  done
  echo "  FAIL $1: no archive on all three nodes within 5 minutes"; fail=1; return 1
}

M='{"match":{"marker":"quixoticbellwether"}}'
V='{"match":{"value":"quixoticbellwether"}}'
NONE='{"match":{"marker":"zzzunmatchedzzz"}}'

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1
up || { echo "FAIL: cluster did not start"; exit 1; }
T=saa-$LABEL
make_topic $T
sleep 5
echo "hot:"
expect "field match" "$(search $T "$M")" "200 1 eq 41"
expect "value match" "$(search $T "$V")" "200 2 eq 41,42"
expect "match_all fills the page" "$(search $T '{"match_all":{}}' | cut -d" " -f1-3)" "200 50 gte"
wait_archived $T
echo "archived:"
expect "field match" "$(search $T "$M")" "200 1 eq 41"
expect "field match, other field" "$(search $T '{"match":{"note":"quixoticbellwether"}}')" "200 1 eq 42"
expect "value match counts each record once" "$(search $T "$V")" "200 2 eq 41,42"
expect "no match" "$(search $T "$NONE")" "200 0 eq -"
expect "match_all fills the page" "$(search $T '{"match_all":{}}' | cut -d" " -f1-3)" "200 50 gte"

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1
up || { echo "FAIL: cluster did not restart"; exit 1; }
sleep 20
echo "restarted, hot index off:"
expect "field match" "$(search $T "$M")" "200 1 eq 41"
expect "no match is 0 hits, not 404" "$(search $T "$NONE")" "200 0 eq -"

echo "=== archive written by $LEGACY_BIN, searched after the upgrade"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$LEGACY_BIN $C/start.sh >/dev/null 2>&1
up || { echo "FAIL: legacy cluster did not start"; exit 1; }
L=saal-$LABEL
make_topic $L
wait_archived $L
CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1
up || { echo "FAIL: upgraded cluster did not start"; exit 1; }
sleep 20
# No per-field index in a legacy archive: the field query searches the whole
# record, so both records holding the word match.
expect "legacy archive, field match (full-text fallback)" "$(search $L "$M")" "200 2 eq 41,42"
expect "legacy archive, value match" "$(search $L "$V")" "200 2 eq 41,42"

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
