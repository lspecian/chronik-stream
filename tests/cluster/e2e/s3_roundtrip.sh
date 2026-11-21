#!/bin/bash
# Archived records are read back from S3, byte for byte.
#
# For changes to the S3 path (the AWS SDK, request signing, the object store
# layer): one node archives to an S3-compatible store (SeaweedFS in Docker).
# Once a sealed WAL segment is archived the node deletes it, so the records in
# it exist only in the bucket. A restart with the local caches removed must
# then serve every record from offset 0, reading the archived ones from S3.
#
#   1. 20,000 records (unique payloads) over 3 partitions, acks=all, with small
#      WAL segments so most of them seal and archive within the run.
#   2. Wait until each partition has archived to the bucket and the local WAL
#      holds under 10% of what was produced.
#   3. Stop, delete the local segment and search caches, start again.
#   4. Consume everything: same count, same payloads, offsets from 0.
#
# Not covered: losing the whole data directory. Restoring the catalog from the
# bucket is not implemented (the metadata uploader reads paths the Raft
# catalog no longer writes, and the restore path has no caller).
#
# Usage: s3_roundtrip.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, docker (pulls chrislusf/seaweedfs).
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
command -v docker >/dev/null || { echo "SKIP: docker not installed"; exit 77; }
D=tests/cluster/data/s3rt-$LABEL; LOG=tests/cluster/logs/s3rt-$LABEL
B=localhost:9092; T=s3rt; N=20000; PARTS=3
S3=http://127.0.0.1:8333; BUCKET=chronik-s3rt
PREFIX="run-$LABEL-$(date +%s)" # fresh per run
SW=chronik-s3rt-seaweedfs
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
keys() { curl -s "$S3/$BUCKET?list-type=2&max-keys=1000&prefix=$PREFIX/" | grep -o '<Key>[^<]*</Key>' | sed 's/<[^>]*>//g'; }
payloads() { seq 0 $((N - 1)) | awk '{ printf "{\"n\":%d,\"pad\":\"%064d\"}\n", $1, $1 }'; }

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
docker rm -f $SW >/dev/null 2>&1
docker run -d --rm --name $SW -p 8333:8333 chrislusf/seaweedfs server -s3 >/dev/null \
  || { echo "FAIL: could not start SeaweedFS"; exit 1; }
for _ in $(seq 60); do curl -s -o /dev/null -m 2 "$S3" && break; sleep 1; done
curl -s -X PUT "$S3/$BUCKET" >/dev/null

rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
start_node() { # <log suffix>
  CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_WAL_ROTATION_SIZE=100KB \
  OBJECT_STORE_BACKEND=s3 S3_ENDPOINT=$S3 S3_BUCKET=$BUCKET S3_PREFIX=$PREFIX \
  S3_REGION=us-east-1 S3_ACCESS_KEY=any S3_SECRET_KEY=any S3_PATH_STYLE=true S3_DISABLE_SSL=true \
    "$BIN" start --data-dir "$D" > "$LOG-$1.log" 2>&1 &
  pid=$!
  for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "FAIL: node did not come up ($LOG-$1.log)"; return 1
}
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null; docker rm -f $SW >/dev/null 2>&1' EXIT
start_node first || exit 1

$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions $PARTS --replication-factor 1 >/dev/null 2>&1
sleep 2
for p in $(seq 0 $((PARTS - 1))); do
  payloads | awk -v p=$p -v n=$PARTS 'NR % n == (p + 1) % n' | kcat -P -b $B -t $T -p $p -X acks=all 2>/dev/null
done
expect "end offsets before" "$($K/kafka-get-offsets.sh --bootstrap-server $B --topic $T 2>/dev/null | awk -F: '{s+=$3} END {print s}')" "$N"
produced=$(payloads | wc -c)

for _ in $(seq 60); do
  archived=0
  for p in $(seq 0 $((PARTS - 1))); do keys | grep -q "/segments/$T/$p/" && archived=$((archived + 1)); done
  wal=$(du -sb "$D/wal" | cut -f1)
  [ $archived = $PARTS ] && [ $((wal * 10)) -lt $produced ] && break
  sleep 5
done
echo "  objects under $PREFIX/: $(keys | wc -l); local WAL $wal bytes of $produced produced"
expect "partitions archived to S3" "$archived" "$PARTS"
expect "local WAL under 10% of what was produced" "$([ $((wal * 10)) -lt $produced ] && echo yes || echo no)" "yes"

kill $pid; wait $pid 2>/dev/null
rm -rf "$D/segments" "$D/search-cache"
start_node restarted || exit 1
sleep 5

expect "end offsets after restart" "$($K/kafka-get-offsets.sh --bootstrap-server $B --topic $T 2>/dev/null | awk -F: '{s+=$3} END {print s}')" "$N"
got=$(kcat -C -b $B -t $T -o beginning -e -q -f '%s\n' 2>/dev/null | sort | md5sum | cut -d' ' -f1)
expect "payloads read back (sorted md5)" "$got" "$(payloads | sort | md5sum | cut -d' ' -f1)"
first=$(kcat -C -b $B -t $T -p 0 -o beginning -c 1 -e -q -f '%o\n' 2>/dev/null)
expect "partition 0 starts at offset" "$first" "0"

# Control: the same read with the bucket unreachable must come up short, or
# the records above were not read from S3 at all.
kill $pid; wait $pid 2>/dev/null
docker stop $SW >/dev/null 2>&1
rm -rf "$D/segments" "$D/search-cache"
start_node no-bucket || exit 1
sleep 5
without=$(timeout 60 kcat -C -b $B -t $T -o beginning -e -q -f '%o\n' -X fetch.wait.max.ms=500 2>/dev/null | wc -l)
echo "  records readable with the bucket stopped: $without of $N"
expect "records come from S3 (fewer without it)" "$([ "$without" -lt $N ] && echo yes || echo no)" "yes"
expect "panics" "$(cat "$LOG"-*.log | grep -c panicked)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
