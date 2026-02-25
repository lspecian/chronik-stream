#!/usr/bin/env bash
#
# Non-interactive test for the RAG chatbot demo.
# Runs the full pipeline with scripted questions instead of interactive input.
#
set -euo pipefail

DATA_DIR="/tmp/chronik-rag-test"
SERVER_BIN="./target/release/chronik-server"
LOG_FILE="/tmp/chronik-rag-test.log"
API="http://localhost:6092"
TOPIC="knowledge-base"
SERVER_PID=""
PASS=0
FAIL=0

GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
CYAN='\033[96m'
DIM='\033[2m'
BOLD='\033[1m'
END='\033[0m'

cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$DATA_DIR"
}
trap cleanup EXIT

assert_gte() {
    local desc="$1" actual="$2" expected="$3"
    if [ "$actual" -ge "$expected" ]; then
        echo -e "  ${GREEN}PASS${END} $desc (got $actual, expected >= $expected)"
        PASS=$((PASS + 1))
    else
        echo -e "  ${RED}FAIL${END} $desc (got $actual, expected >= $expected)"
        FAIL=$((FAIL + 1))
    fi
}

assert_contains() {
    local desc="$1" haystack="$2" needle="$3"
    if echo "$haystack" | grep -qi "$needle" 2>/dev/null; then
        echo -e "  ${GREEN}PASS${END} $desc"
        PASS=$((PASS + 1))
    else
        echo -e "  ${RED}FAIL${END} $desc (expected to contain '$needle')"
        FAIL=$((FAIL + 1))
    fi
}

echo -e "${BOLD}RAG Chatbot Test Suite${END}"
echo -e "${DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"

# ── Start Server ─────────────────────────────────────────────────
echo -e "\n${BLUE}Phase 1: Starting server${END}"
rm -rf "$DATA_DIR" && mkdir -p "$DATA_DIR"

CHRONIK_DEFAULT_SEARCHABLE=true \
CHRONIK_DATA_DIR="$DATA_DIR" \
CHRONIK_ADVERTISED_ADDR=localhost \
RUST_LOG=warn \
    "$SERVER_BIN" start > "$LOG_FILE" 2>&1 &
SERVER_PID=$!

for i in $(seq 1 30); do
    if curl -sf "$API/health" > /dev/null 2>&1; then
        echo -e "  ${GREEN}Server ready${END}"
        break
    fi
    sleep 0.5
done

# ── Load Knowledge Base ──────────────────────────────────────────
echo -e "\n${BLUE}Phase 2: Loading knowledge base${END}"

python3 -c "
import json, time
from kafka import KafkaProducer

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
    api_version=(0, 10, 0)
)

articles = [
    {'id': 'auth-001', 'category': 'authentication', 'title': 'API Authentication',
     'content': 'All API requests require authentication via Bearer token in the Authorization header. API keys can be created and rotated from the dashboard under Settings > API Keys. For service-to-service auth, use OAuth2 client credentials flow.'},
    {'id': 'compute-001', 'category': 'compute', 'title': 'Instance Types',
     'content': 'Acme Cloud offers 4 instance families: General (g-series) for balanced workloads, Compute (c-series) for CPU-intensive work, Memory (m-series) for in-memory databases with up to 512GB RAM, and GPU (p-series) for ML training with NVIDIA A100 or H100.'},
    {'id': 'ts-001', 'category': 'troubleshooting', 'title': 'Connection Timeout Errors',
     'content': 'If you get connection timeout errors, check: 1) Security group allows inbound traffic on the port, 2) Instance is in Running state, 3) VPC has an internet gateway attached, 4) Route table has route to 0.0.0.0/0.'},
    {'id': 'ts-002', 'category': 'troubleshooting', 'title': '502 Bad Gateway Errors',
     'content': 'A 502 error means the load balancer cannot reach your backend. Check: 1) Backend instances are healthy, 2) Application listening on correct port, 3) Health check path returns 200 OK, 4) Security group allows traffic from load balancer.'},
    {'id': 'bill-001', 'category': 'billing', 'title': 'Pricing Model',
     'content': 'Acme Cloud uses pay-as-you-go pricing with per-second billing for compute. Reserved instances offer up to 60% savings with 1 or 3 year commitments. Free tier includes 750 hours of g-small compute per month for 12 months.'},
    {'id': 'sec-001', 'category': 'security', 'title': 'Encryption',
     'content': 'All data encrypted at rest using AES-256. In transit, TLS 1.3 is enforced on all API endpoints. Key Management Service lets you create, rotate, and audit encryption keys. FIPS 140-2 Level 3 validated HSMs available on Enterprise plan.'},
    {'id': 'sla-001', 'category': 'support', 'title': 'SLA and Uptime',
     'content': 'Acme Cloud SLA: 99.99% uptime for multi-AZ deployments, 99.95% for single-AZ. SLA credits: 10% credit for 99.9%-99.99%, 25% for 99.0%-99.9%, 100% for below 99.0%.'},
    {'id': 'k8s-001', 'category': 'kubernetes', 'title': 'Managed Kubernetes',
     'content': 'Acme Kubernetes Service provides managed Kubernetes clusters. Control plane is free. Node pools support auto-scaling with mixed instance types. Built-in ingress controller with automatic SSL.'},
    {'id': 'rate-001', 'category': 'api', 'title': 'Rate Limits',
     'content': 'API rate limits by plan: Free 100 requests/minute, Pro 1000 requests/minute, Enterprise 10000 requests/minute. When exceeded, returns 429 Too Many Requests. Use exponential backoff starting at 1 second.'},
    {'id': 'mig-001', 'category': 'migration', 'title': 'Migration from AWS',
     'content': 'Migrate from AWS: Use Migration Assessment Tool to analyze resources. Export data from S3 to Acme Object Storage using bulk transfer service. Database migration with continuous replication using DMS-compatible tool.'},
]

success = 0
for article in articles:
    try:
        producer.send('knowledge-base', value=article).get(timeout=10)
        success += 1
    except Exception as e:
        print(f'Failed: {e}')

producer.flush()
producer.close()
print(f'  Produced {success}/{len(articles)} articles')
"

# ── Wait for Indexing ─────────────────────────────────────────────
echo -e "\n${BLUE}Phase 3: Waiting for text index${END}"

READY=false
for i in $(seq 1 90); do
    PROBE=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
        "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
        "q": {"text":"authentication"},
        "k": 1, "result_format": "merged"
    }' 2>/dev/null || echo '{}')
    COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
    if [ "$COUNT" -gt 0 ]; then
        READY=true
        echo -e "  ${GREEN}Index ready${END} (${i}s)"
        break
    fi
    printf "  Indexing... %ds\r" "$i"
    sleep 1
done

if [ "$READY" != "true" ]; then
    echo -e "  ${RED}Index not ready after 90s${END}"
    exit 1
fi

# ── Run Test Queries ──────────────────────────────────────────────
echo -e "\n${BLUE}Phase 4: Test Queries${END}"

# Test 1: Authentication query
echo -e "\n${CYAN}Test 1: Search for authentication docs${END}"
R=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"how to authenticate API requests"},
    "k": 3, "rank": {"profile": "relevance"}, "result_format": "merged"
}')
COUNT=$(echo "$R" | jq '.results | length')
assert_gte "returns results" "$COUNT" 1
assert_contains "has query_id" "$(echo "$R" | jq -r '.query_id')" "."
assert_contains "has stats" "$(echo "$R" | jq -r '.stats.candidates')" "[0-9]"

# Test 2: Troubleshooting query
echo -e "\n${CYAN}Test 2: Search for 502 error fix${END}"
R=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"502 bad gateway error fix"},
    "k": 3, "rank": {"profile": "relevance"}, "result_format": "merged"
}')
COUNT=$(echo "$R" | jq '.results | length')
assert_gte "returns results" "$COUNT" 1

# Verify search found relevant results (top score should be high for an exact match)
TOP_SCORE=$(echo "$R" | jq -r '.results[0].final_score // 0')
echo -e "  ${DIM}Top result score: $TOP_SCORE${END}"
assert_gte "top result has positive score" "$(echo "$TOP_SCORE" | cut -d. -f1)" 1

# Test 3: Pricing query
echo -e "\n${CYAN}Test 3: Search for pricing information${END}"
R=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"pricing plan cost reserved instances"},
    "k": 3, "result_format": "merged"
}')
COUNT=$(echo "$R" | jq '.results | length')
assert_gte "returns results" "$COUNT" 1

# Test 4: Multi-keyword query
echo -e "\n${CYAN}Test 4: Search for encryption and security${END}"
R=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"encryption AES TLS security"},
    "k": 3, "rank": {"profile": "relevance"}, "result_format": "merged"
}')
COUNT=$(echo "$R" | jq '.results | length')
assert_gte "returns results" "$COUNT" 1

# Test 5: Ranking includes features and explanation
echo -e "\n${CYAN}Test 5: Results include ranking features${END}"
HAS_FEATURES=$(echo "$R" | jq '.results[0].features | length > 0')
HAS_EXPLANATION=$(echo "$R" | jq -r '.results[0].explanation // ""' | wc -c)
assert_contains "has features" "$HAS_FEATURES" "true"
assert_gte "has explanation" "$HAS_EXPLANATION" 10

# Test 6: Relevance profile changes scores
echo -e "\n${CYAN}Test 6: Relevance vs freshness profile${END}"
R_REL=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"kubernetes cluster"},
    "k": 3, "rank": {"profile": "relevance"}, "result_format": "merged"
}')
R_FRESH=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"kubernetes cluster"},
    "k": 3, "rank": {"profile": "freshness"}, "result_format": "merged"
}')
SCORE_REL=$(echo "$R_REL" | jq -r '.results[0].final_score // 0')
SCORE_FRESH=$(echo "$R_FRESH" | jq -r '.results[0].final_score // 0')
echo -e "  ${DIM}Relevance score: $SCORE_REL, Freshness score: $SCORE_FRESH${END}"
assert_gte "relevance profile returns results" "$(echo "$R_REL" | jq '.results | length')" 1
assert_gte "freshness profile returns results" "$(echo "$R_FRESH" | jq '.results | length')" 1

# Test 7: Grouped result format
echo -e "\n${CYAN}Test 7: Grouped result format${END}"
R=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"rate limit API"},
    "k": 3, "result_format": "grouped"
}')
HAS_GROUPED=$(echo "$R" | jq 'has("grouped_results")')
assert_contains "has grouped_results" "$HAS_GROUPED" "true"

# ── Summary ───────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
TOTAL=$((PASS + FAIL))
echo -e "  ${BOLD}Results:${END} ${GREEN}$PASS passed${END}, ${RED}$FAIL failed${END} / $TOTAL total"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
