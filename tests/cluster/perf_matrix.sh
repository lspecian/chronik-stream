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

say() { printf '%s\n' "$*"; }

[ -x "$BIN" ] || { say "SKIP: cargo build --release --bin chronik-server"; exit 0; }
[ -x "$BENCH" ] || { say "SKIP: cargo build --release --bin chronik-bench"; exit 0; }

SINGLE_DIR="$DIR/data/perf-single"
single_up() {
  rm -rf "$SINGLE_DIR"; mkdir -p "$SINGLE_DIR" "$LOGS"
  # Default Kafka port: `start` has no port flag and CHRONIK_KAFKA_PORT is
  # deprecated, so the single-node shape uses 9092 while the cluster shape uses
  # the shifted ports. They never run at the same time.
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
stop_all() {
  for f in "$LOGS/perf-single.pid" "$LOGS/alt-node1.pid" "$LOGS/alt-node2.pid" "$LOGS/alt-node3.pid"; do
    p=$(cat "$f" 2>/dev/null); [ -n "$p" ] && kill -9 "$p" 2>/dev/null; rm -f "$f"
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

# chronik-bench prints a summary; pull the two numbers that matter out of it.
run_bench() { # $1=bootstrap $2=acks $3=topic
  "$BENCH" -b "$1" -t "$3" -c "$CONCURRENCY" -s "$SIZE" -d "$DURATION" \
    -p "$PARTITIONS" --acks "$2" -m produce 2>&1
}

report() { # $1=label $2=output
  local msgs mb p99
  msgs=$(printf '%s' "$2" | grep -oiE "throughput[^0-9]*([0-9,]+\.?[0-9]*) *msg" | grep -oE "[0-9,]+\.?[0-9]*" | tail -1)
  mb=$(printf '%s' "$2" | grep -oiE "([0-9,]+\.?[0-9]*) *MB/s" | grep -oE "[0-9,]+\.?[0-9]*" | tail -1)
  p99=$(printf '%s' "$2" | grep -oiE "p99[^0-9]*([0-9]+\.?[0-9]*)" | grep -oE "[0-9]+\.?[0-9]*" | tail -1)
  printf '  %-34s %14s msg/s  %10s MB/s  p99 %sms\n' "$1" "${msgs:-?}" "${mb:-?}" "${p99:-?}"
}

say "== chronik-bench: $CONCURRENCY producers, ${SIZE}B, $DURATION, $PARTITIONS partitions =="
say ""

say "-- single node (no replication)"
single_up
wait_for_port 9092 || { say "single node never came up"; exit 1; }
sleep 6
for acks in 0 1 all; do
  report "acks=$acks" "$(run_bench localhost:9092 "$acks" "bench-single-$acks-$$")"
done
stop_all
sleep 2

say ""
say "-- 3 nodes, RF=3, min_insync=2 (follower-pull replication running)"
cluster_up
for p in 9392 9393 9394; do wait_for_port "$p" || { say "cluster never came up"; exit 1; }; done
sleep 12
for acks in 0 1 all; do
  report "acks=$acks" "$(run_bench localhost:9392,localhost:9393,localhost:9394 "$acks" "bench-cluster-$acks-$$")"
done
