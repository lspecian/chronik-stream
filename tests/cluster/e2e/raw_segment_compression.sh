#!/bin/bash
# Raw archive segments are written compressed, and segments written before
# compression still read.
#
# The WAL indexer keeps every sealed WAL segment's batches as a raw segment
# (`segments/<topic>/<partition>/<min>-<max>.segment`) that consumers read once
# the WAL copy is reclaimed. Up to v2.19.2 they were plain bincode: most of a
# node's archive footprint (699 MB of 940 MB for 1M small JSON records).
#
# Here, one node: the previous binary archives topic A (plain), then this one,
# on the same data directory, archives topic B (same records). Checks:
#   - B's raw segment is compressed (format marker) and at most a third of A's;
#   - with the WAL reclaimed, a consumer reads every record of both topics,
#     values intact, in order.
#
# Usage: raw_segment_compression.sh <binary> <previous-binary> <label>
# Needs: kcat.
set -u
BIN=${1:?binary under test}; PREV=${2:?previous binary (writes plain segments)}; LABEL=${3:?label}
# A newer "previous" binary writes compressed segments already, and every "before"
# assertion would fail for that reason alone: refuse it rather than report a FAIL.
prev_ver=$("$PREV" version 2>/dev/null | sed -n "s/^Chronik Server v//p" | head -1)
if [ -z "$prev_ver" ] || [ "$(printf "%s\n" "$prev_ver" 2.19.2 | sort -V | tail -1)" != 2.19.2 ]; then
  echo "WRONG BASELINE: the previous binary is v${prev_ver:-unknown}; this check needs one that writes plain raw segments (v2.19.2 or earlier)"
  exit 2
fi
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
D=tests/cluster/data/rawz-$LABEL; LOG=tests/cluster/logs/rawz-$LABEL
B=localhost:9092; N=20000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
seg() { ls "$D"/segments/segments/"$1"/0/*.segment 2>/dev/null | head -1; }
records() { seq 0 $((N - 1)) | awk '{ printf "{\"level\":\"INFO\",\"service\":\"svc-%d\",\"n\":%d,\"message\":\"request handled in %d ms\"}\n", $1 % 13, $1, $1 % 97 }'; }
start() { # binary
  CHRONIK_ADVERTISED_ADDR=localhost "$1" start --data-dir "$D" >> "$LOG-server.log" 2>&1 &
  pid=$!
  for _ in $(seq 60); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "  FAIL: server did not start"; exit 1
}
stop() { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }
archive() { # topic: produce N records, wait until archived and the WAL copy reclaimed
  records | kcat -P -b $B -t "$1" -p 0 -X acks=1 2>/dev/null
  for _ in $(seq 60); do
    # Reclaimed: no WAL file with data left (a fresh, empty active one stays).
    [ -n "$(seg "$1")" ] && [ "$(find "$D/wal/$1/0" -type f -size +0 2>/dev/null | wc -l)" = 0 ] && return 0
    sleep 3
  done
  echo "  FAIL $1: not archived with the WAL reclaimed within 3 minutes"; fail=1
}
consumed() { # topic -> sha of the values read back
  timeout 60 kcat -C -b $B -t "$1" -p 0 -o beginning -c $N -X check.crcs=true -q -f '%s\n' 2>/dev/null | sha256sum | cut -c1-16
}

echo "=== $LABEL ($BIN, previous $PREV)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"; : > "$LOG-server.log"
trap stop EXIT
want=$(records | sha256sum | cut -c1-16)

start "$PREV"
archive rawa
stop
start "$BIN"
archive rawb
a=$(seg rawa); b=$(seg rawb)
asz=$(stat -c %s "$a"); bsz=$(stat -c %s "$b")
echo "raw segments: previous binary $asz bytes, this one $bsz bytes"
expect "previous binary's segment is plain" "$(head -c 8 "$a" | grep -c CHRNKZ1)" "0"
expect "this binary's segment is compressed" "$(head -c 8 "$b" | grep -c CHRNKZ1)" "1"
expect "compressed to at most a third" "$([ $((bsz * 3)) -le "$asz" ] && echo yes || echo "no: $bsz vs $asz")" "yes"
expect "plain segment read back (WAL reclaimed)" "$(consumed rawa)" "$want"
expect "compressed segment read back (WAL reclaimed)" "$(consumed rawb)" "$want"

# And after a restart (nothing cached).
stop; start "$BIN"
expect "both read back after a restart" "$(consumed rawa) $(consumed rawb)" "$want $want"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
