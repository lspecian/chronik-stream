#!/bin/bash
# A single node whose whole data directory is lost comes back from the object
# store: topics, configs, consumer offsets, and every record.
#
#   1. One node archives to SeaweedFS (S3), uploading disaster-recovery images
#      every 2 s. Two topics (one with a config), 20,000 records, a consumer
#      group committing offset 5000 on partition 0.
#   2. Wait until the records are archived and the images are uploaded.
#   3. Control: delete the data directory, start with CHRONIK_DR_RESTORE=false:
#      the topics are unknown (so what follows is the restore, nothing else).
#   4. Delete it again, start normally: the topics, their partition counts and
#      config, the group's committed offsets and every record (byte for byte)
#      are back; new records continue the offsets; a restart keeps it all.
#
# Usage: dr_restore.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, docker (pulls chrislusf/seaweedfs).
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
command -v kcat >/dev/null || { echo "SKIP: kcat not installed"; exit 77; }
command -v docker >/dev/null || { echo "SKIP: docker not installed"; exit 77; }
D=tests/cluster/data/drr-$LABEL; LOG=tests/cluster/logs/drr-$LABEL
B=localhost:9092; N=20000; PARTS=3
S3=http://127.0.0.1:8333; BUCKET=chronik-drr
PREFIX="run-$LABEL-$(date +%s)" # fresh per run
SW=chronik-drr-seaweedfs
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
keys() { curl -s "$S3/$BUCKET?list-type=2&max-keys=1000&prefix=$PREFIX/$1" | grep -o '<Key>[^<]*</Key>' | sed 's/<[^>]*>//g'; }
payloads() { seq 0 $((N - 1)) | awk '{ printf "{\"n\":%d,\"pad\":\"%064d\"}\n", $1, $1 }'; }
ends() { $K/kafka-get-offsets.sh --bootstrap-server $B --topic "$1" 2>/dev/null | awk -F: '{s+=$3} END {print s+0}'; }
# Never auto-creates the topic (a metadata request for it would).
partitions() { $K/kafka-topics.sh --bootstrap-server $B --describe --topic "$1" 2>/dev/null | grep -c 'Partition:'; }
committed() { # group topic: total committed offset over its partitions
  $K/kafka-consumer-groups.sh --bootstrap-server $B --describe --group "$1" 2>/dev/null \
    | awk -v t="$2" '$2 == t && $4 ~ /^[0-9]+$/ { s += $4 } END { print s+0 }'
}
sql_count() { curl -s -m 30 -X POST localhost:6092/_sql -H "Content-Type: application/json" -d "{\"query\":\"SELECT COUNT(*) AS c FROM $1\"}" | grep -o '"c":[0-9]*' | head -1 | cut -d: -f2; }
wipe() { [ -n "$D" ] && rm -rf "$REPO/$D" && mkdir -p "$REPO/$D"; }

echo "=== $LABEL ($BIN)"
tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
docker rm -f $SW >/dev/null 2>&1
docker run -d --rm --name $SW -p 8333:8333 chrislusf/seaweedfs server -s3 >/dev/null \
  || { echo "FAIL: could not start SeaweedFS"; exit 1; }
for _ in $(seq 60); do curl -s -o /dev/null -m 2 "$S3" && break; sleep 1; done
curl -s -X PUT "$S3/$BUCKET" >/dev/null

mkdir -p "$(dirname "$LOG")"; wipe
pid=
start_node() { # <log suffix> [env...]
  env CHRONIK_ADVERTISED_ADDR=localhost CHRONIK_WAL_ROTATION_SIZE=100KB CHRONIK_DR_UPLOAD_INTERVAL_SECS=2 \
    CHRONIK_COLUMNAR_CATCHUP_DELAY_SECS=5 CHRONIK_COLUMNAR_CATCHUP_INTERVAL_SECS=5 \
    OBJECT_STORE_BACKEND=s3 S3_ENDPOINT=$S3 S3_BUCKET=$BUCKET S3_PREFIX=$PREFIX \
    S3_REGION=us-east-1 S3_ACCESS_KEY=any S3_SECRET_KEY=any S3_PATH_STYLE=true S3_DISABLE_SSL=true \
    "${@:2}" "$BIN" start --data-dir "$D" > "$LOG-$1.log" 2>&1 &
  pid=$!
  for _ in $(seq 120); do curl -fsS -m 2 localhost:6092/health >/dev/null 2>&1 && return 0; sleep 1; done
  echo "FAIL: node did not come up ($LOG-$1.log)"; exit 1
}
stop_node() { kill $pid 2>/dev/null; wait $pid 2>/dev/null; pid=; }
trap 'stop_node; docker rm -f $SW >/dev/null 2>&1' EXIT

start_node first
$K/kafka-topics.sh --bootstrap-server $B --create --topic drr --partitions $PARTS --replication-factor 1 >/dev/null 2>&1
$K/kafka-configs.sh --bootstrap-server $B --alter --entity-type topics --entity-name drr --add-config columnar.enabled=true >/dev/null 2>&1
$K/kafka-topics.sh --bootstrap-server $B --create --topic drr-cfg --partitions 2 --replication-factor 1 \
  --config retention.ms=123456789 >/dev/null 2>&1
sleep 2
for p in $(seq 0 $((PARTS - 1))); do
  payloads | awk -v p=$p -v n=$PARTS 'NR % n == (p + 1) % n' | kcat -P -b $B -t drr -p $p -X acks=all 2>/dev/null
done
echo '{"x":1}' | kcat -P -b $B -t drr-cfg -p 1 2>/dev/null
# Group g1 consumes 5000 records and commits its position.
kcat -C -b $B -G g1 drr -X auto.offset.reset=earliest -X enable.auto.commit=true -X auto.commit.interval.ms=200 \
  -c 5000 -e -q >/dev/null 2>&1
sleep 2
g1=$(committed g1 drr)
expect "before: records" "$(ends drr)" "$N"
expect "before: g1 committed offsets" "$([ "${g1:-0}" -gt 0 ] && echo yes || echo "no ($g1)")" "yes"
produced=$(payloads | wc -c)
for _ in $(seq 60); do
  wal=$(du -sb "$D/wal" | cut -f1)
  [ $((wal * 10)) -lt $produced ] && break
  sleep 5
done
for _ in $(seq 60); do [ "$(sql_count drr)" = "$N" ] && break; sleep 5; done
expect "before: /_sql count" "$(sql_count drr)" "$N"
sleep 6 # two upload intervals after the last change
echo "  images: $(keys dr/ | tr '\n' ' ')"
expect "catalog image uploaded" "$(keys dr/catalog/ | grep -c 'node-1.img')" "1"
expect "local state image uploaded" "$(keys dr/local/ | grep -c 'node-1.img')" "1"
stop_node

echo "-- control: data directory lost, restore disabled"
wipe
start_node control CHRONIK_DR_RESTORE=false
expect "control: topic unknown without restore" "$(partitions drr)" "0"
sleep 5 # past an upload interval: the blank node must not replace the backup
expect "control: uploads refused over the backup" "$(grep -c 'uploads paused' "$LOG-control.log")" "1"
stop_node

echo "-- data directory lost, restored from the object store"
wipe
start_node restored
grep -E 'Restored from the object store' "$LOG-restored.log" | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-220 | sed 's/^/  /'
expect "restored: partitions of drr" "$(partitions drr)" "$PARTS"
expect "restored: partitions of drr-cfg" "$(partitions drr-cfg)" "2"
cfg=$($K/kafka-configs.sh --bootstrap-server $B --describe --entity-type topics --entity-name drr-cfg 2>/dev/null | grep -o 'retention.ms=[0-9]*' | head -1)
expect "restored: drr-cfg config" "$cfg" "retention.ms=123456789"
expect "restored: records" "$(ends drr)" "$N"
got=$(kcat -C -b $B -t drr -o beginning -e -q -f '%s\n' 2>/dev/null | sort | md5sum | cut -d' ' -f1)
expect "restored: payloads (sorted md5)" "$got" "$(payloads | sort | md5sum | cut -d' ' -f1)"
sql_body=$(curl -s -m 30 -X POST localhost:6092/_sql -H "Content-Type: application/json" -d '{"query":"SELECT COUNT(*) AS c FROM drr"}' | head -c 300)
echo "  /_sql right after the restore: $sql_body"
for _ in $(seq 40); do [ "$(sql_count drr)" = "$N" ] && break; sleep 5; done
expect "restored: /_sql count (Parquet rebuilt from the archive)" "$(sql_count drr)" "$N"
expect "restored: g1 committed offsets" "$(committed g1 drr)" "$g1"
seq 1 10 | kcat -P -b $B -t drr -p 0 -X acks=all 2>/dev/null
expect "restored: new records continue the offsets" "$(ends drr)" "$((N + 10))"
first_new=$(kcat -C -b $B -t drr -p 0 -o -10 -c 1 -e -q -f '%o\n' 2>/dev/null)
p0_before=$(( (N + PARTS - 1) / PARTS )) # records on partition 0 before
expect "restored: first new offset on partition 0" "$first_new" "$p0_before"
stop_node

echo "-- restart after the restore"
start_node again
expect "again: records" "$(ends drr)" "$((N + 10))"
expect "again: g1 committed offsets" "$(committed g1 drr)" "$g1"
expect "again: partitions of drr-cfg" "$(partitions drr-cfg)" "2"
expect "panics" "$(cat "$LOG"-*.log | grep -c panicked)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
