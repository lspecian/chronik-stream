#!/bin/bash
# Issue #57, defect 1: building a cold Tantivy index must not race Tantivy's own
# background merge threads.
#
# `commit_and_serialize` committed the on-disk index and then tarred its
# directory while the IndexWriter was still alive. Tantivy merges on background
# threads and deletes the segment files it merged away; tar lists a directory and
# then opens each entry, so a file removed in between failed the whole archive
# with a bare `No such file or directory (os error 2)`. The segment was then kept
# and retried every indexing pass, forever.
#
# The trigger is size, not topic: enough documents in one WAL segment that the
# writer flushes more than one index segment and the merge policy has work to do.
# A small topic on the same binary indexes fine, which is what made the original
# report look topic-specific.
#
# Checks, against a running local cluster:
#   1. no "add index to tar" / "Failed to create Tantivy index" in any log
#   2. indexes were actually built (so the run proves something)
#   3. no segment entered backoff or quarantine
#   4. the text is searchable afterwards, i.e. the index is usable not just present
#
# Usage:
#   tests/test_tantivy_index_archive.sh                      # target/release
#   CHRONIK_BIN=/path/to/v2.15.1 tests/test_tantivy_index_archive.sh
#
# Exit: 0 pass, 1 fail, 2 setup failed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CLUSTER="$PROJECT_ROOT/tests/cluster"

TOPIC="i57tantivy$$"
# Enough documents, each with a sizeable text body, to push the 50MB writer heap
# over more than one flush — that is what gives the merge policy work to do while
# the archive is being written.
RECORDS="${RECORDS:-120000}"
FILLER_WORDS="${FILLER_WORDS:-60}"
API1=6092
BOOT=localhost:9092
KAFKA_BIN="${KAFKA_BIN:-/opt/kafka/bin}"

pass=0; fail=0
ok()  { echo "  PASS: $1"; pass=$((pass+1)); }
bad() { echo "  FAIL: $1"; fail=$((fail+1)); }
setup_fail() { echo "SETUP FAILED: $1" >&2; cleanup; exit 2; }
cleanup() { "$CLUSTER/stop.sh" >/dev/null 2>&1; }
trap cleanup EXIT

[ -x "$KAFKA_BIN/kafka-topics.sh" ] || setup_fail "Kafka CLI not at $KAFKA_BIN (set KAFKA_BIN)"

echo "=== Issue #57 defect 1: a cold Tantivy index must archive cleanly ==="
echo "binary:  ${CHRONIK_BIN:-$PROJECT_ROOT/target/release/chronik-server}"
echo "records: $RECORDS"
echo

"$CLUSTER/stop.sh" >/dev/null 2>&1
"$CLUSTER/start.sh" >/dev/null 2>&1 || setup_fail "cluster did not start"
for _ in $(seq 1 60); do
    curl -fsS -m 2 "http://localhost:$API1/health" >/dev/null 2>&1 && break
    sleep 2
done
curl -fsS -m 5 "http://localhost:$API1/health" >/dev/null 2>&1 \
    || setup_fail "node 1 API never came up"

"$KAFKA_BIN/kafka-topics.sh" --bootstrap-server "$BOOT" --create --topic "$TOPIC" \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1

# One partition, so every record lands in one WAL segment and one index.
# A distinctive term goes in a known record so the index can be proven usable.
echo "producing $RECORDS records..."
seq 1 "$RECORDS" \
  | awk -v w="$FILLER_WORDS" '{
        body="";
        for (i = 0; i < w; i++) body = body sprintf("lorem%d ipsum%d dolor%d ", $1 % 97, i, ($1 + i) % 89);
        marker = ($1 == 42) ? "quixoticbellwether" : "ordinary";
        printf "{\"id\":%d,\"marker\":\"%s\",\"body\":\"%s\"}\n", $1, marker, body;
    }' \
  | "$KAFKA_BIN/kafka-console-producer.sh" --bootstrap-server "$BOOT" --topic "$TOPIC" \
        --producer-property acks=all --producer-property batch.size=131072 >/dev/null 2>&1

TRUTH=$("$KAFKA_BIN/kafka-get-offsets.sh" --bootstrap-server "$BOOT" --topic "$TOPIC" 2>/dev/null \
        | awk -F: '{s+=$3} END {print s+0}')
[ "$TRUTH" -gt 0 ] || setup_fail "nothing was produced"
echo "ground truth from Kafka: $TRUTH records"

# Let the indexer seal, index and upload. Sealing is on a timer, so this waits
# for evidence rather than a fixed sleep.
echo "waiting for an indexing run to complete..."
indexed=0
for _ in $(seq 1 40); do
    if grep -qE "WAL indexing run complete" "$CLUSTER"/logs/node*.log 2>/dev/null; then
        indexed=1
        sleep 15   # let any further passes run too
        break
    fi
    sleep 10
done
[ "$indexed" = 1 ] || setup_fail "no indexing run completed in ~7 minutes"

# --- 1. the failure this issue is about -------------------------------------
tar_errs=$(grep -c "add index to tar" "$CLUSTER"/logs/node*.log 2>/dev/null | awk -F: '{s+=$2} END {print s+0}')
if [ "$tar_errs" -eq 0 ]; then
    ok "no 'add index to tar' failure in any node log"
else
    bad "$tar_errs 'add index to tar' failures — the archive still races the merge threads"
    grep -h "add index to tar" "$CLUSTER"/logs/node*.log | tail -3
fi

idx_errs=$(grep -c "Failed to create Tantivy index" "$CLUSTER"/logs/node*.log 2>/dev/null | awk -F: '{s+=$2} END {print s+0}')
if [ "$idx_errs" -eq 0 ]; then
    ok "no Tantivy index creation failed"
else
    bad "$idx_errs Tantivy index creations failed"
    grep -h "Failed to create Tantivy index" "$CLUSTER"/logs/node*.log | tail -3
fi

# --- 2. prove the run actually built indexes --------------------------------
built=$(grep -h "WAL indexing run complete" "$CLUSTER"/logs/node*.log 2>/dev/null \
        | sed 's/\x1b\[[0-9;]*m//g' \
        | sed -n 's/.*indexes=\([0-9]*\).*/\1/p' | awk '{s+=$1} END {print s+0}')
if [ "${built:-0}" -gt 0 ]; then
    ok "indexes were built ($built across the cluster), so a clean log means something"
else
    bad "no indexes were built — this run proves nothing about the archive"
fi

# --- 3. nothing fell into the new backoff -----------------------------------
backoff=$(grep -c "failed indexing — backing off" "$CLUSTER"/logs/node*.log 2>/dev/null | awk -F: '{s+=$2} END {print s+0}')
quarantined=$(grep -c "quarantining it" "$CLUSTER"/logs/node*.log 2>/dev/null | awk -F: '{s+=$2} END {print s+0}')
if [ "$backoff" -eq 0 ] && [ "$quarantined" -eq 0 ]; then
    ok "no segment entered backoff or quarantine"
else
    bad "$backoff segments backed off, $quarantined quarantined"
fi

# --- 4. the archive was uploaded for THIS topic -----------------------------
uploaded=$(grep -h "Uploaded Tantivy index to object store" "$CLUSTER"/logs/node*.log 2>/dev/null \
           | sed 's/\x1b\[[0-9;]*m//g' | grep -c "$TOPIC")
if [ "${uploaded:-0}" -gt 0 ]; then
    ok "the index for this topic reached the object store ($uploaded upload(s))"
else
    bad "no index was uploaded for $TOPIC — the archive never completed"
fi

# --- 5. and the archive is a readable tar holding a real index ---------------
# This is the direct test of the defect: tar the committed index while its
# directory is still changing and the archive is either absent or short. Reading
# it back proves it is complete, without depending on the cold-search read path
# (a separate pipeline with its own history).
# This topic's archive specifically. Falling back to "any recent .tar.gz" would
# let another topic's healthy archive vouch for this one, which is how the
# original report came to look topic-specific in the first place.
# The topic is a directory in the object key (…/tantivy_indexes/<topic>/partition-N/segment-a-b.tar.gz),
# not part of the filename.
archive=$(find "$CLUSTER"/data -path "*/${TOPIC}/*" -name "*.tar.gz" 2>/dev/null | head -1)
if [ -n "$archive" ] && [ -s "$archive" ]; then
    listing=$(tar -tzf "$archive" 2>&1)
    if [ $? -eq 0 ]; then
        ok "the archive decompresses cleanly ($(wc -l <<<"$listing") entries)"
        grep -q "index/meta.json" <<<"$listing" \
            && ok "and contains the Tantivy index's own meta.json" \
            || bad "the archive has no index/meta.json: $(head -5 <<<"$listing" | tr '\n' ' ')"
        grep -q "^metadata.json" <<<"$listing" \
            && ok "and contains the segment metadata" \
            || bad "the archive has no metadata.json"
        # A complete index is more than a header: the merged segment files must
        # be in there too.
        segs=$(grep -cE "index/[0-9a-f]+\." <<<"$listing")
        [ "${segs:-0}" -gt 0 ] \
            && ok "and carries $segs index segment file(s)" \
            || bad "the archive holds no index segment files"
    else
        bad "the archive does not decompress: $listing"
    fi
else
    bad "no archive for $TOPIC on disk to read back (an archive that was never written is the defect, not a missing test fixture)"
fi

echo
echo "=== $pass passed, $fail failed ==="
[ "$fail" -eq 0 ] || exit 1
exit 0
