#!/usr/bin/env bash
# T9: produce throughput A/B against a baseline binary (bytes landed),
# alternating run order (a fixed order once produced a fake +11%).
# Usage: BASE_BIN=<released binary> t9_perf_ab.sh   (CHRONIK_BIN = under test)
# PASS: the binary under test is within run-to-run noise of the baseline at
# acks=1 and acks=all (mean of ROUNDS alternating runs, tolerance T9_TOL %).
source "$(dirname "$0")/lib.sh"
BASE="${BASE_BIN:?set BASE_BIN to the baseline binary}"
ROUNDS="${T9_ROUNDS:-5}"; TOL="${T9_TOL:-10}"
export PERF_RECORDS="${PERF_RECORDS:-100000}"
out="$DIR/data/t9"; rm -rf "$out"; mkdir -p "$out"
run() { # $1 label $2 binary $3 round
  CHRONIK_BIN="$2" "$DIR/perf_replication.sh" > "$out/$1-$3.txt" 2>&1
  for a in 1 all; do grep -E "acks=$a " "$out/$1-$3.txt" | awk -v l="$1" -v a="$a" '{print l, a, $2}'; done
}
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) = 1 ]; then run new "$BIN" "$r"; run base "$BASE" "$r"; else run base "$BASE" "$r"; run new "$BIN" "$r"; fi
done > "$out/rates.txt"
cat "$out/rates.txt"
grep -l '^FAIL' "$out"/*-*.txt >/dev/null 2>&1 && fail "a run did not start cleanly: $(grep -h '^FAIL' "$out"/*-*.txt | head -3 | tr '\n' ' ')"
[ "$(wc -l < "$out/rates.txt")" -eq $((ROUNDS * 4)) ] || fail "expected $((ROUNDS * 4)) rates, got $(wc -l < "$out/rates.txt")"
for a in 1 all; do
  n=$(awk -v a="$a" '$1=="new" && $2==a {s+=$3; c++} END{print (c? s/c : 0)}' "$out/rates.txt")
  b=$(awk -v a="$a" '$1=="base" && $2==a {s+=$3; c++} END{print (c? s/c : 0)}' "$out/rates.txt")
  d=$(awk -v n="$n" -v b="$b" 'BEGIN{print (b>0)? (n-b)*100/b : 0}')
  say "acks=$a: new $(printf %.0f "$n") msg/s, base $(printf %.0f "$b") msg/s, difference ${d}%"
  awk -v d="$d" -v t="$TOL" 'BEGIN{exit !(d >= -t)}' || fail "acks=$a throughput ${d}% below baseline (tolerance -${TOL}%)"
done
grep -l 'client errors [1-9]' "$out"/*.txt >/dev/null 2>&1 && fail "client errors during a run: $(grep -l 'client errors [1-9]' "$out"/*.txt | tr '\n' ' ')"
finish "T9 throughput within noise of baseline"
