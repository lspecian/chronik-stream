#!/bin/bash
# The whole cluster loses its disks; it comes back from the object store.
#
#   1. Three nodes archive to SeaweedFS (S3), uploading disaster-recovery images
#      every 2 s. Topics (one with a config), a SCRAM user, 12,000 records at
#      acks=all, and a consumer group's committed offsets.
#   2. Every node's data directory is deleted.
#   3. Control: the nodes start normally. A cluster node is never restored
#      without CHRONIK_DR_RESTORE=true: the topics are unknown, and the blank
#      nodes refuse to overwrite the images of the lost cluster.
#   4. The data directories are deleted again and every node starts with
#      CHRONIK_DR_RESTORE=true: all nodes found the cluster on the same catalog
#      image, and on every node the topics, partitions, config and user are
#      back; the records are readable byte for byte, the group's offsets are
#      back, acks=all writes continue the offsets and every partition returns to
#      a full ISR.
#
# Usage: dr_cluster_restore.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, docker (pulls chrislusf/seaweedfs).
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
source "$(dirname "$0")/../catalog/lib.sh"
command -v docker >/dev/null || { echo "SKIP: docker not installed"; exit 77; }
S3=http://127.0.0.1:8333; BUCKET=chronik-drc
PREFIX="run-$LABEL-$(date +%s)"
SW=chronik-drc-seaweedfs
T=drc; N=12000
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
keys() { curl -s "$S3/$BUCKET?list-type=2&max-keys=1000&prefix=$PREFIX/$1" | grep -o '<Key>[^<]*</Key>' | sed 's/<[^>]*>//g'; }
payloads() { seq 0 $((N - 1)) | awk '{ printf "{\"n\":%d,\"pad\":\"%048d\"}\n", $1, $1 }'; }
ends() { "$K"/kafka-get-offsets.sh --bootstrap-server "$BOOTS" --topic "$1" 2>/dev/null | awk -F: '{s+=$3} END {print s+0}'; }
partitions_on() { # node topic (never auto-creates)
  "$K"/kafka-topics.sh --bootstrap-server "localhost:$(port_of "$1")" --describe --topic "$2" 2>/dev/null | grep -c 'Partition:'
}
committed() {
  "$K"/kafka-consumer-groups.sh --bootstrap-server localhost:9092 --describe --group "$1" 2>/dev/null \
    | awk -v t="$2" '$2 == t && $4 ~ /^[0-9]+$/ { s += $4 } END { print s+0 }'
}
# SASL is optional on these brokers: a login offered is verified.
scram_ok() { # port user password
  kcat -b "localhost:$1" -X security.protocol=SASL_PLAINTEXT -X sasl.mechanisms=SCRAM-SHA-512 \
    -X sasl.username="$2" -X sasl.password="$3" -L -t __none -m 10 >/dev/null 2>&1
}
wipe_all() {
  for n in 1 2 3; do
    local d="$DIR/data/node$n"
    [ -d "$d" ] && find "$d" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
  done
}

echo "=== $LABEL ($CHRONIK_BIN)"
docker rm -f $SW >/dev/null 2>&1
docker run -d --rm --name $SW -p 8333:8333 chrislusf/seaweedfs server -s3 >/dev/null \
  || { echo "FAIL: could not start SeaweedFS"; exit 1; }
trap 'cluster_stop; docker rm -f $SW >/dev/null 2>&1' EXIT
for _ in $(seq 60); do curl -s -o /dev/null -m 2 "$S3" && break; sleep 1; done
curl -s -X PUT "$S3/$BUCKET" >/dev/null
BASE_ENV="CHRONIK_WAL_ROTATION_SIZE=100KB CHRONIK_DR_UPLOAD_INTERVAL_SECS=2 OBJECT_STORE_BACKEND=s3 S3_ENDPOINT=$S3 \
S3_BUCKET=$BUCKET S3_PREFIX=$PREFIX S3_REGION=us-east-1 S3_ACCESS_KEY=any S3_SECRET_KEY=any S3_PATH_STYLE=true S3_DISABLE_SSL=true \
CHRONIK_SASL_ENABLED=optional CHRONIK_INTER_BROKER_SECRET=drc-inter-broker-secret-0001"

export EXTRA_ENV="$BASE_ENV"
cluster_start || { echo "FAIL: cluster did not start"; exit 1; }
sleep 5
"$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic $T --partitions 6 --replication-factor 3 >/dev/null 2>&1
"$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --create --topic drc-cfg --partitions 2 --replication-factor 3 \
  --config retention.ms=987654321 >/dev/null 2>&1
"$K"/kafka-configs.sh --bootstrap-server "$BOOTS" --alter --entity-type users --entity-name drc-user \
  --add-config 'SCRAM-SHA-512=[iterations=4096,password=drc-pw]' >/dev/null 2>&1
sleep 3
payloads | kcat -P -b "$BOOTS" -t $T -X acks=all 2>/dev/null
# Group traffic through node 1 only: each broker coordinates the groups whose
# clients reach it, with offsets kept on that node (see the changelog).
kcat -C -b localhost:9092 -G gdrc $T -X auto.offset.reset=earliest -X enable.auto.commit=true -X auto.commit.interval.ms=200 \
  -c 3000 -e -q >/dev/null 2>&1
sleep 2
g=$(committed gdrc $T)
expect "before: records" "$(ends $T)" "$N"
expect "before: group committed offsets" "$([ "${g:-0}" -gt 0 ] && echo yes || echo "no ($g)")" "yes"
expect "before: SCRAM user logs in" "$(scram_ok 9092 drc-user drc-pw && echo yes || echo no)" "yes"
sleep 30 # the indexer archives, then the images are uploaded after the last change
expect "catalog images (one per node)" "$(keys dr/catalog/ | grep -c '\.img$')" "3"
expect "local state images (one per node)" "$(keys dr/local/ | grep -c '\.img$')" "3"
cluster_stop

echo "-- every disk lost; control: started without CHRONIK_DR_RESTORE"
wipe_all
cluster_start || { echo "FAIL: control cluster did not start"; exit 1; }
sleep 6
expect "control: topic unknown on node 1" "$(partitions_on 1 $T)" "0"
paused=0; for n in 1 2 3; do grep -q 'uploads paused' "$DIR/logs/node$n.log" && paused=$((paused + 1)); done
expect "control: nodes refusing to overwrite the lost cluster's images" "$paused" "3"
hinted=$(grep -l 'start every node with CHRONIK_DR_RESTORE=true' "$DIR"/logs/node*.log | wc -l)
expect "control: nodes explaining how to restore" "$hinted" "3"
cluster_stop

echo "-- every disk lost; restored with CHRONIK_DR_RESTORE=true"
wipe_all
export EXTRA_ENV="$BASE_ENV CHRONIK_DR_RESTORE=true"
cluster_start || { echo "FAIL: restored cluster did not start"; exit 1; }
sleep 5
for n in 1 2 3; do
  grep 'Restored from the object store' "$DIR/logs/node$n.log" | sed 's/\x1b\[[0-9;]*m//g' | grep -o 'catalog of node [0-9]* at index [0-9]*' | tail -1 | sed "s/^/  node $n: /"
done
sources=$(for n in 1 2 3; do grep 'Restored from the object store' "$DIR/logs/node$n.log" | grep -o 'catalog of node [0-9]* at index [0-9]*' | tail -1; done | sort -u | wc -l)
expect "every node restored the same catalog image" "$sources" "1"
for n in 1 2 3; do
  expect "node $n: partitions of $T" "$(partitions_on $n $T)" "6"
  expect "node $n: partitions of drc-cfg" "$(partitions_on $n drc-cfg)" "2"
done
cfg=$("$K"/kafka-configs.sh --bootstrap-server "$BOOTS" --describe --entity-type topics --entity-name drc-cfg 2>/dev/null | grep -o 'retention.ms=[0-9]*' | head -1)
expect "restored: drc-cfg config" "$cfg" "retention.ms=987654321"
for n in 1 2 3; do
  expect "node $n: restored SCRAM user logs in" "$(scram_ok $(port_of $n) drc-user drc-pw && echo yes || echo no)" "yes"
done
expect "restored: a wrong password is refused" "$(scram_ok 9092 drc-user wrong && echo yes || echo no)" "no"
expect "restored: records" "$(ends $T)" "$N"
got=$(kcat -C -b "$BOOTS" -t $T -o beginning -e -q -f '%s\n' 2>/dev/null | sort | md5sum | cut -d' ' -f1)
expect "restored: payloads (sorted md5)" "$got" "$(payloads | sort | md5sum | cut -d' ' -f1)"
expect "restored: group committed offsets" "$(committed gdrc $T)" "$g"
seq 1 60 | kcat -P -b "$BOOTS" -t $T -X acks=all 2>/dev/null
expect "restored: acks=all writes continue" "$(ends $T)" "$((N + 60))"
short=99
for _ in $(seq 60); do
  short=$("$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --describe --topic $T 2>/dev/null \
    | grep -o 'Isr: [0-9,]*' | awk -F'[ ,]' '{ if (NF - 1 < 3) n++ } END { print n+0 }')
  [ "$short" = 0 ] && break
  sleep 2
done
expect "restored: partitions short of a full ISR" "$short" "0"
expect "panics" "$(panics)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
