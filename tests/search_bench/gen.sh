#!/bin/bash
# Deterministic synthetic log corpus for the search benchmark: N documents,
# one JSON object per line, in files of BATCH documents (produced to Kafka by
# run.sh; plain_tantivy_bench reads the same files).
#
# Usage: gen.sh <out-dir> [N=1000000] [BATCH=5000]
# Document: {"ts": epoch millis, "level", "service", "host", "status",
#            "latency_ms", "message": 8-15 words}. Some words are planted at
# known rates so query results are non-trivial: "timeout" ~2%, "connection"
# ~5%, "refused" ~1%.
set -eu
OUT=${1:?out dir}; N=${2:-1000000}; BATCH=${3:-5000}
mkdir -p "$OUT"
awk -v n="$N" -v batch="$BATCH" -v out="$OUT" 'BEGIN {
  srand(42)
  split("ka lo mi ra te su vo ni pe da zu fi go ha be ru so ti na we", syl, " ")
  for (i = 1; i <= 2000; i++) {
    w = ""
    k = 2 + int(rand() * 3)
    for (j = 0; j < k; j++) w = w syl[1 + int(rand() * 20)]
    vocab[i] = w
  }
  split("INFO INFO INFO INFO INFO INFO INFO WARN WARN ERROR", levels, " ")
  split("200 200 200 200 200 201 204 301 400 404 500 503", statuses, " ")
  ts0 = 1767225600000   # 2026-01-01T00:00:00Z
  file = -1
  for (d = 0; d < n; d++) {
    if (d % batch == 0) { if (file >= 0) close(path); file++; path = sprintf("%s/bulk-%05d.ndjson", out, file) }
    msg = ""
    words = 8 + int(rand() * 8)
    for (j = 0; j < words; j++) msg = msg (j ? " " : "") vocab[1 + int(rand() * 2000)]
    r = rand()
    if (r < 0.02) msg = msg " timeout"
    else if (r < 0.07) msg = msg " connection"
    if (rand() < 0.01) msg = msg " refused"
    printf "{\"ts\":%d,\"level\":\"%s\",\"service\":\"svc-%d\",\"host\":\"host-%02d\",\"status\":%s,\"latency_ms\":%d,\"message\":\"%s\"}\n", \
      ts0 + d * 50, levels[1 + int(rand() * 10)], int(rand() * 20), int(rand() * 50), statuses[1 + int(rand() * 12)], int(rand() * 2000), msg > path
  }
  close(path)
}'
echo "$(ls "$OUT" | wc -l) files, $(du -sh "$OUT" | cut -f1), $N documents in $OUT"
