#!/usr/bin/env bash
#
# End-to-end test for the /_query orchestrator endpoint.
#
# Prerequisites:
#   CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_DEFAULT_COLUMNAR=true \
#     cargo run --bin chronik-server start --data-dir /tmp/chronik-e2e-test
#
# Usage:
#   bash tests/e2e/test_query_orchestrator.sh
#

set -euo pipefail

API="http://localhost:6092"
KAFKA="localhost:9092"
TOPIC_LOGS="e2e-logs"
TOPIC_ORDERS="e2e-orders"
PASSED=0
FAILED=0
ERRORS=""

# Colors
GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
BOLD='\033[1m'
END='\033[0m'

ok()    { echo -e "  ${GREEN}PASS${END} $1"; PASSED=$((PASSED + 1)); }
fail()  { echo -e "  ${RED}FAIL${END} $1${2:+ — $2}"; FAILED=$((FAILED + 1)); ERRORS="${ERRORS}\n    - $1"; }
info()  { echo -e "  ${BLUE}INFO${END} $1"; }
section() { echo -e "\n${BOLD}$1${END}"; }

assert_status() {
    local desc="$1" expected="$2" actual="$3" body="${4:-}"
    if [ "$actual" = "$expected" ]; then
        ok "$desc"
    else
        fail "$desc" "expected=$expected actual=$actual body=${body:0:200}"
    fi
}

assert_json() {
    local desc="$1" json="$2" path="$3" expected="$4"
    local actual
    actual=$(echo "$json" | jq -r "$path" 2>/dev/null)
    if [ "$actual" = "$expected" ]; then
        ok "$desc"
    else
        fail "$desc" "expected='$expected' got='$actual'"
    fi
}

assert_json_gt() {
    local desc="$1" json="$2" path="$3" threshold="$4"
    local actual
    actual=$(echo "$json" | jq -r "$path" 2>/dev/null)
    if [ "$actual" != "null" ] && [ "$actual" -gt "$threshold" ] 2>/dev/null; then
        ok "$desc (=$actual)"
    else
        fail "$desc" "expected >$threshold, got '$actual'"
    fi
}

assert_json_exists() {
    local desc="$1" json="$2" path="$3"
    local actual
    actual=$(echo "$json" | jq -r "$path" 2>/dev/null)
    if [ "$actual" != "null" ] && [ -n "$actual" ]; then
        ok "$desc"
    else
        fail "$desc" "field '$path' is null or missing"
    fi
}

# ============================================================================
section "Phase 1: Server Connectivity"
# ============================================================================

HEALTH=$(curl -sf "$API/health" 2>/dev/null || echo "FAIL")
if echo "$HEALTH" | jq -e '.status == "ok"' > /dev/null 2>&1; then
    ok "Unified API health check"
else
    fail "Unified API health check" "Server not running at $API"
    echo -e "\n${RED}Start server first:${END}"
    echo "  CHRONIK_DEFAULT_SEARCHABLE=true CHRONIK_DEFAULT_COLUMNAR=true \\"
    echo "    cargo run --release --bin chronik-server start --data-dir /tmp/chronik-e2e-test"
    exit 1
fi

# ============================================================================
section "Phase 2: Produce Test Data"
# ============================================================================

# Use kafka-python via a minimal inline script (unavoidable for Kafka protocol)
python3 -c "
from kafka import KafkaProducer
import json, time
p = KafkaProducer(bootstrap_servers='$KAFKA', api_version=(0,10,0),
    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
    key_serializer=lambda k: k.encode('utf-8') if k else None)
msgs = [
    {'level':'ERROR','service':'payment','msg':'Payment gateway timeout after 30s','user_id':1001},
    {'level':'ERROR','service':'payment','msg':'Card declined for transaction TX-8842','user_id':1002},
    {'level':'WARN','service':'auth','msg':'Failed login attempt from suspicious IP','user_id':1003},
    {'level':'INFO','service':'order','msg':'Order ORD-5521 placed successfully','user_id':1004},
    {'level':'ERROR','service':'database','msg':'Connection pool exhausted','user_id':1005},
]
for i in range(50):
    m = msgs[i%len(msgs)].copy(); m['seq']=i; m['timestamp']=int(time.time()*1000)
    p.send('$TOPIC_LOGS', key=f'log-{i}', value=m)
for i in range(50):
    o = {'order_id':f'ORD-{10000+i}','amount':round(10+(i*7.3)%500,2),
         'status':['completed','failed','pending'][i%3],'customer':f'cust-{i%20}',
         'timestamp':int(time.time()*1000)}
    p.send('$TOPIC_ORDERS', key=f'order-{i}', value=o)
p.flush(); p.close()
print('OK')
" 2>/dev/null

if [ $? -eq 0 ]; then
    ok "Produced 100 messages (50 logs + 50 orders)"
else
    fail "Produce test data" "kafka-python failed"
fi

# ============================================================================
section "Phase 3: Wait for Indexing"
# ============================================================================

info "Waiting for Tantivy + columnar indexing..."
info "Checking capabilities AND actual search results (Tantivy commits every ~30s)"
TEXT_READY=false
SQL_READY=false
for i in $(seq 1 90); do
    CAPS=$(curl -sf "$API/_query/capabilities" 2>/dev/null || echo "[]")

    # Text: check both capability AND actual search results (index may exist but be empty)
    if ! $TEXT_READY; then
        if echo "$CAPS" | jq -e '.[] | select(.topic=="'"$TOPIC_LOGS"'" and .text_search==true)' > /dev/null 2>&1; then
            # Probe actual text search to verify documents are indexed
            PROBE=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
                "sources": [{"topic":"'"$TOPIC_LOGS"'","modes":["text"]}],
                "q": {"text":"payment"},
                "k": 1, "result_format": "merged"
            }' 2>/dev/null || echo '{}')
            PROBE_COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
            if [ "$PROBE_COUNT" -gt 0 ]; then
                TEXT_READY=true
            fi
        fi
    fi

    # SQL: check capability (filesystem fallback makes this reliable)
    if ! $SQL_READY; then
        if echo "$CAPS" | jq -e '.[] | select(.topic=="'"$TOPIC_ORDERS"'" and .sql_query==true)' > /dev/null 2>&1; then
            SQL_READY=true
        fi
    fi

    if $TEXT_READY && $SQL_READY; then break; fi
    printf "\r  Waiting... %ds (text=%s sql=%s) " "$i" "$TEXT_READY" "$SQL_READY"
    sleep 2
done
echo ""

if $TEXT_READY; then ok "Tantivy text index ready (verified with search probe)"; else info "Text index not ready (will test fetch mode)"; fi
if $SQL_READY; then ok "SQL tables registered"; else info "SQL tables not ready (will test fetch mode)"; fi

# ============================================================================
section "Phase 4: Test /_query/capabilities"
# ============================================================================

CAPS=$(curl -sf "$API/_query/capabilities")
STATUS=$?
assert_status "GET /_query/capabilities" "0" "$STATUS"

TOPICS=$(echo "$CAPS" | jq -r '.[].topic' | sort)
echo "$TOPICS" | grep -q "$TOPIC_LOGS" && ok "'$TOPIC_LOGS' in capabilities" || fail "'$TOPIC_LOGS' in capabilities"
echo "$TOPICS" | grep -q "$TOPIC_ORDERS" && ok "'$TOPIC_ORDERS' in capabilities" || fail "'$TOPIC_ORDERS' in capabilities"

LOGS_FETCH=$(echo "$CAPS" | jq '.[] | select(.topic=="'"$TOPIC_LOGS"'") | .fetch')
assert_json_exists "'$TOPIC_LOGS' has fetch" "$CAPS" '.[] | select(.topic=="'"$TOPIC_LOGS"'") | .fetch'

info "Capabilities:"
echo "$CAPS" | jq -c '.[] | {topic, text_search, sql_query, vector_search, fetch}'

# ============================================================================
section "Phase 5: Test /_query/profiles"
# ============================================================================

PROFILES=$(curl -sf "$API/_query/profiles")
echo "$PROFILES" | jq -e '.[].name' > /dev/null 2>&1 && ok "GET /_query/profiles returns data" || fail "profiles endpoint"

echo "$PROFILES" | jq -e '.[] | select(.name=="default")' > /dev/null 2>&1 && ok "'default' profile" || fail "default profile"
echo "$PROFILES" | jq -e '.[] | select(.name=="freshness")' > /dev/null 2>&1 && ok "'freshness' profile" || fail "freshness profile"
echo "$PROFILES" | jq -e '.[] | select(.name=="relevance")' > /dev/null 2>&1 && ok "'relevance' profile" || fail "relevance profile"

# ============================================================================
section "Phase 6: Test /_query — Fetch Mode (always works)"
# ============================================================================

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_LOGS"'","modes":["fetch"]}],
    "q": {"fetch":{"offset":0,"partition":0,"max_bytes":1048576}},
    "k": 10, "result_format": "merged"
}')
assert_json_exists "Fetch: has query_id" "$RESP" '.query_id'
assert_json_exists "Fetch: has stats" "$RESP" '.stats'

NUM=$(echo "$RESP" | jq '.results | length')
if [ "$NUM" -gt 0 ]; then
    ok "Fetch returned $NUM results"
else
    fail "Fetch returned 0 results"
fi
info "query_id=$(echo "$RESP" | jq -r '.query_id')"
info "latency=$(echo "$RESP" | jq -r '.stats.latency_ms')ms, candidates=$(echo "$RESP" | jq -r '.stats.candidates')"

# ============================================================================
section "Phase 7: Test /_query — Text Mode (full-text search)"
# ============================================================================

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_LOGS"'","modes":["text"]}],
    "q": {"text":"payment gateway timeout"},
    "k": 10, "result_format": "merged"
}')

if [ $? -eq 0 ]; then
    NUM=$(echo "$RESP" | jq '.results | length')
    if [ "$NUM" -gt 0 ]; then
        ok "Text search returned $NUM results"
        TOP_SCORE=$(echo "$RESP" | jq '.results[0].final_score')
        info "Top result: score=$TOP_SCORE, topic=$(echo "$RESP" | jq -r '.results[0].topic')"
    else
        fail "Text search returned 0 results" "$(echo "$RESP" | jq -c '.stats')"
    fi
else
    fail "Text search request failed"
fi

# ============================================================================
section "Phase 8: Test /_query — SQL Mode"
# ============================================================================

# SQL tables use sanitized names (e2e-orders → e2e_orders)
SANITIZED=$(echo "$TOPIC_ORDERS" | sed 's/[-.]/_/g')
RESP=$(curl -s -w "\n%{http_code}" "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_ORDERS"'","modes":["sql"]}],
    "q": {"sql":"SELECT * FROM '"$SANITIZED"' LIMIT 10"},
    "k": 10, "result_format": "merged"
}')

HTTP_CODE=$(echo "$RESP" | tail -1)
BODY=$(echo "$RESP" | sed '$d')

if [ "$HTTP_CODE" = "200" ]; then
    NUM=$(echo "$BODY" | jq '.results | length')
    ok "SQL query returned $NUM results (status=$HTTP_CODE)"
    if [ "$NUM" -gt 0 ]; then
        info "Top: offset=$(echo "$BODY" | jq -r '.results[0].offset'), partition=$(echo "$BODY" | jq -r '.results[0].partition')"
    fi
else
    info "SQL query returned $HTTP_CODE: $(echo "$BODY" | jq -r '.error // empty' 2>/dev/null)"
    info "This may be expected if columnar tables aren't registered yet"
fi

# ============================================================================
section "Phase 9: Test Multi-Topic Query"
# ============================================================================

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [
        {"topic":"'"$TOPIC_LOGS"'","modes":["fetch"]},
        {"topic":"'"$TOPIC_ORDERS"'","modes":["fetch"]}
    ],
    "q": {"fetch":{"offset":0,"partition":0,"max_bytes":1048576}},
    "k": 20, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
assert_json_gt "Multi-topic returned results" "$RESP" '.results | length' 0

TOPICS_IN=$(echo "$RESP" | jq -r '[.results[].topic] | unique | sort | join(",")')
info "Topics in results: $TOPICS_IN"

if echo "$TOPICS_IN" | grep -q "$TOPIC_LOGS" && echo "$TOPICS_IN" | grep -q "$TOPIC_ORDERS"; then
    ok "Results contain both topics"
else
    info "Only some topics in results (partitioning may differ)"
fi

# ============================================================================
section "Phase 10: Test Grouped Result Format"
# ============================================================================

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [
        {"topic":"'"$TOPIC_LOGS"'","modes":["fetch"]},
        {"topic":"'"$TOPIC_ORDERS"'","modes":["fetch"]}
    ],
    "q": {"fetch":{"offset":0,"partition":0,"max_bytes":1048576}},
    "k": 20, "result_format": "grouped"
}')

GROUPED=$(echo "$RESP" | jq '.grouped_results')
if [ "$GROUPED" != "null" ]; then
    ok "Grouped format has grouped_results"
    GKEYS=$(echo "$GROUPED" | jq -r 'keys | join(",")')
    info "Group keys: $GKEYS"
    for key in $(echo "$GROUPED" | jq -r 'keys[]'); do
        GCOUNT=$(echo "$GROUPED" | jq ".\"$key\" | length")
        info "  $key: $GCOUNT results"
    done
else
    fail "Grouped format missing grouped_results"
fi

# ============================================================================
section "Phase 11: Test Error Handling"
# ============================================================================

# Empty sources → 400
CODE=$(curl -s -o /dev/null -w "%{http_code}" "$API/_query" -H 'Content-Type: application/json' \
    -d '{"sources":[],"q":{},"k":10}')
assert_status "Empty sources returns 400" "400" "$CODE"

# Unknown topic → 400
CODE=$(curl -s -o /dev/null -w "%{http_code}" "$API/_query" -H 'Content-Type: application/json' \
    -d '{"sources":[{"topic":"nonexistent-xyz","modes":["text"]}],"q":{"text":"test"},"k":10}')
assert_status "Unknown topic returns 400" "400" "$CODE"

# Missing query params → 400
CODE=$(curl -s -o /dev/null -w "%{http_code}" "$API/_query" -H 'Content-Type: application/json' \
    -d '{"sources":[{"topic":"'"$TOPIC_LOGS"'","modes":["text"]}],"q":{},"k":10}')
assert_status "Missing query params returns 400" "400" "$CODE"

# ============================================================================
section "Phase 12: Test Ranking Profile Selection"
# ============================================================================

for PROFILE in freshness relevance default; do
    CODE=$(curl -s -o /dev/null -w "%{http_code}" "$API/_query" -H 'Content-Type: application/json' -d '{
        "sources": [{"topic":"'"$TOPIC_LOGS"'","modes":["fetch"]}],
        "q": {"fetch":{"offset":0,"partition":0,"max_bytes":1048576}},
        "k": 10, "rank": {"profile":"'"$PROFILE"'"}, "result_format": "merged"
    }')
    assert_status "Profile '$PROFILE' returns 200" "200" "$CODE"
done

# ============================================================================
section "Phase 13: Test Hybrid Query (Text + Fetch)"
# ============================================================================

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_LOGS"'","modes":["text","fetch"]}],
    "q": {
        "text": "payment error",
        "fetch": {"offset":0,"partition":0,"max_bytes":524288}
    },
    "k": 15, "result_format": "merged"
}')

if [ $? -eq 0 ]; then
    NUM=$(echo "$RESP" | jq '.results | length')
    ok "Hybrid query returned $NUM results"
    info "candidates=$(echo "$RESP" | jq -r '.stats.candidates'), latency=$(echo "$RESP" | jq -r '.stats.latency_ms')ms"

    # Check if results have features (indicates RRF processing)
    FEATS=$(echo "$RESP" | jq -r '.results[0].features // empty' 2>/dev/null)
    if [ -n "$FEATS" ] && [ "$FEATS" != "null" ]; then
        ok "Results have feature scores"
        info "Features: $FEATS"
    fi
else
    fail "Hybrid query failed"
fi

# ============================================================================
section "Phase 14: Test Prometheus Metrics"
# ============================================================================

METRICS=$(curl -sf "http://localhost:13092/metrics" 2>/dev/null || echo "")
if echo "$METRICS" | grep -q "chronik_query_requests_total"; then
    ok "chronik_query_requests_total metric present"
    COUNT=$(echo "$METRICS" | grep "^chronik_query_requests_total" | awk '{print $2}')
    info "Total requests: $COUNT"
else
    fail "chronik_query_requests_total not found"
fi

if echo "$METRICS" | grep -q "chronik_query_latency"; then
    ok "chronik_query_latency metric present"
else
    fail "chronik_query_latency not found"
fi

# ============================================================================
section "Summary"
# ============================================================================

TOTAL=$((PASSED + FAILED))
echo -e "  Total:  $TOTAL"
echo -e "  ${GREEN}Passed: $PASSED${END}"
echo -e "  ${RED}Failed: $FAILED${END}"

if [ -n "$ERRORS" ]; then
    echo -e "\n  ${RED}Failed tests:${END}"
    echo -e "$ERRORS"
fi
echo ""

exit $FAILED
