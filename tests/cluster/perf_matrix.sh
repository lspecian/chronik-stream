#!/usr/bin/env bash
# The performance numbers this repo publishes, measured with one tool on one
# machine so they can be compared with each other.
#
# Every figure published before 2026-08-13 was taken while replication was
# silently disabled (PR #29): `acks=1` and `acks=all` replicated nothing, so a
# "3-node cluster" number described one node writing to its own disk. Those
# numbers were deleted rather than annotated — a report of invalid measurements
# with a warning on top is worse than no report, because the warning is read
# once and the tables are cited forever.
#
# Two shapes, both with `chronik-bench`:
#   SINGLE NODE — no replication to do. The ceiling of the write path.
#   3-NODE RF=3 — follower-pull replication running. What replication costs.
#
# Scope, stated plainly: one machine. The 3-node shape runs three brokers on it,
# sharing disk, cores and loopback, which understates network cost and
# overstates disk contention against three separate machines. It is a lower
# bound, and it is comparable across acks levels and against the single-node
# shape because every run uses the same tool on the same hardware.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/chronik-server"
BENCH="$ROOT/target/release/chronik-bench"
DIR="$ROOT/tests/cluster"
LOGS="$DIR/logs"
DURATION="${PERF_DURATION:-30s}"
CONCURRENCY="${PERF_CONCURRENCY:-64}"
SIZE="${PERF_SIZE:-256}"
PARTITIONS="${PERF_PARTITIONS:-3}"
RUNS="${PERF_RUNS:-3}"

say() { printf '%s\n' "$*"; }

[ -x "$BIN" ] || { say "SKIP: cargo build --release --bin chronik-server"; exit 0; }
[ -x "$BENCH" ] || { say "SKIP: cargo build --release --bin chronik-bench"; exit 0; }

SINGLE_DIR="$DIR/data/perf-single"
single_up() {
  rm -rf "$SINGLE_DIR"; mkdir -p "$SINGLE_DIR" "$LOGS"
  # Single-node uses 9092 while the cluster shape uses the shifted ports; they
  # never run at the same time. `--kafka-port` exists since 2026-08-16 if these
  # ever need to overlap.
  CHRONIK_UNIFIED_API_PORT=6492 CHRONIK_ADVERTISED_ADDR=localhost \
    RUST_LOG=warn "$BIN" start --data-dir "$SINGLE_DIR" > "$LOGS/perf-single.log" 2>&1 &
  echo $! > "$LOGS/perf-single.pid"
}
cluster_up() {
  rm -rf "$DIR/data/alt-node"{1,2,3}
  for n in 1 2 3; do
    mkdir -p "$LOGS" "$DIR/data/alt-node$n"
    CHRONIK_UNIFIED_API_PORT=$((6391 + n)) RUST_LOG=warn \
      "$BIN" start --config "$DIR/altport-node$n.toml" > "$LOGS/perfm-node$n.log" 2>&1 &
    echo $! > "$LOGS/alt-node$n.pid"
  done
}
# Kill the brokers and do not return until their ports are actually free.
#
# `kill -9` + `sleep 2` was not enough. A broker that has not finished dying
# still holds its listener, so the next shape's node failed to bind while
# `wait_for_port` happily connected to the corpse — and the benchmark that
# followed measured a cluster that was half up. That is how this harness
# reported 2,912 msg/s for a configuration three clean runs put at 5,475-6,409.
stop_all() {
  for f in "$LOGS/perf-single.pid" "$LOGS/alt-node1.pid" "$LOGS/alt-node2.pid" "$LOGS/alt-node3.pid"; do
    p=$(cat "$f" 2>/dev/null); [ -n "$p" ] && kill -9 "$p" 2>/dev/null; rm -f "$f"
  done
  for _ in $(seq 1 60); do
    busy=0
    for port in 9092 9392 9393 9394; do
      (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null && { exec 3<&-; busy=1; }
    done
    [ "$busy" -eq 0 ] && break
    sleep 1
  done

  # And let the disk catch up before the next measurement starts.
  #
  # An `acks=0` run writes several hundred MB in 30 seconds. Starting the next
  # shape while the kernel is still flushing that measures the residue of the
  # previous benchmark: the cluster rows came out at 8,044 and 2,970 msg/s in
  # sequence against 14,502 and 5,475-6,409 for the same builds measured on a
  # quiet machine. Each row is supposed to describe its own configuration.
  sync
  sleep "${PERF_SETTLE:-20}"
}
trap stop_all EXIT
wait_for_port() {
  for _ in $(seq 1 60); do
    (exec 3<>/dev/tcp/127.0.0.1/$1) 2>/dev/null && { exec 3<&- ; return 0; }
    sleep 1
  done
  return 1
}

# chronik-bench prints a summary; pull the two numbers that matter out of it.
run_bench() { # $1=bootstrap $2=acks $3=topic
  "$BENCH" -b "$1" -t "$3" -c "$CONCURRENCY" -s "$SIZE" -d "$DURATION" \
    -p "$PARTITIONS" --acks "$2" -m produce 2>&1
}

# Pull the published numbers out of chronik-bench's summary box.
#
# Anchored on the exact labels the reporter prints, because loose patterns
# quietly matched the wrong things: "throughput" appears only as a section
# HEADING (so the message rate came out empty and printed as "?"), a bare
# `MB/s` search also caught `Data transferred: N MB`, and `p99` matched the
# `p99.9` line one row below — which is why every row reported an identical
# "p99 99.9ms". A number that is wrong in the same way every time reads as a
# real measurement, so this parses the labels exactly and says so when it
# cannot.
extract() { # $1=output $2=field
  case "$2" in
    msgs) printf '%s' "$1" | sed -n 's/.*Message rate: *\([0-9,]*\) msg\/s.*/\1/p' | tail -1 | tr -d ',' ;;
    mb)   printf '%s' "$1" | sed -n 's/.*Bandwidth: *\([0-9.,]*\) MB\/s.*/\1/p' | tail -1 | tr -d ',' ;;
    p99)  printf '%s' "$1" | sed -n 's/.*p99: .*(\ *\([0-9.]*\) ms).*/\1/p' | tail -1 ;;
  esac
}

median() { printf '%s\n' "$@" | sort -n | awk '{v[NR]=$0} END {print v[int((NR+1)/2)]}'; }

# Every figure is the median of PERF_RUNS measurements, each on its own cluster.
#
# A single measurement is not reproducible here. `acks=all` on the cluster came
# out at 2,912, 2,970, 6,587 and 3,066 msg/s across four runs of this script,
# while the same configuration measured on its own gave 5,475-6,409 four times
# running. The spread is the machine, not the build: three brokers, a load
# generator and the page cache all share one box, and whatever ran before leaves
# the disk busy. Reporting whichever number came up first would make every
# comparison in this file a coin toss.
run_row() { # $1=label $2=up-fn $3=ports $4=settle-after-up $5=bootstrap $6=acks $7=topic-prefix
  local msgs=() mbs=() p99s=() out
  for run in $(seq 1 "$RUNS"); do
    "$2"
    for port in $3; do
      wait_for_port "$port" || { say "  $1: broker on $port never came up"; stop_all; return 1; }
    done
    sleep "$4"
    out=$(run_bench "$5" "$6" "$7-$run-$$")
    msgs+=("$(extract "$out" msgs)")
    mbs+=("$(extract "$out" mb)")
    p99s+=("$(extract "$out" p99)")
    stop_all
  done
  printf '  %-30s %12s msg/s  %9s MB/s  p99 %sms   (median of %s: %s)\n' \
    "$1" "$(median "${msgs[@]}")" "$(median "${mbs[@]}")" "$(median "${p99s[@]}")" \
    "$RUNS" "$(printf '%s ' "${msgs[@]}")"
}

say "== chronik-bench: $CONCURRENCY producers, ${SIZE}B, $DURATION, $PARTITIONS partitions =="
say ""

# Each measurement gets a FRESH cluster.
#
# Running all three acks levels against one cluster made the last one look worst
# simply for being last: `acks=all` followed a minute of `acks=0` and `acks=1`
# traffic and measured 2,407 msg/s where the same build on a fresh cluster
# measures 6,000+. The rows exist to be compared with each other, so the only
# thing that may differ between them is the acks level.
say "-- single node (no replication)"
for acks in 0 1 all; do
  run_row "acks=$acks" single_up "9092" 6 "localhost:9092" "$acks" "bench-single-$acks"
done

say ""
say "-- 3 nodes, RF=3, min_insync=2 (follower-pull replication running)"
for acks in 0 1 all; do
  run_row "acks=$acks" cluster_up "9392 9393 9394" 12 \
    "localhost:9392,localhost:9393,localhost:9394" "$acks" "bench-cluster-$acks"
done
