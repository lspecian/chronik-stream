#!/bin/bash
# CreateTopics racing auto-create (parked since 2026-11-14, thunderbird v2.16.0):
# an explicit `--partitions 1` create on one node and a producer auto-creating
# the same name (3 partitions by default) on another both succeeded, and the
# producer wrote to partitions 1 and 2 of a topic the catalog said had one —
# records no consumer of the topic ever saw.
#
# Since v2.17.0 both creates are catalog commands applied in one Raft order, so
# exactly one may win. Each round races the two on different nodes and checks:
#   - every node reports the same partition count for the topic;
#   - every record the producer had acknowledged is readable from the
#     partitions the topic has (none landed in a partition it does not have);
#   - the explicit create failed exactly when the topic has 3 partitions.
#   PASS = all rounds consistent.
set -u
TEST_BIN=${1:?binary under test}; LABEL=${2:?label}
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
B=localhost:9092,localhost:9093,localhost:9094
ROUNDS=${CREATE_RACE_ROUNDS:-20}
N=60
cd "$REPO"
. tests/cluster/ports_free.sh
up() { for i in $(seq 90); do $K/kafka-topics.sh --bootstrap-server $1 --list >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
partitions_on() { $K/kafka-topics.sh --bootstrap-server localhost:$1 --describe --topic $2 2>/dev/null | grep -c "Partition: "; }

echo "=== $LABEL ($TEST_BIN)"
CHRONIK_BIN=$TEST_BIN ./tests/cluster/start.sh >/dev/null 2>&1 || { echo "FAIL: start.sh failed"; exit 1; }
up localhost:9092 || { echo "FAIL: cluster did not start"; exit 1; }
sleep 5
R=$(for n in 1 2 3; do curl -s -m 2 localhost:$((6091 + n))/admin/health | grep -q '"is_leader":true' && echo $n; done | head -1)
[ -n "$R" ] || { echo "FAIL: no node reports catalog leadership"; exit 1; }
O=$(( R % 3 + 1 ))   # another node, for the producer
echo "catalog leader node$R takes the explicit create; node$O takes the auto-creating producer"

W=tests/cluster/data/create_race; mkdir -p $W
DELAYS=(0 0.8 1.2 1.6 2.0 2.4 3.0)
fail=0; one=0; three=0
for i in $(seq "$ROUNDS"); do
  T=race-$i-$$
  seq 1 $N | sed "s/.*/k&:v&/" > $W/race.in
  ( $K/kafka-topics.sh --bootstrap-server localhost:$((9091 + R)) --create --topic $T \
      --partitions 1 --replication-factor 3 >$W/race.create 2>&1; echo $? >$W/race.crc ) &
  # Stagger the producer against kafka-topics.sh (a JVM, ~1-2 s to start),
  # so both orders happen: with no delay the producer wins every round.
  sleep "${DELAYS[$(( i % ${#DELAYS[@]} ))]}"
  kcat -P -b localhost:$((9091 + O)) -t $T -K: -X acks=all -X message.timeout.ms=30000 \
      -X topic.metadata.refresh.interval.ms=500 <$W/race.in 2>$W/race.prod
  prc=$?
  wait
  crc=$(cat $W/race.crc)
  sleep 2
  counts=$(for n in 1 2 3; do partitions_on $((9091 + n)) $T; done | sort -u | paste -sd,)
  read_back=$(timeout 60 kcat -C -b $B -t $T -o beginning -e -q 2>/dev/null | wc -l)
  verdict=ok
  case "$counts" in
    1) one=$((one + 1)); [ "$crc" = 0 ] || verdict="topic has 1 partition but the explicit create failed" ;;
    3) three=$((three + 1)); [ "$crc" != 0 ] || verdict="topic has 3 partitions but the explicit create succeeded" ;;
    *) verdict="nodes disagree on the partition count: $counts" ;;
  esac
  [ "$prc" = 0 ] || verdict="producer failed: $(tail -2 $W/race.prod | paste -sd' ')"
  [ "$read_back" = "$N" ] || verdict="$verdict; read back $read_back of $N acknowledged records"
  [ "$verdict" = ok ] || { echo "  round $i ($T): FAIL: $verdict"; fail=1; }
done
echo "rounds: $ROUNDS (explicit create won $one, auto-create won $three)"
[ $one -gt 0 ] && [ $three -gt 0 ] || { echo "FAIL: only one order happened (explicit $one, auto $three); the race was not exercised"; fail=1; }
panics=$(cat tests/cluster/logs/node*.log | grep -c panicked)
echo "panics: $panics"; [ "$panics" = 0 ] || fail=1
./tests/cluster/stop.sh >/dev/null 2>&1
if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
