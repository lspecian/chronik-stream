#!/bin/sh
# Verify distributed /_sql merges on a running multi-node cluster (issue #55).
#
# WHY THIS EXISTS: these merges were once "verified on a real three-node cluster
# over 900 rows whose true values were known" while eight of them were wrong.
# Two separate reasons, and both are worth keeping in mind:
#
#   1. Every node caps its /_sql reply at CHRONIK_SQL_ROW_LIMIT (default 1000).
#      Below that cap a fan-out behaves exactly like a single node, so node
#      truncation and the cap/page interaction cannot be reached at all. That is
#      why this script REQUIRES more rows in one partition than the cap and
#      exits 2 rather than skipping when it cannot get them.
#   2. The rest were simply never asked. No query in that run had an ORDER BY
#      with a LIMIT, an unaliased aggregate, or a HAVING — so a top-N built from
#      an arbitrary slice of each node's rows went unnoticed at any size. Hence
#      the breadth here: every merge shape, and every one of them unaliased,
#      because the coordinator's name for a column is not the engine's.
#
# Two checks need care to stay honest. MIN/MAX runs over CAST(_partition AS
# VARCHAR), a value each node alone holds: every node carries every replica, so
# MIN/MAX over shared values agrees by luck even when the merge keeps whichever
# node answered first. And ORDER BY COUNT(*) is checked ASC as well as DESC,
# because partition counts usually descend naturally — DESC alone passes
# without the sort ever running.
#
# Ground truth is derived from Kafka's own end offsets, never hardcoded, so this
# works against any cluster and any dataset of the right size.
#
# To create a suitable topic (3 partitions, >1000 rows each), from a pod with
# the Kafka CLI on it:
#   kafka-topics.sh --bootstrap-server $BS --create --topic q55big \
#     --partitions 3 --replication-factor 3
#   i=1; while [ $i -le 6000 ]; do echo "$i|{\"id\":$i}"; i=$((i+1)); done \
#     | kafka-console-producer.sh --bootstrap-server $BS --topic q55big \
#         --request-required-acks all \
#         --property parse.key=true --property key.separator='|'
#
# Usage:
#   tests/test_distributed_sql.sh --api <host:port> --bootstrap <host:port> \
#       [--topic q55big] [--cap 1000] [--kafka-bin /opt/kafka/bin]
#
#   # ...or, where curl and the Kafka CLI are not in the same place (the
#   # apache/kafka image has no curl), take the end offsets from Kafka first and
#   # pass them in. Truth still comes from Kafka, never from /_sql:
#   #   kafka-get-offsets.sh --bootstrap-server B --topic T --time latest
#   tests/test_distributed_sql.sh --api <host:port> --counts 2087,1971,1952
#
# Exit: 0 all checks passed | 1 a check failed | 2 cannot run (never a silent pass)

set -u
API=""; BOOTSTRAP=""; COUNTS_IN=""; TOPIC="q55big"; CAP=1000
KBIN="${KAFKA_BIN:-/opt/kafka/bin}"
while [ $# -gt 0 ]; do
  case "$1" in
    --api) API="$2"; shift 2;;
    --bootstrap) BOOTSTRAP="$2"; shift 2;;
    --counts) COUNTS_IN="$2"; shift 2;;
    --topic) TOPIC="$2"; shift 2;;
    --cap) CAP="$2"; shift 2;;
    --kafka-bin) KBIN="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done
[ -n "$API" ] || { echo "CANNOT RUN: --api <host:port> is required" >&2; exit 2; }
command -v curl >/dev/null 2>&1 || { echo "CANNOT RUN: curl not found" >&2; exit 2; }
if [ -z "$COUNTS_IN" ]; then
  [ -n "$BOOTSTRAP" ] || {
    echo "CANNOT RUN: give --bootstrap <host:port> or --counts <n,n,n>" >&2; exit 2; }
  [ -x "$KBIN/kafka-get-offsets.sh" ] || {
    echo "CANNOT RUN: $KBIN/kafka-get-offsets.sh not found; ground truth must come" >&2
    echo "            from Kafka, not from /_sql (the thing under test). Either point" >&2
    echo "            --kafka-bin at it, or pass --counts from a host that has it." >&2
    exit 2; }
fi

PASS=0; FAIL=0

# Read the "rows" of a /_sql response as values in COLUMN order, rows in
# response order. Both matter: the server serialises each row from a map, so
# key order within a row object is arbitrary and cannot be relied on, while row
# order is the answer itself — an ORDER BY that silently did not sort must not
# be able to pass. Numbers are normalised to %.10g on both sides so 1043.0,
# 1043 and 323736.66666666674 compare as written.
EXTRACT='
function norm(v) {
  if (v ~ /^-?[0-9]+\.?[0-9]*([eE][-+]?[0-9]+)?$/) return sprintf("%.10g", v+0)
  gsub(/^"/, "", v); gsub(/"$/, "", v); return v
}
{
  j = $0
  if (index(j, "\"error\"")) { print "ERROR"; exit }
  ci = index(j, "\"columns\":[")
  if (ci == 0) { print "NO_COLUMNS"; exit }
  blob = substr(j, ci + 11)
  blob = substr(blob, 1, index(blob, "]") - 1)
  nc = 0
  while (length(blob) > 0) {
    if (substr(blob, 1, 1) == "\"") {
      blob = substr(blob, 2)
      e = index(blob, "\"")
      cols[++nc] = substr(blob, 1, e - 1)
      blob = substr(blob, e + 1)
    } else blob = substr(blob, 2)
  }
  ri = index(j, "\"rows\":[")
  if (ri == 0) { print "NO_ROWS"; exit }
  rows = substr(j, ri + 8)
  re = index(rows, "],\"row_count\"")
  if (re == 0) { print "NO_ROW_COUNT"; exit }
  rows = substr(rows, 1, re - 1)
  out = ""; n = split(rows, row, /\},\{/)
  for (r = 1; r <= n; r++) {
    for (c = 1; c <= nc; c++) {
      needle = "\"" cols[c] "\":"
      p = index(row[r], needle)
      if (p == 0) { out = out (length(out) ? "," : "") "<missing:" cols[c] ">"; continue }
      v = substr(row[r], p + length(needle))
      if (substr(v, 1, 1) == "\"") { v = substr(v, 2); v = substr(v, 1, index(v, "\"") - 1) }
      else { sub(/[,}].*$/, "", v) }
      out = out (length(out) ? "," : "") norm(v)
    }
  }
  print out
}'

# $1 label  $2 expected  $3 sql  $4 extra JSON (e.g. ', "limit": 5000')
check() {
  _l="$1"; _e="$2"; _q="$3"; _x="${4:-}"
  _b=$(printf '{"query": %s%s}' \
        "$(printf '%s' "$_q" | sed 's/\\/\\\\/g; s/"/\\"/g; s/^/"/; s/$/"/')" "$_x")
  _r=$(curl -s -m 90 -X POST "http://$API/_sql" -H 'Content-Type: application/json' -d "$_b")
  _g=$(printf '%s\n' "$_r" | awk "$EXTRACT")
  if [ "$_g" = "$_e" ]; then
    PASS=$((PASS+1)); printf 'ok   %s\n' "$_l"
  else
    FAIL=$((FAIL+1)); printf 'FAIL %s\n       expected: %s\n       got:      %s\n       raw:      %s\n' \
      "$_l" "$_e" "$_g" "$(printf '%s' "$_r" | head -c 300)"
  fi
}
# Refusals must be refusals: an HTTP 400 naming the limit, never a short 200.
check_refused() {
  _l="$1"; _q="$2"
  _b=$(printf '{"query": %s}' "$(printf '%s' "$_q" | sed 's/\\/\\\\/g; s/"/\\"/g; s/^/"/; s/$/"/')")
  _r=$(curl -s -m 90 -w '|%{http_code}' -X POST "http://$API/_sql" \
        -H 'Content-Type: application/json' -d "$_b")
  case "$_r" in
    *'|400'*DistributedQueryUnsupported*|*DistributedQueryUnsupported*'|400')
      PASS=$((PASS+1)); printf 'ok   %s (refused, as it must be)\n' "$_l";;
    *) FAIL=$((FAIL+1))
       printf 'FAIL %s\n       expected: HTTP 400 DistributedQueryUnsupported\n       got:      %s\n' \
         "$_l" "$(printf '%s' "$_r" | head -c 300)";;
  esac
}

echo "== ground truth from Kafka end offsets =="
if [ -n "$COUNTS_IN" ]; then
  OFF=$(printf '%s' "$COUNTS_IN" | tr ',' '\n' | awk 'NF{printf "%d:%d\n", NR-1, $1}')
else
  OFF=$("$KBIN/kafka-get-offsets.sh" --bootstrap-server "$BOOTSTRAP" --topic "$TOPIC" --time latest 2>/dev/null \
        | sed "s/^$TOPIC://" | sort -t: -k1,1n)
fi
[ -n "$OFF" ] || { echo "CANNOT RUN: no end offsets for topic $TOPIC" >&2; exit 2; }
echo "$OFF" | sed 's/^/  partition:rows /'
COUNTS=$(echo "$OFF" | cut -d: -f2 | paste -sd, -)
MAXC=$(echo "$OFF" | cut -d: -f2 | sort -n | tail -1)
if [ "$MAXC" -le "$CAP" ]; then
  echo "CANNOT RUN: largest partition has $MAXC rows, at or below the ${CAP}-row cap." >&2
  echo "            Below the cap this suite proves nothing. Load more rows." >&2
  exit 2
fi
echo "  largest partition $MAXC rows > cap $CAP -- in the right regime"

# Everything below is computed from $COUNTS alone.
TOTAL=$(echo "$COUNTS" | awk -F, '{s=0; for(i=1;i<=NF;i++) s+=$i; printf "%.10g", s}')
SUMOFF=$(echo "$COUNTS" | awk -F, '{s=0; for(i=1;i<=NF;i++) s+=$i*($i-1)/2; printf "%.10g", s}')
AVGOFF=$(echo "$SUMOFF $TOTAL" | awk '{printf "%.10g", $1/$2}')
MAXOFF=$((MAXC-1))
PCOUNTS=$(echo "$OFF" | awk -F: '{printf "%s%s,%s", (NR>1?",":""), $1, $2}')
PCOUNTS_ASC=$(echo "$OFF" | sort -t: -k2,2n | awk -F: '{printf "%s%s,%s", (NR>1?",":""), $1, $2}')
PCOUNTS_DESC=$(echo "$OFF" | sort -t: -k2,2nr | awk -F: '{printf "%s%s,%s", (NR>1?",":""), $1, $2}')
NPART=$(echo "$OFF" | wc -l | tr -d ' ')
LASTP=$((NPART-1))
# Top 10 offsets descending across the whole multiset.
TOP10=$(echo "$COUNTS" | awk -F, -v n=10 '{
  for (v=0; v<100000 && c<n; v++) {}
  # walk down from the global max, emitting each offset once per partition holding it
  mx=0; for(i=1;i<=NF;i++) if($i>mx) mx=$i;
  out=""; k=0;
  for (o=mx-1; o>=0 && k<n; o--)
    for (i=1;i<=NF && k<n;i++) if (o < $i) { out=out (k?",":"") o; k++ }
  print out }')
# Rank-based deep page: 5 offsets starting at rank cap-2 (so offset+limit > cap).
DEEP_OFFSET=$((CAP-2))
DEEPPAGE=$(echo "$COUNTS" | awk -F, -v skip="$DEEP_OFFSET" -v n=5 '{
  mx=0; for(i=1;i<=NF;i++) if($i>mx) mx=$i;
  out=""; k=0; r=0;
  for (o=mx-1; o>=0 && k<n; o--)
    for (i=1;i<=NF && k<n;i++) if (o < $i) { r++; if (r>skip) { out=out (k?",":"") o; k++ } }
  print out }')
# Per-partition AVG and VAR_POP of 0..e-1  ->  (e-1)/2  and  (e^2-1)/12
GAVG=$(echo "$OFF" | awk -F: '{printf "%s%s,%.10g", (NR>1?",":""), $1, ($2-1)/2}')
GVAR=$(echo "$OFF" | awk -F: '{printf "%s%s,%.10g", (NR>1?",":""), $1, ($2*$2-1)/12}')

echo
echo "== row-wise merges (the paths the cap makes dangerous) =="
check "top-10 by _offset DESC"       "$TOP10"     "SELECT _offset AS o FROM $TOPIC ORDER BY _offset DESC LIMIT 10"
check "deep page past the cap"       "$DEEPPAGE"  "SELECT _offset AS o FROM $TOPIC ORDER BY _offset DESC LIMIT 5 OFFSET $DEEP_OFFSET"

echo
echo "== scalar aggregates =="
check "COUNT(*)"                     "$TOTAL"     "SELECT COUNT(*) AS c FROM $TOPIC"
check "SUM(_offset)"                 "$SUMOFF"    "SELECT SUM(_offset) AS s FROM $TOPIC"
check "MIN/MAX(_offset)"             "0,$MAXOFF"  "SELECT MIN(_offset) AS mn, MAX(_offset) AS mx FROM $TOPIC"
check "AVG unaliased"                "$AVGOFF"    "SELECT AVG(_offset) FROM $TOPIC"
# Text MIN/MAX where the nodes DISAGREE. Every node holds every replica, so
# MIN/MAX over shared values agrees by luck; only a per-node-distinct value
# can catch a merge that keeps whichever node answered first.
check "MIN/MAX over text, nodes disagree" "0,$LASTP" \
  "SELECT MIN(CAST(_partition AS VARCHAR)) AS mn, MAX(CAST(_partition AS VARCHAR)) AS mx FROM $TOPIC"

echo
echo "== grouped merges =="
check "GROUP BY, ORDER BY COUNT(*) DESC" "$PCOUNTS_DESC" \
  "SELECT _partition, COUNT(*) FROM $TOPIC GROUP BY _partition ORDER BY COUNT(*) DESC"
# ASC too: with counts that descend naturally, only ASC proves the sort ran.
check "GROUP BY, ORDER BY COUNT(*) ASC"  "$PCOUNTS_ASC" \
  "SELECT _partition, COUNT(*) FROM $TOPIC GROUP BY _partition ORDER BY COUNT(*) ASC"
check "grouped AVG, unaliased"           "$GAVG" \
  "SELECT _partition, AVG(_offset) FROM $TOPIC GROUP BY _partition ORDER BY _partition"
check "grouped VAR_POP, unaliased"       "$GVAR" \
  "SELECT _partition, VAR_POP(_offset) FROM $TOPIC GROUP BY _partition ORDER BY _partition"
check "HAVING on unaliased COUNT(*)"     "$PCOUNTS" \
  "SELECT _partition, COUNT(*) FROM $TOPIC GROUP BY _partition HAVING COUNT(*) > 0 ORDER BY _partition"

echo
echo "== refusals: a merge that cannot be exact must say so, not come back short =="
check_refused "GROUP BY with more groups than the cap" \
  "SELECT _offset, COUNT(*) FROM $TOPIC GROUP BY _offset"
check_refused "COUNT(DISTINCT) with more values than the cap" \
  "SELECT COUNT(DISTINCT _offset) AS d FROM $TOPIC"

echo
echo "== control: the same shapes BELOW the cap, where the old code also passed =="
CTL=$((CAP/4))
CTLTOTAL=$(echo "$COUNTS" | awk -F, -v c="$CTL" '{s=0; for(i=1;i<=NF;i++) s+=($i<c?$i:c); printf "%.10g", s}')
CTLTOP=$(echo "$COUNTS" | awk -F, -v c="$CTL" -v n=10 '{
  out=""; k=0;
  for (o=c-1; o>=0 && k<n; o--)
    for (i=1;i<=NF && k<n;i++) if (o < $i) { out=out (k?",":"") o; k++ }
  print out }')
check "COUNT(*) below the cap"       "$CTLTOTAL" "SELECT COUNT(*) AS c FROM $TOPIC WHERE _offset < $CTL"
check "top-10 below the cap"         "$CTLTOP"   "SELECT _offset AS o FROM $TOPIC WHERE _offset < $CTL ORDER BY _offset DESC LIMIT 10"

echo
echo "================================================"
echo "passed: $PASS   failed: $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
echo "all distributed /_sql checks passed above the ${CAP}-row per-node cap"
