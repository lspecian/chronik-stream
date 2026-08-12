#!/usr/bin/env bash
# #36: does the broker append more records than were produced?
#
# The issue reports 300 produced coming back as 585, with partition end offsets
# confirming the extra records were genuinely appended — write-side duplication,
# not a consumer artefact — and calls it "not reliably reproducible".
#
# This measures it directly and unambiguously:
#   - produce exactly N records to a single partition
#   - ask the BROKER for the partition's end offset (not a consumer, not grep)
#   - end offset == N, or the broker stored something nobody sent
#
# A consumer count can be wrong for its own reasons (a rebalance re-reading from
# the start will double a count without a single duplicate on disk), and grep
# over a WAL file counts storage artefacts rather than records. The end offset is
# the broker's own statement of how many records exist.
#
# Runs each acks level several times, because the issue reports it as
# intermittent — one clean run proves nothing.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/chronik-server"
DIR="$ROOT/tests/cluster"
LOGS="$DIR/logs"
N="${DUP_N:-100}"
ROUNDS="${DUP_ROUNDS:-3}"
FAIL=0

say()  { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; FAIL=1; }

[ -x "$BIN" ] || { say "SKIP: build first (cargo build --release --bin chronik-server)"; exit 0; }
command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }

start_node() {
  mkdir -p "$LOGS" "$DIR/data/alt-node$1"
  CHRONIK_REPLICATION_MODE=pull CHRONIK_UNIFIED_API_PORT=$((6391 + $1)) RUST_LOG=info \
    "$BIN" start --config "$DIR/altport-node$1.toml" > "$LOGS/alt-node$1.log" 2>&1 &
  echo $! > "$LOGS/alt-node$1.pid"
}
stop_all() {
  for n in 1 2 3; do
    p=$(cat "$LOGS/alt-node$n.pid" 2>/dev/null)
    [ -n "$p" ] && { kill -CONT "$p" 2>/dev/null; kill -9 "$p" 2>/dev/null; }
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

BOOT="localhost:9392,localhost:9393,localhost:9394"
KC=(-X socket.timeout.ms=5000 -X metadata.request.timeout.ms=5000)

# The broker's own end offset for partition 0 — how many records it says exist.
end_offset() { # $1 = topic
  timeout 30 kcat -Q -b "$BOOT" -t "$1:0:-1" "${KC[@]}" 2>/dev/null \
    | grep -oE 'offset [0-9]+' | grep -oE '[0-9]+' | head -1
}

say "== #36 duplication probe: $N records per round, $ROUNDS rounds per acks level =="
rm -rf "$DIR/data/alt-node"{1,2,3}
for n in 1 2 3; do start_node "$n"; done
for p in 9392 9393 9394; do
  wait_for_port "$p" || { fail "node on $p never came up"; exit 1; }
done
sleep 12
say "-- three nodes up"
say ""

for acks in -1 1; do
  label="all"; [ "$acks" = "1" ] && label="1"
  for round in $(seq 1 "$ROUNDS"); do
    T="dup-a${label}-r${round}-$$"
    # One partition: every record lands in the same log, so the end offset is
    # directly comparable to what was sent.
    timeout 30 kcat -P -b "$BOOT" -t "$T" -p 0 -X request.required.acks="$acks" "${KC[@]}" \
      < <(seq 1 "$N" | sed 's/^/rec-/') 2>/dev/null
    sleep 4

    got=$(end_offset "$T")
    got="${got:-unknown}"
    if [ "$got" = "$N" ]; then
      say "   acks=$label round $round: end offset $got / produced $N  ✓"
    else
      say "   acks=$label round $round: end offset $got / produced $N  ← MISMATCH"
      fail "acks=$label round $round: broker holds $got records for $N produced"
    fi
  done
done

say ""
if [ "$FAIL" -eq 0 ]; then
  say "== no duplication observed in this run =="
else
  say "== DUPLICATION REPRODUCED — the broker stored records nobody sent =="
fi
exit "$FAIL"
