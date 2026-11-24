#!/bin/bash
# Before rolling a cluster to the first version that enforces retention: keep
# everything on the topics that already exist, as before.
#
# Up to v2.19.2 retention.ms / retention.bytes were not enforced on archived
# data, so existing topics kept everything, whatever their stored retention
# (most carry the 7-day default nobody chose). From then on retention is
# enforced like Kafka. This sets retention.ms=-1 and retention.bytes=-1 on every
# existing topic (internal `__` topics excepted), so nothing on this cluster is
# deleted unless someone sets a limit afterwards. Topics created later get
# Kafka's defaults (7 days, no size limit).
#
# Run it against the cluster BEFORE the roll: the new version's first
# retention pass runs a minute after boot.
#
# Usage: keep_existing_topics_forever.sh <bootstrap> [--apply]
#   Without --apply it lists the topics and their current dynamic configs.
# Needs: KAFKA_HOME (kafka-topics.sh, kafka-configs.sh).
set -u
BOOTSTRAP=${1:?bootstrap servers, e.g. localhost:9092}; APPLY=${2:-}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
PARALLEL=${PARALLEL:-8}

topics=$($K/kafka-topics.sh --bootstrap-server "$BOOTSTRAP" --list 2>/dev/null | grep -v '^__' | grep -v '^$')
n=$(printf '%s\n' "$topics" | grep -c .)
echo "$n topic(s) on $BOOTSTRAP (internal __ topics excluded)"
if [ "$APPLY" != "--apply" ]; then
  printf '%s\n' "$topics" | head -20 | while read -r t; do
    printf '  %s: %s\n' "$t" "$($K/kafka-configs.sh --bootstrap-server "$BOOTSTRAP" --describe --entity-type topics \
      --entity-name "$t" 2>/dev/null | grep -o 'retention\.[a-z]*=[0-9-]*' | paste -sd' ' -)"
  done
  [ "$n" -gt 20 ] && echo "  ... (first 20 shown)"
  echo "dry run: re-run with --apply to set retention.ms=-1,retention.bytes=-1 on all $n"
  exit 0
fi

failed=$(printf '%s\n' "$topics" | xargs -P "$PARALLEL" -I{} sh -c \
  "$K/kafka-configs.sh --bootstrap-server '$BOOTSTRAP' --alter --entity-type topics --entity-name '{}' \
     --add-config retention.ms=-1,retention.bytes=-1 >/dev/null 2>&1 || echo '{}'")
if [ -n "$failed" ]; then
  echo "FAILED on:"; printf '  %s\n' $failed; exit 1
fi
echo "retention.ms=-1, retention.bytes=-1 set on $n topic(s)"
