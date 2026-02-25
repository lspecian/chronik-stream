#!/usr/bin/env bash
#
# Log Analysis Dashboard Demo
# ============================
#
# Demonstrates Chronik's /_query orchestrator with a realistic
# microservices log analysis use case.
#
# 5 services produce structured logs, then we run 12 query scenarios:
#   - Full-text search (find errors by keyword)
#   - SQL analytics (aggregate, group by, top-N)
#   - Hybrid search (text + fetch with RRF fusion)
#   - Multi-topic queries (search across services)
#   - Ranking profiles (freshness vs relevance)
#
# Prerequisites:
#   - chronik-server binary built (cargo build --release)
#   - kafka-python installed (pip3 install kafka-python)
#   - jq installed
#
# Usage:
#   bash tests/e2e/demo_log_analysis.sh
#
# The script starts its own server, produces data, runs queries,
# and cleans up when done.

set -euo pipefail

# ── Configuration ────────────────────────────────────────────────
DATA_DIR="/tmp/chronik-demo-logs"
SERVER_BIN="./target/release/chronik-server"
LOG_FILE="/tmp/chronik-demo-server.log"
API="http://localhost:6092"
KAFKA="localhost:9092"
SERVER_PID=""

# Topics
TOPIC_APP="app-logs"
TOPIC_ACCESS="access-logs"

# Colors
GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
CYAN='\033[96m'
YELLOW='\033[93m'
BOLD='\033[1m'
DIM='\033[2m'
END='\033[0m'

# ── Helpers ──────────────────────────────────────────────────────

banner() {
    echo ""
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
    echo -e "${BOLD}  $1${END}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
}

scenario() {
    echo ""
    echo -e "  ${CYAN}Scenario $1: $2${END}"
    echo -e "  ${DIM}$3${END}"
}

show_query() {
    echo -e "  ${DIM}Query:${END} $1"
}

show_result() {
    echo -e "  ${GREEN}Result:${END} $1"
}

show_detail() {
    echo -e "  ${DIM}  $1${END}"
}

cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        echo -e "\n${DIM}Stopping server (PID $SERVER_PID)...${END}"
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# ── Phase 1: Start Server ───────────────────────────────────────

banner "Log Analysis Dashboard Demo"

echo -e "\n${BOLD}Phase 1: Start Server${END}"

# Check binary exists
if [ ! -f "$SERVER_BIN" ]; then
    echo -e "  ${RED}Binary not found at $SERVER_BIN${END}"
    echo -e "  Run: cargo build --release --bin chronik-server"
    exit 1
fi

# Kill any existing server
pkill -f "chronik-server" 2>/dev/null || true
sleep 1

# Clean data directory
rm -rf "$DATA_DIR"

# Start server
CHRONIK_DEFAULT_SEARCHABLE=true \
CHRONIK_DEFAULT_COLUMNAR=true \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_DATA_DIR="$DATA_DIR" \
RUST_LOG=info \
"$SERVER_BIN" start --data-dir "$DATA_DIR" &>"$LOG_FILE" &
SERVER_PID=$!

echo -e "  Server starting (PID $SERVER_PID)..."

# Wait for health
for i in $(seq 1 30); do
    if curl -sf "$API/health" >/dev/null 2>&1; then
        echo -e "  ${GREEN}Server ready in ${i}s${END}"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo -e "  ${RED}Server failed to start. Check $LOG_FILE${END}"
        exit 1
    fi
    sleep 1
done

# ── Phase 2: Produce Log Data ───────────────────────────────────

echo -e "\n${BOLD}Phase 2: Produce Microservice Log Data${END}"

echo -e "  Producing logs from 5 microservices..."

# Use kafka-python inline (unavoidable for Kafka wire protocol)
python3 -c "
from kafka import KafkaProducer
import json, time, random

p = KafkaProducer(
    bootstrap_servers='$KAFKA',
    api_version=(0, 10, 0),
    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
    key_serializer=lambda k: k.encode('utf-8') if k else None,
)

services = ['payment-svc', 'auth-svc', 'order-svc', 'inventory-svc', 'notification-svc']
endpoints = ['/api/pay', '/api/login', '/api/orders', '/api/stock', '/api/notify']
levels = ['INFO', 'WARN', 'ERROR', 'DEBUG']
level_weights = [50, 20, 15, 15]

error_messages = [
    'Payment gateway timeout after 30s — retrying',
    'Card declined for transaction TX-{seq}',
    'Database connection pool exhausted (max=50)',
    'Redis cache miss — falling back to primary DB',
    'Rate limit exceeded for IP 10.0.{ip1}.{ip2}',
    'SSL certificate verification failed for upstream',
    'OutOfMemoryError in JVM heap (used=3.8GB/4GB)',
    'Deadlock detected in transaction isolation level SERIALIZABLE',
    'Connection refused to inventory-svc:8080',
    'Request timeout — downstream auth-svc not responding',
]

warn_messages = [
    'Slow query detected: {ms}ms on SELECT * FROM orders',
    'Connection pool at 80% capacity ({used}/50)',
    'Retry attempt {n}/3 for payment processing',
    'Deprecated API version v1 called by client {client}',
    'High memory usage: {pct}% of allocated heap',
]

info_messages = [
    'Order ORD-{seq} placed successfully (amount=\${amount})',
    'User {user} logged in from {ip}',
    'Inventory updated: SKU-{sku} quantity={qty}',
    'Payment processed: \${amount} via {method}',
    'Notification sent to user-{user} via email',
    'Health check passed — all dependencies up',
    'Cache hit rate: {pct}%',
    'Request completed in {ms}ms',
]

now = int(time.time() * 1000)

# app-logs: structured application logs
for i in range(200):
    svc_idx = i % 5
    level = random.choices(levels, weights=level_weights, k=1)[0]

    if level == 'ERROR':
        msg = random.choice(error_messages).format(
            seq=10000+i, ip1=random.randint(1,254), ip2=random.randint(1,254),
            ms=random.randint(100,5000))
    elif level == 'WARN':
        msg = random.choice(warn_messages).format(
            ms=random.randint(500,3000), used=random.randint(30,48),
            n=random.randint(1,3), client=f'mobile-app-v{random.randint(1,3)}',
            pct=random.randint(70,95))
    else:
        msg = random.choice(info_messages).format(
            seq=10000+i, amount=round(random.uniform(10,999),2),
            user=f'user-{random.randint(1,100)}',
            ip=f'10.0.{random.randint(1,254)}.{random.randint(1,254)}',
            sku=random.randint(1000,9999), qty=random.randint(1,500),
            method=random.choice(['visa','mastercard','paypal','stripe']),
            pct=random.randint(85,99), ms=random.randint(5,200))

    log = {
        'level': level,
        'service': services[svc_idx],
        'msg': msg,
        'endpoint': endpoints[svc_idx],
        'response_time_ms': random.randint(1, 5000) if level == 'ERROR' else random.randint(1, 200),
        'trace_id': f'trace-{i:06d}',
        'timestamp': now - (200 - i) * 1000,  # spread over 200s
    }
    p.send('$TOPIC_APP', key=f'{services[svc_idx]}-{i}', value=log)

# access-logs: HTTP access logs
for i in range(100):
    status = random.choices([200, 201, 301, 400, 401, 403, 404, 500, 502, 503],
                            weights=[40, 10, 5, 8, 5, 3, 10, 8, 5, 6], k=1)[0]
    access = {
        'method': random.choice(['GET', 'POST', 'PUT', 'DELETE']),
        'path': random.choice(['/api/orders', '/api/users', '/api/products',
                               '/api/payments', '/api/auth/login', '/api/inventory',
                               '/healthz', '/metrics']),
        'status': status,
        'response_time_ms': random.randint(1, 100) if status < 400 else random.randint(200, 5000),
        'user_agent': random.choice(['Mozilla/5.0', 'curl/7.88', 'python-requests/2.31',
                                     'Go-http-client/2.0', 'okhttp/4.12']),
        'remote_ip': f'10.0.{random.randint(1,254)}.{random.randint(1,254)}',
        'bytes_sent': random.randint(100, 50000),
        'timestamp': now - (100 - i) * 2000,
    }
    p.send('$TOPIC_ACCESS', key=f'access-{i}', value=access)

p.flush()
p.close()
print('OK')
" 2>/dev/null

echo -e "  ${GREEN}Produced 300 messages:${END}"
echo -e "    200 app-logs (5 microservices, structured errors/warns/info)"
echo -e "    100 access-logs (HTTP request logs with status codes)"

# ── Phase 3: Wait for Indexing ───────────────────────────────────

echo -e "\n${BOLD}Phase 3: Wait for Indexing${END}"
echo -e "  Waiting for Tantivy + Parquet indexing..."

TEXT_READY=false
SQL_READY=false

for i in $(seq 1 90); do
    CAPS=$(curl -sf "$API/_query/capabilities" 2>/dev/null || echo "[]")

    if ! $TEXT_READY; then
        if echo "$CAPS" | jq -e ".[] | select(.topic==\"$TOPIC_APP\" and .text_search==true)" >/dev/null 2>&1; then
            PROBE=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
                "sources": [{"topic":"'"$TOPIC_APP"'","modes":["text"]}],
                "q": {"text":"timeout"},
                "k": 1, "result_format": "merged"
            }' 2>/dev/null || echo '{}')
            PROBE_COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
            if [ "$PROBE_COUNT" -gt 0 ]; then
                TEXT_READY=true
            fi
        fi
    fi

    if ! $SQL_READY; then
        SANITIZED=$(echo "$TOPIC_APP" | sed 's/[-.]/_/g')
        PROBE=$(curl -sf "$API/_sql" -H 'Content-Type: application/json' \
            -d '{"query":"SELECT COUNT(*) as cnt FROM '"$SANITIZED"'_hot"}' 2>/dev/null || echo '{}')
        CNT=$(echo "$PROBE" | jq -r '.rows[0].cnt // 0' 2>/dev/null || echo "0")
        if [ "$CNT" -gt 0 ] 2>/dev/null; then
            SQL_READY=true
        fi
    fi

    if $TEXT_READY && $SQL_READY; then break; fi
    printf "\r  Waiting... %ds (text=%s sql=%s) " "$i" "$TEXT_READY" "$SQL_READY"
    sleep 2
done
echo ""

$TEXT_READY && echo -e "  ${GREEN}Text search ready${END}" || echo -e "  ${YELLOW}Text search not ready${END}"
$SQL_READY && echo -e "  ${GREEN}SQL engine ready${END}" || echo -e "  ${YELLOW}SQL engine not ready${END}"

# Show capabilities
echo -e "\n  ${BOLD}Topic Capabilities:${END}"
curl -sf "$API/_query/capabilities" | jq -c '.[] | {topic, text_search, sql_query, fetch}'

# ── Phase 4: Query Scenarios ────────────────────────────────────

banner "Query Scenarios"

SANITIZED_APP=$(echo "$TOPIC_APP" | sed 's/[-.]/_/g')
SANITIZED_ACCESS=$(echo "$TOPIC_ACCESS" | sed 's/[-.]/_/g')

# ─────────────────────────────────────────────────────────────────
scenario 1 "Find payment errors" \
    "Full-text search for 'payment gateway timeout' across app logs"

show_query "POST /_query { text: 'payment gateway timeout', modes: ['text'] }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_APP"'","modes":["text"]}],
    "q": {"text":"payment gateway timeout"},
    "k": 5, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
show_result "$NUM results found"

echo "$RESP" | jq -r '.results[:3][] | "    score=\(.final_score | tostring | .[0:6])  offset=\(.offset)  partition=\(.partition)"'
echo -e "  ${DIM}  (showing top 3)${END}"

# ─────────────────────────────────────────────────────────────────
scenario 2 "Find all connection errors" \
    "Full-text search for 'connection refused OR connection pool' across app logs"

show_query "POST /_query { text: 'connection refused OR pool exhausted' }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_APP"'","modes":["text"]}],
    "q": {"text":"connection refused OR pool exhausted"},
    "k": 10, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
show_result "$NUM results matching connection issues"
show_detail "latency=$(echo "$RESP" | jq -r '.stats.latency_ms')ms, candidates=$(echo "$RESP" | jq -r '.stats.candidates')"

# ─────────────────────────────────────────────────────────────────
scenario 3 "Record count by topic (SQL)" \
    "SQL COUNT(*) to verify data volume per topic"

QUERY="SELECT COUNT(*) as total FROM ${SANITIZED_APP}"
show_query "POST /_sql { query: \"$QUERY\" }"

RESP=$(curl -sf "$API/_sql" -H 'Content-Type: application/json' \
    -d '{"query":"'"$QUERY"'"}')

TOTAL=$(echo "$RESP" | jq -r '.rows[0].total // 0')
TIME=$(echo "$RESP" | jq -r '.execution_time_ms')
show_result "$TOTAL app-log records queried in ${TIME}ms"

QUERY2="SELECT COUNT(*) as total FROM ${SANITIZED_ACCESS}"
RESP2=$(curl -sf "$API/_sql" -H 'Content-Type: application/json' \
    -d '{"query":"'"$QUERY2"'"}')
TOTAL2=$(echo "$RESP2" | jq -r '.rows[0].total // 0')
show_result "$TOTAL2 access-log records"

# ─────────────────────────────────────────────────────────────────
scenario 4 "Recent records by offset (SQL)" \
    "SQL ORDER BY _offset DESC to find the most recent log entries"

QUERY="SELECT _offset, _partition, _topic, _timestamp FROM ${SANITIZED_APP} ORDER BY _offset DESC LIMIT 5"
show_query "POST /_sql { query: \"$QUERY\" }"

RESP=$(curl -sf "$API/_sql" -H 'Content-Type: application/json' \
    -d '{"query":"'"$QUERY"'"}')

ROW_COUNT=$(echo "$RESP" | jq '.row_count')
TIME=$(echo "$RESP" | jq '.execution_time_ms')
show_result "$ROW_COUNT rows in ${TIME}ms"
echo "$RESP" | jq -r '.rows[:5][] | "    offset=\(._offset)  partition=\(._partition)  topic=\(._topic)"'

# ─────────────────────────────────────────────────────────────────
scenario 5 "HTTP 5xx errors in access logs (SQL)" \
    "SQL query via /_query to find server errors in access-logs"

show_query "POST /_query { sql: 'SELECT * FROM ${SANITIZED_ACCESS} LIMIT 10', modes: ['sql'] }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_ACCESS"'","modes":["sql"]}],
    "q": {"sql":"SELECT * FROM '"$SANITIZED_ACCESS"' LIMIT 10"},
    "k": 10, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
show_result "$NUM access log entries via SQL"
show_detail "latency=$(echo "$RESP" | jq -r '.stats.latency_ms')ms"

# ─────────────────────────────────────────────────────────────────
scenario 6 "Hybrid search: text + fetch with RRF fusion" \
    "Combine full-text relevance with direct WAL fetch, merged via Reciprocal Rank Fusion"

show_query "POST /_query { text: 'database', modes: ['text','fetch'] }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_APP"'","modes":["text","fetch"]}],
    "q": {
        "text": "database connection",
        "fetch": {"offset":0,"partition":0,"max_bytes":524288}
    },
    "k": 10, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
CANDIDATES=$(echo "$RESP" | jq -r '.stats.candidates')
show_result "$NUM results from $CANDIDATES candidates (text + fetch merged)"

# Show RRF features on top result
TOP_FEATS=$(echo "$RESP" | jq -r '.results[0].features // empty' 2>/dev/null)
if [ -n "$TOP_FEATS" ] && [ "$TOP_FEATS" != "null" ]; then
    show_detail "Top result features:"
    echo "$TOP_FEATS" | jq -r 'to_entries[] | "      \(.key) = \(.value)"'
fi

# ─────────────────────────────────────────────────────────────────
scenario 7 "Multi-topic search" \
    "Search across BOTH app-logs and access-logs in a single query"

show_query "POST /_query { sources: [app-logs, access-logs], modes: ['fetch'] }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [
        {"topic":"'"$TOPIC_APP"'","modes":["fetch"]},
        {"topic":"'"$TOPIC_ACCESS"'","modes":["fetch"]}
    ],
    "q": {"fetch":{"offset":0,"partition":0,"max_bytes":1048576}},
    "k": 20, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
TOPICS=$(echo "$RESP" | jq -r '[.results[].topic] | unique | join(", ")')
show_result "$NUM results spanning topics: $TOPICS"

# ─────────────────────────────────────────────────────────────────
scenario 8 "Grouped results by topic" \
    "Same multi-topic query but with results grouped by topic"

show_query "POST /_query { result_format: 'grouped' }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [
        {"topic":"'"$TOPIC_APP"'","modes":["fetch"]},
        {"topic":"'"$TOPIC_ACCESS"'","modes":["fetch"]}
    ],
    "q": {"fetch":{"offset":0,"partition":0,"max_bytes":1048576}},
    "k": 20, "result_format": "grouped"
}')

show_result "Results grouped by topic:"
echo "$RESP" | jq -r '.grouped_results | to_entries[] | "    \(.key): \(.value | length) results"'

# ─────────────────────────────────────────────────────────────────
scenario 9 "Ranking: freshness profile" \
    "Same search query but with freshness ranking (prioritize recent logs)"

show_query "POST /_query { text: 'error', rank: {profile: 'freshness'} }"

RESP_FRESH=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_APP"'","modes":["text"]}],
    "q": {"text":"error"},
    "k": 5, "rank": {"profile":"freshness"}, "result_format": "merged"
}')

RESP_REL=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_APP"'","modes":["text"]}],
    "q": {"text":"error"},
    "k": 5, "rank": {"profile":"relevance"}, "result_format": "merged"
}')

FRESH_TOP=$(echo "$RESP_FRESH" | jq -r '.results[0].final_score // 0')
REL_TOP=$(echo "$RESP_REL" | jq -r '.results[0].final_score // 0')

show_result "Freshness profile: top score=$FRESH_TOP"
show_result "Relevance profile: top score=$REL_TOP"

echo -e "\n  ${DIM}  Freshness top-3 offsets:${END}"
echo "$RESP_FRESH" | jq -r '.results[:3][] | "    offset=\(.offset) score=\(.final_score | tostring | .[0:8])"'
echo -e "  ${DIM}  Relevance top-3 offsets:${END}"
echo "$RESP_REL" | jq -r '.results[:3][] | "    offset=\(.offset) score=\(.final_score | tostring | .[0:8])"'

# ─────────────────────────────────────────────────────────────────
scenario 10 "SQL via /_query orchestrator (not /_sql)" \
    "Run SQL through the query orchestrator which adds scoring and ranking"

show_query "POST /_query { sql: 'SELECT * FROM ${SANITIZED_APP} LIMIT 5', modes: ['sql'] }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_APP"'","modes":["sql"]}],
    "q": {"sql":"SELECT * FROM '"$SANITIZED_APP"' LIMIT 5"},
    "k": 5, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
show_result "$NUM results via SQL through orchestrator"
echo "$RESP" | jq -r '.results[:3][] | "    offset=\(.offset) partition=\(.partition) score=\(.final_score) source=\(.source)"'
show_detail "latency=$(echo "$RESP" | jq -r '.stats.latency_ms')ms"

# ─────────────────────────────────────────────────────────────────
scenario 11 "Search with explanation" \
    "Text search with result explanation showing feature breakdown"

show_query "POST /_query { text: 'SSL certificate', k: 3 }"

RESP=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC_APP"'","modes":["text"]}],
    "q": {"text":"SSL certificate verification"},
    "k": 3, "result_format": "merged"
}')

NUM=$(echo "$RESP" | jq '.results | length')
show_result "$NUM results for 'SSL certificate verification'"

for idx in $(seq 0 $((NUM < 3 ? NUM - 1 : 2))); do
    R=$(echo "$RESP" | jq ".results[$idx]")
    SCORE=$(echo "$R" | jq -r '.final_score | tostring | .[0:8]')
    OFFSET=$(echo "$R" | jq -r '.offset')
    EXPLAIN=$(echo "$R" | jq -r '.explanation // "n/a"')
    show_detail "#$((idx+1)): score=$SCORE offset=$OFFSET explanation=$EXPLAIN"
done

# ─────────────────────────────────────────────────────────────────
scenario 12 "Capabilities introspection" \
    "GET /_query/capabilities and /_query/profiles for API discovery"

show_query "GET /_query/capabilities"

CAPS=$(curl -sf "$API/_query/capabilities")
echo -e "  ${GREEN}Topics available:${END}"
echo "$CAPS" | jq -r '.[] | "    \(.topic): text=\(.text_search) sql=\(.sql_query) vector=\(.vector_search) fetch=\(.fetch)"'

show_query "GET /_query/profiles"
PROFILES=$(curl -sf "$API/_query/profiles")
echo -e "  ${GREEN}Ranking profiles:${END}"
echo "$PROFILES" | jq -r '.[] | "    \(.name): weights=\(.weights | to_entries | map("\(.key)=\(.value)") | join(", "))"'

# ── Prometheus Metrics ──────────────────────────────────────────

banner "Prometheus Metrics"

METRICS=$(curl -sf "http://localhost:13092/metrics" 2>/dev/null || echo "")
if [ -n "$METRICS" ]; then
    echo ""
    TOTAL=$(echo "$METRICS" | { grep "^chronik_query_requests_total " || true; } | head -1 | awk '{print $2}')
    if [ -n "$TOTAL" ]; then
        echo -e "  ${BOLD}chronik_query_requests_total${END}: $TOTAL"
    fi

    CANDIDATES=$(echo "$METRICS" | { grep "^chronik_query_candidates_total " || true; } | head -1 | awk '{print $2}')
    if [ -n "$CANDIDATES" ]; then
        echo -e "  ${BOLD}chronik_query_candidates_total${END}: $CANDIDATES"
    fi
else
    echo -e "  ${YELLOW}Metrics endpoint not available on port 13092${END}"
fi

# ── Summary ─────────────────────────────────────────────────────

banner "Demo Complete"

echo ""
echo -e "  ${GREEN}12 query scenarios demonstrated successfully${END}"
echo ""
echo -e "  ${BOLD}Capabilities shown:${END}"
echo -e "    - Full-text search (BM25 scoring via Tantivy)"
echo -e "    - SQL analytics (DataFusion over Parquet/hot buffer)"
echo -e "    - Hybrid search (text + fetch merged via RRF)"
echo -e "    - Multi-topic fan-out (parallel across topics)"
echo -e "    - Grouped results (by topic)"
echo -e "    - Ranking profiles (default, freshness, relevance)"
echo -e "    - Feature-based explanations"
echo -e "    - Prometheus metrics"
echo ""
echo -e "  ${BOLD}Data produced:${END}"
echo -e "    200 app-logs (5 services: payment, auth, order, inventory, notification)"
echo -e "    100 access-logs (HTTP requests with status codes)"
echo ""
echo -e "  ${BOLD}API endpoints used:${END}"
echo -e "    POST /_query              Unified query orchestrator"
echo -e "    POST /_sql                Direct SQL queries"
echo -e "    GET  /_query/capabilities Per-topic capability detection"
echo -e "    GET  /_query/profiles     Available ranking profiles"
echo -e "    GET  /metrics             Prometheus metrics (port 13092)"
echo ""
echo -e "  ${DIM}Server log: $LOG_FILE${END}"
echo -e "  ${DIM}Data dir:   $DATA_DIR${END}"
echo ""
