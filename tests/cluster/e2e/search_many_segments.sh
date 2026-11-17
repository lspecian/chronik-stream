#!/bin/bash
# A search across every topic answers on a node with many archived segments,
# and the unpacked copies it keeps stay within their budget.
#
# v2.17.1 and earlier opened every archived segment of every topic one after
# another, unpacking each into /tmp/chronik-cold-search and never deleting it.
# A production node with 6,000-13,000 archives never answered a `/_search`
# naming no topic, and each attempt left more unpacked copies behind (14 GB
# from two cut-off requests). A request `timeout` was ignored.
#
# Here: 30 searchable topics, so every node holds 30+ archives, searched cold
# (hot index off, fresh cache after a restart):
#   - a 1ms `timeout` answers at once, 200, with `timed_out: true`;
#   - match_all across every topic answers with a full page;
#   - a term in one record of one topic is found by a search naming no topic;
#   - the cache lives in each node's data dir and stays within
#     CHRONIK_SEARCH_CACHE_BYTES.
#
# Usage: search_many_segments.sh <binary> <label>
# Needs: KAFKA_HOME, kcat.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
C=tests/cluster; B=localhost:9092
cd "$REPO"
fail=0
TOPICS=30
BUDGET=$((400 * 1024))
up() { for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
expect() { # what actual expected
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
# search <body> -> "<http code> <timed_out> <hits returned> <seconds>"
search() {
  local out code json
  out=$(curl -s -m 60 -w '\n%{http_code} %{time_total}' -X POST localhost:6092/_search \
    -H 'Content-Type: application/json' -d "$1")
  code=${out##*$'\n'}; json=${out%$'\n'*}
  local to n
  to=$(grep -o '"timed_out":[a-z]*' <<<"$json" | head -1 | cut -d: -f2)
  n=$(grep -o '"_id":' <<<"$json" | wc -l)
  echo "${code%% *} ${to:--} $n ${code##* }"
}

echo "=== $LABEL ($TEST_BIN)"
$C/stop.sh >/dev/null 2>&1
CHRONIK_BIN=$TEST_BIN $C/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed (cluster not (re)started)"; exit 1; }
up || { echo "FAIL: cluster did not start"; exit 1; }
P=sms-$LABEL
for t in $(seq 1 $TOPICS); do
  $K/kafka-topics.sh --bootstrap-server $B --create --topic $P-$t --partitions 1 --replication-factor 3 >/dev/null 2>&1
done
sleep 5
for t in $(seq 1 $TOPICS); do
  seq 1 300 | awk -v t=$t '{
      m = (t == 17 && $1 == 123) ? "zephyrquandary" : "ordinary";
      printf "{\"id\":%d,\"topic\":%d,\"marker\":\"%s\",\"pad\":\"%0200d\"}\n", $1, t, m, 0 }' \
    | kcat -P -b $B -t $P-$t -p 0 -X acks=all 2>/dev/null
done
echo "produced $TOPICS topics x 300 records; waiting for every node to archive them"
for _ in $(seq 72); do
  n=$(find $C/data -path "*/$P-*" -name '*.tar.gz' 2>/dev/null | wc -l)
  [ "$n" -ge $((TOPICS * 3)) ] && break
  sleep 5
done
n=$(find $C/data -path "*/$P-*" -name '*.tar.gz' 2>/dev/null | wc -l)
echo "archives on disk: $n"
[ "$n" -ge $((TOPICS * 3)) ] || { echo "  FAIL: fewer than $((TOPICS * 3)) archives after 6 minutes"; fail=1; }

# Cold: hot index off, and a restart empties the cache.
CHRONIK_HOT_TEXT_ENABLED=false CHRONIK_SEARCH_CACHE_BYTES=$BUDGET CHRONIK_BIN=$TEST_BIN $C/restart.sh >/dev/null 2>&1 || { echo "FAIL: restart.sh failed (cluster not (re)started)"; exit 1; }
up || { echo "FAIL: cluster did not restart"; exit 1; }
sleep 15

echo "time limit:"
r=$(search '{"query":{"match":{"marker":"zephyrquandary"}},"size":10,"timeout":"1ms"}')
echo "  ($r)"
expect "1ms timeout answers 200 with timed_out" "$(cut -d' ' -f1-2 <<<"$r")" "200 true"
expect "and at once" "$(awk '{print ($4 < 5) ? "under 5s" : $4 "s"}' <<<"$r")" "under 5s"

echo "across every topic:"
r=$(search '{"query":{"match_all":{}},"size":5}')
echo "  ($r)"
expect "match_all fills the page" "$(cut -d' ' -f1-3 <<<"$r")" "200 false 5"
r=$(search '{"query":{"match":{"marker":"zephyrquandary"}},"size":10}')
echo "  ($r)"
expect "one record in one of $TOPICS topics" "$(cut -d' ' -f1-3 <<<"$r")" "200 false 1"
body=$(curl -s -m 60 -X POST localhost:6092/_search -H 'Content-Type: application/json' \
  -d '{"query":{"match":{"marker":"zephyrquandary"}},"size":10}')
expect "from the right topic and offset" \
  "$(grep -o "\"_index\":\"[^\"]*\"\|\"offset\":[0-9]*" <<<"$body" | sort -u | paste -sd' ' -)" \
  "\"_index\":\"$P-17\" \"offset\":122"

echo "cache:"
for node in 1 2 3; do
  d=$C/data/node$node/search-cache
  if [ -d "$d" ]; then
    sz=$(du -sb "$d" | cut -f1)
    # One segment may be in use past the budget while the next is unpacked.
    expect "node$node cache within budget" "$([ "$sz" -le $((BUDGET * 2)) ] && echo within || echo "$sz bytes")" "within"
    # The term search read all $TOPICS of this node's segments, which do not
    # fit in the budget: some must have been evicted.
    kept=$(find "$d" -mindepth 1 -maxdepth 1 -type d ! -name '.evicted-*' | wc -l)
    expect "node$node evicted to fit" "$([ "$kept" -lt $TOPICS ] && echo evicted || echo "kept all $kept")" "evicted"
  else
    expect "node$node cache in its data dir" "missing" "present"
  fi
done

$C/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
