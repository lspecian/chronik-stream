#!/usr/bin/env bash
# A 10x-low run survives a 104-second settle (7,511 msg/s against ~89,000 on the
# other five), so it is not the cluster failing to form in time — that hypothesis
# is dead. Something goes wrong DURING the run, only when the producer waits for
# replication.
#
# So stop inferring from throughput and keep the evidence: run at INFO, preserve
# all three brokers' logs per run, tag each set ok/LOW, and let a later diff say
# what the low runs do that the good ones do not.
#
# Ordered by what would explain a 10x collapse in acks=1 but not acks=0:
#   - ISR shrinking mid-run, so acks=1 waits on a follower that fell out
#   - a leader election partway through
#   - follower fetches erroring and backing off
set -u
DIR=/home/ubuntu/Development/chronik-stream/tests/cluster
BIN=/home/ubuntu/Development/chronik-stream/target/release/chronik-server
BENCH=/home/ubuntu/Development/chronik-stream/target/release/chronik-bench
LOGS=$DIR/logs
OUT=/tmp/outlier-evidence
ACKS="${ACKS:-1}"; RUNS="${RUNS:-8}"
rm -rf "$OUT"; mkdir -p "$OUT"

stop_all() {
  for n in 1 2 3; do
    p=$(cat "$LOGS/alt-node$n.pid" 2>/dev/null)
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
    rm -f "$LOGS/alt-node$n.pid"
  done
  for _ in $(seq 1 60); do
    busy=0
    for port in 9392 9393 9394; do
      (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null && { exec 3<&-; busy=1; }
    done
    [ "$busy" -eq 0 ] && break
    sleep 1
  done
  sync
  sleep 20
}

printf '%-5s %10s %8s\n' run msg/s verdict
for run in $(seq 1 "$RUNS"); do
  rm -rf "$DIR/data/alt-node"{1,2,3}
  for n in 1 2 3; do
    mkdir -p "$LOGS" "$DIR/data/alt-node$n"
    CHRONIK_UNIFIED_API_PORT=$((6391 + n)) RUST_LOG=info \
      "$BIN" start --config "$DIR/altport-node$n.toml" > "$LOGS/perfm-node$n.log" 2>&1 &
    echo $! > "$LOGS/alt-node$n.pid"
  done
  for port in 9392 9393 9394; do
    for _ in $(seq 1 60); do
      (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null && { exec 3<&-; break; }
      sleep 1
    done
  done
  sleep 25

  r=$(timeout 180 "$BENCH" -b localhost:9392,localhost:9393,localhost:9394 \
      -t out4-$ACKS-$run-$$ -c 1024 -s 256 -d 30s -p 3 --acks "$ACKS" -m produce 2>&1 \
      | sed -n 's/.*Message rate: *\([0-9,]*\).*/\1/p' | tail -1 | tr -d ',')
  v="ok"; [ "${r:-0}" -lt 30000 ] && v="LOW"

  d="$OUT/run${run}-${v}-${r:-0}"
  mkdir -p "$d"
  for n in 1 2 3; do cp "$LOGS/perfm-node$n.log" "$d/node$n.log" 2>/dev/null; done

  printf '%-5s %10s %8s\n' "$run" "${r:-0}" "$v"
  stop_all
done
echo DONE
