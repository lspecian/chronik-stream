#!/usr/bin/env bash
# Throughput with follower-pull replication actually running.
#
# Every published performance number in this repo was measured WITHOUT working
# replication: `acks=1` and `acks=all` did not replicate at all until #29, so
# figures taken on those paths describe a single node writing to its own disk
# while calling itself a cluster. This measures the same three acks levels on a
# real 3-node cluster with replication on, so the cost of replication is visible
# instead of assumed.
#
# Scope, stated plainly: three processes on ONE machine, sharing its disk, CPU
# and loopback. That understates network cost and overstates disk contention
# relative to three separate machines. It is a lower bound on what the code can
# do, measured honestly, and it is comparable ACROSS acks levels because every
# run is identical apart from the acks setting.
#
# The WAL profile is deliberately left unset so every level gets the same one.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/chronik-server"
DIR="$ROOT/tests/cluster"
LOGS="$DIR/logs"
BOOT="localhost:9392,localhost:9393,localhost:9394"
RECORDS="${PERF_RECORDS:-200000}"
PAYLOAD="${PERF_PAYLOAD:-100}"

say() { printf '%s\n' "$*"; }

[ -x "$BIN" ] || { say "SKIP: build first (cargo build --release --bin chronik-server)"; exit 0; }
command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }

start_node() {
  mkdir -p "$LOGS" "$DIR/data/alt-node$1"
  CHRONIK_REPLICATION_MODE=pull CHRONIK_UNIFIED_API_PORT=$((6391 + $1)) RUST_LOG=warn \
    "$BIN" start --config "$DIR/altport-node$1.toml" > "$LOGS/perf-node$1.log" 2>&1 &
  echo $! > "$LOGS/alt-node$1.pid"
}
stop_all() {
  for n in 1 2 3; do
    p=$(cat "$LOGS/alt-node$n.pid" 2>/dev/null)
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
    rm -f "$LOGS/alt-node$n.pid"
  done
}
trap stop_all EXIT
wait_for_port() {
  for _ in $(seq 1 60); do
    (exec 3<>/dev/tcp/127.0.0.1/$1) 2>/dev/null && { exec 3<&- ; return 0; }
    sleep 1
  done
  return 1
}

# The broker's own end offset — how many records it actually stored.
end_offset() { # $1 = topic
  local total=0
  for p in 0 1 2; do
    local o
    o=$(timeout 30 kcat -Q -b "$BOOT" -t "$1:$p:-1" 2>/dev/null \
      | grep -oE 'offset [0-9]+' | grep -oE '[0-9]+' | head -1)
    total=$((total + ${o:-0}))
  done
  echo "$total"
}

payload=$(head -c "$PAYLOAD" < /dev/zero | tr '\0' 'x')

median() { printf '%s\n' "$@" | sort -n | awk '{v[NR]=$1} END {print (NR%2) ? v[(NR+1)/2] : int((v[NR/2]+v[NR/2+1])/2)}'; }

measure() { # $1 = acks value, $2 = label
  local rates=() stored_last=0 errs_total=0 first_err=""
  # Three rounds: a single timing on a shared machine is noise. Observed spread
  # across rounds is roughly ±30%, so a lone figure would be a coin toss
  # presented as a measurement.
  for round in 1 2 3; do
    local topic="perf-$2-r$round-$$" t0 t1 ms
    # Create and settle first, so topic creation is not inside the measurement.
    echo warmup | kcat -P -b "$BOOT" -t "$topic" -X request.required.acks="$1" 2>/dev/null
    sleep 8

    # Client errors are part of the result, not noise to be discarded. A run
    # that produces 200,000 records and stores fewer is either a broker losing
    # acknowledged writes or a client dropping them, and discarding stderr makes
    # those two indistinguishable — the confusion that makes a benchmark
    # worthless.
    local errfile="$LOGS/perf-$2-r$round.err"
    t0=$(date +%s%3N)
    seq 1 "$RECORDS" | sed "s/\$/ $payload/" \
      | timeout 900 kcat -P -b "$BOOT" -t "$topic" -X request.required.acks="$1" 2>"$errfile"
    t1=$(date +%s%3N)
    ms=$((t1 - t0))
    [ "$ms" -le 0 ] && ms=1
    rates+=("$(( RECORDS * 1000 / ms ))")

    sleep 6
    stored_last=$(end_offset "$topic")
    local errs
    errs=$(grep -c . "$errfile" 2>/dev/null); errs=${errs:-0}
    errs_total=$((errs_total + errs))
    [ "$errs" -gt 0 ] && [ -z "$first_err" ] && first_err=$(head -1 "$errfile" | cut -c1-120)
  done

  printf '  acks=%-4s %8s msg/s (median of 3)   broker stored %-7s client errors %s\n' \
    "$2" "$(median "${rates[@]}")" "$stored_last" "$errs_total"
  [ -n "$first_err" ] && printf '           first client error: %s\n' "$first_err"
  return 0
}

say "== throughput with follower-pull replication (3 nodes, RF=3, min_insync=2) =="
say "   $RECORDS records x ${PAYLOAD}B, one machine, WAL profile left at its default"
say ""
rm -rf "$DIR/data/alt-node"{1,2,3}
for n in 1 2 3; do start_node "$n"; done
for p in 9392 9393 9394; do wait_for_port "$p" || { say "node on $p never came up"; exit 1; }; done
sleep 12

measure 0 0
measure 1 1
measure -1 all

say ""
say "   acks=0 is fire-and-forget: the shortfall in 'stored' is the guarantee it does not make."
