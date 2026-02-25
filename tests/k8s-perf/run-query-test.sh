#!/usr/bin/env bash
#
# Phase 9 Query Scale Test
# ========================
# Tests the /_query endpoint under load with 100K indexed documents.
#
# Phases:
#   1. Deploy Chronik standalone with text indexing enabled
#   2. Load 100K documents via Kafka
#   3. Wait for Tantivy indexing
#   4. Run k6 query load test (10→500 VUs, ~3.5 min)
#   5. Collect results
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NAMESPACE="chronik-perf"
RESULTS_DIR="$SCRIPT_DIR/results/query-$(date +%Y%m%d-%H%M%S)"
TOPIC="query-bench"

GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
CYAN='\033[96m'
DIM='\033[2m'
BOLD='\033[1m'
END='\033[0m'

echo -e "${BOLD}Phase 9 — Query Orchestrator Scale Test${END}"
echo -e "${DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "  Results: $RESULTS_DIR"
mkdir -p "$RESULTS_DIR"

# ── Phase 1: Deploy Chronik ──────────────────────────────────────
echo -e "\n${BLUE}Phase 1: Deploy Chronik with text indexing${END}"

kubectl apply -f "$SCRIPT_DIR/00-namespace.yaml"
kubectl apply -f "$SCRIPT_DIR/60-chronik-query-standalone.yaml"

echo -e "  ${DIM}Waiting for Chronik pod...${END}"
for i in $(seq 1 90); do
    PHASE=$(kubectl get pod chronik-query-bench -n "$NAMESPACE" -o jsonpath='{.status.phase}' 2>/dev/null || echo "")
    READY=$(kubectl get pod chronik-query-bench -n "$NAMESPACE" -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null || echo "")
    if [ "$PHASE" = "Running" ] && [ "$READY" = "true" ]; then
        echo -e "  ${GREEN}Chronik ready${END} (~$((i*2))s)"
        break
    fi
    if [ "$i" -eq 90 ]; then
        echo -e "  ${RED}Chronik not ready after 180s${END}"
        kubectl describe pod chronik-query-bench -n "$NAMESPACE" | tail -20
        exit 1
    fi
    sleep 2
done

# Apply the service
kubectl apply -f "$SCRIPT_DIR/61-chronik-query-service.yaml"
sleep 2

# Verify Unified API is accessible
echo -e "  ${DIM}Checking Unified API...${END}"
kubectl exec chronik-query-bench -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/health 2>/dev/null && \
    echo -e "  ${GREEN}Unified API healthy${END}" || \
    echo -e "  ${RED}Unified API not responding${END}"

# ── Phase 2: Load Data ──────────────────────────────────────────
echo -e "\n${BLUE}Phase 2: Load 100K documents${END}"

# Delete old job if exists
kubectl delete job query-data-loader -n "$NAMESPACE" 2>/dev/null || true
sleep 2

kubectl apply -f "$SCRIPT_DIR/62-data-loader-job.yaml"

echo -e "  ${DIM}Loading documents (this takes ~2-3 minutes)...${END}"
for i in $(seq 1 300); do
    STATUS=$(kubectl get job query-data-loader -n "$NAMESPACE" -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
    FAILED=$(kubectl get job query-data-loader -n "$NAMESPACE" -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")
    if [ "$STATUS" = "True" ]; then
        echo -e "  ${GREEN}Data loaded${END} (~$((i*2))s)"
        break
    fi
    if [ "$FAILED" = "True" ]; then
        echo -e "  ${RED}Data loader failed${END}"
        kubectl logs -n "$NAMESPACE" job/query-data-loader --tail=30
        exit 1
    fi
    if [ $((i % 15)) -eq 0 ]; then
        # Show progress from loader logs
        LAST_LINE=$(kubectl logs -n "$NAMESPACE" job/query-data-loader --tail=1 2>/dev/null || echo "loading...")
        printf "  %s\r" "$LAST_LINE"
    fi
    sleep 2
done

# Save loader logs
kubectl logs -n "$NAMESPACE" job/query-data-loader > "$RESULTS_DIR/data-loader.log" 2>&1

# ── Phase 3: Wait for Indexing ───────────────────────────────────
echo -e "\n${BLUE}Phase 3: Wait for Tantivy indexing${END}"

echo -e "  ${DIM}Waiting for text index to cover documents...${END}"
INDEXED=false
for i in $(seq 1 120); do
    PROBE=$(kubectl exec chronik-query-bench -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"$TOPIC\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"encryption\"},\"k\":1,\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
    if [ "$COUNT" -gt 0 ]; then
        INDEXED=true
        echo -e "  ${GREEN}Index ready${END} (${i}s)"
        break
    fi
    printf "  Indexing... %ds\r" "$i"
    sleep 1
done

if [ "$INDEXED" != "true" ]; then
    echo -e "  ${RED}Index not ready after 120s${END}"
    kubectl logs chronik-query-bench -n "$NAMESPACE" --tail=30
    exit 1
fi

# Check approximate document count
DOC_PROBE=$(kubectl exec chronik-query-bench -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_query \
    -H 'Content-Type: application/json' \
    -d "{\"sources\":[{\"topic\":\"$TOPIC\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"the\"},\"k\":100,\"result_format\":\"merged\"}" \
    2>/dev/null || echo '{}')
DOC_COUNT=$(echo "$DOC_PROBE" | jq '.stats.candidates // 0' 2>/dev/null || echo "0")
echo -e "  ${DIM}Indexed candidates visible: ~${DOC_COUNT}${END}"

# Wait a bit more for full indexing
echo -e "  ${DIM}Waiting 30s for full index build...${END}"
sleep 30

DOC_PROBE=$(kubectl exec chronik-query-bench -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_query \
    -H 'Content-Type: application/json' \
    -d "{\"sources\":[{\"topic\":\"$TOPIC\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"the\"},\"k\":100,\"result_format\":\"merged\"}" \
    2>/dev/null || echo '{}')
DOC_COUNT=$(echo "$DOC_PROBE" | jq '.stats.candidates // 0' 2>/dev/null || echo "0")
echo -e "  ${GREEN}Final indexed candidates: ~${DOC_COUNT}${END}"

# Quick sanity queries
echo -e "\n  ${CYAN}Sanity checks:${END}"
for q in "encryption AES" "Kubernetes pod" "502 timeout error" "pricing cost savings"; do
    R=$(kubectl exec chronik-query-bench -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"$TOPIC\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"$q\"},\"k\":5,\"rank\":{\"profile\":\"relevance\"},\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    RC=$(echo "$R" | jq '.results | length' 2>/dev/null || echo "0")
    LATENCY=$(echo "$R" | jq '.stats.latency_ms // 0' 2>/dev/null || echo "?")
    echo -e "    \"$q\" → ${RC} results (${LATENCY}ms)"
done

# ── Phase 4: Run k6 Query Load Test ─────────────────────────────
echo -e "\n${BLUE}Phase 4: Run k6 query load test (~3.5 min, 4 pods, up to 500 VUs)${END}"

# Clean old test runs
kubectl delete testrun k6-chronik-query -n "$NAMESPACE" 2>/dev/null || true
sleep 3

kubectl apply -f "$SCRIPT_DIR/63-k6-query-configmap.yaml"
kubectl apply -f "$SCRIPT_DIR/64-k6-query-test.yaml"

echo -e "  ${DIM}Waiting for k6 test to complete...${END}"
for i in $(seq 1 120); do
    STAGE=$(kubectl get testrun k6-chronik-query -n "$NAMESPACE" -o jsonpath='{.status.stage}' 2>/dev/null || echo "")
    if [ "$STAGE" = "finished" ] || [ "$STAGE" = "error" ]; then
        echo -e "  ${GREEN}k6 test ${STAGE}${END} (~$((i*5))s)"
        break
    fi
    if [ $((i % 12)) -eq 0 ]; then
        echo -e "  ${DIM}Still running... ($((i*5))s)${END}"
    fi
    sleep 5
done

# ── Phase 5: Collect Results ─────────────────────────────────────
echo -e "\n${BLUE}Phase 5: Collect results${END}"

# k6 output
kubectl logs -n "$NAMESPACE" -l k6_cr=k6-chronik-query --tail=-1 \
    > "$RESULTS_DIR/k6-query-output.log" 2>&1 || true

# Chronik server logs
kubectl logs chronik-query-bench -n "$NAMESPACE" --tail=500 \
    > "$RESULTS_DIR/chronik-server.log" 2>&1 || true

# ── Summary ──────────────────────────────────────────────────────
echo -e "\n${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "${BOLD}  Query Scale Test Results${END}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo ""

echo -e "  ${CYAN}k6 Summary:${END}"
grep -E "http_req_duration|query_errors|query_latency|queries_completed|iterations|vus" \
    "$RESULTS_DIR/k6-query-output.log" 2>/dev/null | tail -20 || echo "  (check $RESULTS_DIR/k6-query-output.log)"

echo ""
echo -e "  Results saved to: ${DIM}$RESULTS_DIR/${END}"
echo -e "  Files:"
ls -lh "$RESULTS_DIR/" 2>/dev/null | while IFS= read -r line; do
    echo -e "    ${DIM}$line${END}"
done
echo ""
