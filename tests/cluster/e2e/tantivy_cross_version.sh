#!/bin/bash
# Archived search indexes written by one build answer identically on another.
#
# For a Tantivy upgrade (or any change to how archives are read): archives a
# cluster on <writer> builds must give the reader the same answers, hit for hit
# and in the same order, so a rolling upgrade (old writer, new reader) and a
# rollback (new writer, old reader) both keep search working.
#
#   1. Three nodes on <writer>; a searchable topic of 20,000 JSON records whose
#      words have skewed frequencies (ranking matters), archived on every node.
#   2. With the hot index off (every answer comes from the archives), a set of
#      queries is recorded: a rare field match, common words (top 50 in rank
#      order with totals), boolean must/must_not and should, match_all, no match.
#   3. The cluster restarts on <reader> over the same data: the same queries
#      must give the same answers. Mean query latency is printed for both.
#
# Usage: tantivy_cross_version.sh <writer-binary> <reader-binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
WRITER=${1:?writer binary}; READER=${2:?reader binary}; LABEL=${3:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092
cd "$REPO"
OUT="$C/data/tcv-$LABEL"
fail=0
for b in "$WRITER" "$READER"; do [ -x "$b" ] || { echo "SKIP: $b is not executable"; exit 77; }; done
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
trap '$C/stop.sh >/dev/null 2>&1' EXIT
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}

T=tcv-$LABEL
QUERIES=(
  'rare field|{"match":{"marker":"quixoticbellwether"}}'
  'common word|{"match":{"value":"alpha"}}'
  'mid word|{"match":{"value":"gamma"}}'
  'two words|{"match":{"value":"beta delta"}}'
  'boolean|{"bool":{"must":[{"match":{"value":"alpha"}}],"must_not":[{"match":{"value":"beta"}}]}}'
  'either word|{"bool":{"should":[{"match":{"value":"gamma"}},{"match":{"value":"delta"}}]}}'
  'match_all|{"match_all":{}}'
  'no match|{"match":{"value":"zzzunmatchedzzz"}}'
)

# answer <query-json> -> "<http> <total> <relation> <offsets in rank order>"
answer() {
  local body code json total rel offs
  body=$(curl -s -m 30 -w '\n%{http_code}' -X POST "localhost:6092/$T/_search" \
    -H 'Content-Type: application/json' -d "{\"query\":$1,\"size\":50}")
  code=${body##*$'\n'}; json=${body%$'\n'*}
  total=$(grep -o '"total":{"value":[0-9]*' <<<"$json" | grep -o '[0-9]*$')
  rel=$(grep -o '"relation":"[a-z]*"' <<<"$json" | head -1 | cut -d'"' -f4)
  offs=$(grep -o '"offset":[0-9]*' <<<"$json" | cut -d: -f2 | paste -sd, -)
  echo "$code ${total:--} ${rel:--} ${offs:--}"
}

# record <file>: every query's answer, then 20 rounds for latency.
record() {
  : > "$1"
  for q in "${QUERIES[@]}"; do echo "${q%%|*}: $(answer "${q#*|}")" >> "$1"; done
  local start; start=$(date +%s%N)
  for _ in $(seq 20); do for q in "${QUERIES[@]}"; do answer "${q#*|}" >/dev/null; done; done
  echo "  mean query latency: $(( ($(date +%s%N) - start) / 1000000 / (20 * ${#QUERIES[@]}) )) ms"
}

echo "=== $LABEL: written by $WRITER, read by $READER"
rm -rf "$OUT"; mkdir -p "$OUT"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$WRITER $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 3 --replication-factor 3 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name $T --add-config searchable=true >/dev/null 2>&1
sleep 5
# Word frequencies: alpha 1/2, beta 1/3, gamma 1/5, delta 1/7 of records;
# "gamma delta" adjacent in some; one record carries the rare marker.
seq 1 20000 | awk '{
    w = "";
    if ($1 % 2 == 0) w = w " alpha";
    if ($1 % 3 == 0) w = w " beta";
    if ($1 % 5 == 0) w = w (($1 % 35 == 0) ? " gamma delta" : " gamma");
    else if ($1 % 7 == 0) w = w " delta filler";
    m = ($1 == 12345) ? "quixoticbellwether" : "ordinary";
    printf "%d|{\"id\":%d,\"marker\":\"%s\",\"value\":\"rec%d%s\"}\n", $1, $1, m, $1, w }' \
  | kcat -P -K '|' -b $B -t $T -X acks=all 2>/dev/null

archived=0
for _ in $(seq 90); do
  [ "$(find $C/data -path "*/$T/*" \( -name '*.tar' -o -name '*.tar.gz' \) 2>/dev/null | wc -l)" -ge 9 ] && { archived=1; break; }
  sleep 5
done
expect "every partition archived on every node" "$archived" "1"

# Answers from the archives alone.
CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_BIN=$WRITER $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
sleep 20
echo "-- writer ($WRITER), hot index off"
record "$OUT/writer.txt"
expect "rare field match found" "$(grep '^rare field:' "$OUT/writer.txt" | cut -d' ' -f3-4)" "200 1"
expect "match_all fills a page" "$(grep '^match_all:' "$OUT/writer.txt" | awk '{print $NF}' | tr ',' '\n' | grep -c .)" "50"

CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_BIN=$READER $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed"; exit 1; }
up || { echo "FAIL: cluster did not restart on the reader"; exit 1; }
sleep 20
echo "-- reader ($READER), same data, hot index off"
record "$OUT/reader.txt"

while IFS= read -r line; do
  name=${line%%:*}
  expect "same answer: $name" "$(grep -F "$name:" "$OUT/reader.txt" | head -1 | md5sum | cut -c1-8)" "$(echo "$line" | md5sum | cut -c1-8)"
done < "$OUT/writer.txt"
diff "$OUT/writer.txt" "$OUT/reader.txt" >/dev/null || { echo "  differences:"; diff "$OUT/writer.txt" "$OUT/reader.txt" | head -10 | cut -c1-200; }
expect "panics" "$(cat $C/logs/node*.log | grep -c panicked)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
