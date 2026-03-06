#!/usr/bin/env bash
#
# DV-2b: Amazon Grocery Scale Validation (5.1M reviews)
# =====================================================
#
# Same pattern as DV-2a but at 5M scale. Reuses cluster + loader script.
#
# Usage:
#   ./run-amazon-grocery-test.sh                 # Full run
#   ./run-amazon-grocery-test.sh --skip-build    # Skip build
#   ./run-amazon-grocery-test.sh --skip-deploy   # Skip cluster deploy
#   ./run-amazon-grocery-test.sh --skip-load     # Skip data load
#   ./run-amazon-grocery-test.sh --quick         # Load 500K records only
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
NAMESPACE="chronik-perf"
CLUSTER_NAME="chronik-thunderbird"
IMAGE_TAG="dv2b-grocery"
TOPIC="amazon-grocery"
RESULTS_DIR="$SCRIPT_DIR/results/amazon-grocery-$(date +%Y%m%d-%H%M%S)"
NODES=("dell-1" "dell-2" "dell-3")
DATASET_DIR="/home/ubuntu/datasets/amazon"
DATA_FILE="Grocery_and_Gourmet_Food.jsonl"
DOWNLOAD_URL="https://huggingface.co/datasets/McAuley-Lab/Amazon-Reviews-2023/resolve/main/raw/review_categories/Grocery_and_Gourmet_Food.jsonl?download=true"

SKIP_BUILD=false
SKIP_DEPLOY=false
SKIP_LOAD=false
MAX_RECORDS=0

for arg in "$@"; do
    case "$arg" in
        --skip-build)  SKIP_BUILD=true ;;
        --skip-deploy) SKIP_DEPLOY=true ;;
        --skip-load)   SKIP_LOAD=true ;;
        --quick)       MAX_RECORDS=500000 ;;
    esac
done

GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
CYAN='\033[96m'
DIM='\033[2m'
BOLD='\033[1m'
END='\033[0m'

echo -e "${BOLD}DV-2b: Amazon Grocery Scale Validation (5.1M)${END}"
echo -e "${DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "  Dataset:  Amazon Reviews 2023 — Grocery & Gourmet Food (5.1M)"
echo -e "  Results:  $RESULTS_DIR"
if [ "$MAX_RECORDS" -gt 0 ]; then
    echo -e "  ${CYAN}Quick mode: ${MAX_RECORDS} records only${END}"
fi
mkdir -p "$RESULTS_DIR"

# ── Phase 0: Dataset ──────────────────────────────────────────────────

echo -e "\n${BLUE}Phase 0: Dataset${END}"
mkdir -p "$DATASET_DIR"
if [ -f "$DATASET_DIR/$DATA_FILE" ]; then
    FILE_SIZE=$(du -h "$DATASET_DIR/$DATA_FILE" | cut -f1)
    echo -e "  ${GREEN}Dataset found${END} ($DATA_FILE, ${FILE_SIZE})"
else
    echo -e "  ${DIM}Downloading Amazon Grocery dataset (~1.2GB)...${END}"
    wget -q --show-progress -O "$DATASET_DIR/$DATA_FILE" "$DOWNLOAD_URL"
    echo -e "  ${GREEN}Downloaded${END}"
fi

# ── Phase 0b: Build ───────────────────────────────────────────────────

if [ "$SKIP_BUILD" = false ]; then
    echo -e "\n${BLUE}Phase 0b: Build image${END}"
    cd "$PROJECT_DIR"
    cargo build --release --bin chronik-server 2>&1 | tail -3
    mkdir -p artifacts/linux/amd64
    cp target/release/chronik-server artifacts/linux/amd64/
    docker build -f Dockerfile.binary -t "chronik-server:${IMAGE_TAG}" --build-arg TARGETARCH=amd64 .
    for node in "${NODES[@]}"; do
        docker save "chronik-server:${IMAGE_TAG}" | ssh "$node" docker load 2>/dev/null || true
    done
    echo -e "  ${GREEN}Image built and distributed${END}"
else
    echo -e "\n${DIM}Phase 0b: Skipped (--skip-build)${END}"
fi

# ── Phase 1-2: Deploy + Wait ──────────────────────────────────────────

if [ "$SKIP_DEPLOY" = false ]; then
    echo -e "\n${BLUE}Phase 1: Deploy${END}"
    kubectl apply -f "$SCRIPT_DIR/00-namespace.yaml"
    kubectl apply -f "$SCRIPT_DIR/90-chronik-thunderbird-cluster.yaml"
    kubectl apply -f "$SCRIPT_DIR/91-chronik-thunderbird-service.yaml"
else
    echo -e "\n${DIM}Phase 1: Skipped${END}"
fi

echo -e "\n${BLUE}Phase 2: Wait for cluster ready${END}"
for i in $(seq 1 120); do
    READY_COUNT=$(kubectl get pods -n "$NAMESPACE" \
        -l "app.kubernetes.io/instance=${CLUSTER_NAME}" \
        --field-selector=status.phase=Running -o name 2>/dev/null | wc -l || echo 0)
    if [ "$READY_COUNT" -ge 3 ]; then
        echo -e "  ${GREEN}All 3 pods running${END}"
        break
    fi
    [ "$i" -eq 120 ] && { echo -e "  ${RED}Timeout${END}"; exit 1; }
    [ $((i % 10)) -eq 0 ] && echo -e "  ${DIM}$READY_COUNT/3 ($((i*3))s)...${END}"
    sleep 3
done
sleep 15

# ── Phase 3: Load Data ────────────────────────────────────────────────

if [ "$SKIP_LOAD" = false ]; then
    echo -e "\n${BLUE}Phase 3: Load Grocery reviews (5.1M)${END}"
    kubectl delete job amazon-grocery-loader -n "$NAMESPACE" 2>/dev/null || true
    sleep 3
    kubectl apply -f "$SCRIPT_DIR/105-amazon-loader-configmap.yaml"

    if [ "$MAX_RECORDS" -gt 0 ]; then
        echo -e "  ${CYAN}Quick mode: $MAX_RECORDS records${END}"
        cat "$SCRIPT_DIR/109-amazon-grocery-loader-job.yaml" | \
            sed "s/value: \"0\"/value: \"${MAX_RECORDS}\"/" | \
            kubectl apply -f -
    else
        kubectl apply -f "$SCRIPT_DIR/109-amazon-grocery-loader-job.yaml"
    fi

    LOAD_START=$(date +%s)
    for i in $(seq 1 2700); do
        STATUS=$(kubectl get job amazon-grocery-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
        FAILED=$(kubectl get job amazon-grocery-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")
        if [ "$STATUS" = "True" ]; then
            echo -e "  ${GREEN}Loaded${END} ($(($(date +%s) - LOAD_START))s)"
            break
        fi
        [ "$FAILED" = "True" ] && { echo -e "  ${RED}Failed${END}"; kubectl logs -n "$NAMESPACE" job/amazon-grocery-loader --tail=30; exit 1; }
        [ $((i % 30)) -eq 0 ] && echo -e "  ${DIM}$(kubectl logs -n "$NAMESPACE" job/amazon-grocery-loader --tail=1 2>/dev/null)${END}"
        sleep 2
    done
    kubectl logs -n "$NAMESPACE" job/amazon-grocery-loader > "$RESULTS_DIR/loader.log" 2>&1
else
    echo -e "\n${DIM}Phase 3: Skipped${END}"
fi

# ── Phase 4: Wait for Indexing ─────────────────────────────────────────

echo -e "\n${BLUE}Phase 4: Wait for indexing${END}"
TABLE_HOT=$(echo "${TOPIC}" | tr '-' '_')_hot
for i in $(seq 1 600); do
    PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_search \
        -H 'Content-Type: application/json' \
        -d "{\"index\":\"$TOPIC\",\"query\":{\"match\":{\"_all\":\"coffee\"}},\"size\":1}" \
        2>/dev/null || echo '{}')
    COUNT=$(echo "$PROBE" | jq '.hits.total.value // (.hits.hits | length) // 0' 2>/dev/null || echo "0")
    if [ "$COUNT" -gt 0 ]; then
        echo -e "  ${GREEN}Text index ready${END} (${i}s)"
        break
    fi
    [ $((i % 60)) -eq 0 ] && echo -e "  ${DIM}Indexing... ${i}s${END}"
    sleep 1
done
echo -e "  ${DIM}Waiting 120s for full index at 5M scale...${END}"
sleep 120

SQL_PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_sql -H 'Content-Type: application/json' \
    -d "{\"query\":\"SELECT COUNT(*) as cnt FROM ${TABLE_HOT}\"}" 2>/dev/null || echo '{}')
SQL_COUNT=$(echo "$SQL_PROBE" | jq '.[0].cnt // 0' 2>/dev/null || echo "0")
echo -e "  ${GREEN}SQL: ~${SQL_COUNT} rows${END}"

# ── Phase 5-6: Sanity + k6 ────────────────────────────────────────────

echo -e "\n${BLUE}Phase 5: Sanity checks${END}"
for q in "coffee tastes burnt" "protein powder clumps" "organic"; do
    R=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_search -H 'Content-Type: application/json' \
        -d "{\"index\":\"$TOPIC\",\"query\":{\"match\":{\"_all\":\"$q\"}},\"size\":5}" \
        2>/dev/null || echo '{}')
    RC=$(echo "$R" | jq '.hits.total.value // (.hits.hits | length) // 0' 2>/dev/null || echo "0")
    LATENCY=$(echo "$R" | jq '.took // 0' 2>/dev/null || echo "?")
    echo -e "  \"$q\" → ${RC} results (${LATENCY}ms)"
done

echo -e "\n${BLUE}Phase 6: k6 benchmark${END}"
kubectl delete testrun k6-amazon-grocery-scale -n "$NAMESPACE" 2>/dev/null || true
sleep 3
kubectl apply -f "$SCRIPT_DIR/107-amazon-scale-configmap.yaml"
kubectl apply -f "$SCRIPT_DIR/111-amazon-grocery-scale-test.yaml"

for i in $(seq 1 150); do
    STAGE=$(kubectl get testrun k6-amazon-grocery-scale -n "$NAMESPACE" \
        -o jsonpath='{.status.stage}' 2>/dev/null || echo "")
    if [ "$STAGE" = "finished" ] || [ "$STAGE" = "error" ]; then
        echo -e "  ${GREEN}k6 ${STAGE}${END}"
        break
    fi
    [ $((i % 12)) -eq 0 ] && echo -e "  ${DIM}Running... ($((i*5))s)${END}"
    sleep 5
done

# ── Phase 7-8: Collect + Summary ───────────────────────────────────────

echo -e "\n${BLUE}Phase 7: Collect${END}"
kubectl logs -n "$NAMESPACE" -l k6_cr=k6-amazon-grocery-scale --tail=-1 > "$RESULTS_DIR/k6-output.log" 2>&1 || true
for nid in 1 2 3; do
    kubectl logs "${CLUSTER_NAME}-${nid}" -n "$NAMESPACE" --tail=500 > "$RESULTS_DIR/node${nid}.log" 2>&1 || true
    kubectl exec "${CLUSTER_NAME}-${nid}" -n "$NAMESPACE" -- curl -sf "http://localhost:$((13000+nid))/metrics" > "$RESULTS_DIR/metrics-node${nid}.txt" 2>&1 || true
done

echo -e "\n${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "${BOLD}  DV-2b: Amazon Grocery Scale Results (5.1M)${END}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "\n  ${CYAN}Success Criteria:${END}"
echo -e "    Text search p50 < 15ms at 5M docs"
echo -e "    SQL COUNT p50 < 50ms"
echo -e "    Error rate < 5%"
echo -e "    No OOM during Tantivy index build"
echo ""
grep -E "text_latency|sql_latency|total_errors|iterations" "$RESULTS_DIR/k6-output.log" 2>/dev/null | tail -15 || echo "  (check k6-output.log)"
echo -e "\n  Results: ${DIM}$RESULTS_DIR/${END}"
echo ""
