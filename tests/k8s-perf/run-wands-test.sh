#!/usr/bin/env bash
#
# DV-1: WANDS Search Quality Validation
# ======================================
#
# Deploys Chronik cluster, loads 42K WANDS products (Wayfair), waits for
# vector + text + SQL indexing, then runs quality evaluation (NDCG@10,
# Precision@10, MRR) across 4 search modes against ground-truth labels.
#
# Phases:
#   0. Download WANDS dataset (git clone if missing)
#   1. Deploy ChronikCluster (3 nodes via operator CRD)
#   2. Wait for cluster ready (all pods + Raft leader)
#   3. Load WANDS products (42K records, ~1-2 min)
#   4. Wait for indexing (text + vector + columnar)
#   5. Sanity checks (text + vector + SQL)
#   6. Run quality evaluation (NDCG/Precision/MRR)
#   7. Collect results
#   8. Summary
#
# Usage:
#   ./run-wands-test.sh                 # Full run
#   ./run-wands-test.sh --skip-build    # Skip build phase
#   ./run-wands-test.sh --skip-deploy   # Skip cluster deploy (already running)
#   ./run-wands-test.sh --skip-load     # Skip data load (already loaded)
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
NAMESPACE="chronik-perf"
CLUSTER_NAME="chronik-thunderbird"
IMAGE_TAG="dv1-wands"
TOPIC="wands"
RESULTS_DIR="$SCRIPT_DIR/results/wands-$(date +%Y%m%d-%H%M%S)"
NODES=("dell-1" "dell-2" "dell-3")
DATASET_DIR="/home/ubuntu/datasets/WANDS"

# Parse flags
SKIP_BUILD=false
SKIP_DEPLOY=false
SKIP_LOAD=false

for arg in "$@"; do
    case "$arg" in
        --skip-build)  SKIP_BUILD=true ;;
        --skip-deploy) SKIP_DEPLOY=true ;;
        --skip-load)   SKIP_LOAD=true ;;
    esac
done

# Colors
GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
CYAN='\033[96m'
DIM='\033[2m'
BOLD='\033[1m'
END='\033[0m'

echo -e "${BOLD}DV-1: WANDS Search Quality Validation${END}"
echo -e "${DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "  Dataset:  WANDS (42K products, 480 queries, 233K labels)"
echo -e "  Results:  $RESULTS_DIR"
mkdir -p "$RESULTS_DIR"

# ── Phase 0: Download Dataset ─────────────────────────────────────────

echo -e "\n${BLUE}Phase 0: Dataset${END}"

if [ -f "$DATASET_DIR/dataset/product.csv" ]; then
    PRODUCT_COUNT=$(wc -l < "$DATASET_DIR/dataset/product.csv")
    echo -e "  ${GREEN}WANDS dataset found${END} (~${PRODUCT_COUNT} lines in product.csv)"
else
    echo -e "  ${DIM}Downloading WANDS dataset...${END}"
    git clone https://github.com/wayfair/WANDS.git "$DATASET_DIR" 2>&1 | tail -3
    if [ -f "$DATASET_DIR/dataset/product.csv" ]; then
        echo -e "  ${GREEN}Downloaded${END}"
    else
        echo -e "  ${RED}ERROR: product.csv not found after clone${END}"
        ls -la "$DATASET_DIR/" 2>/dev/null
        exit 1
    fi
fi

# ── Phase 0b: Build ───────────────────────────────────────────────────

if [ "$SKIP_BUILD" = false ]; then
    echo -e "\n${BLUE}Phase 0b: Build image${END}"

    echo -e "  ${DIM}Building chronik-server (release)...${END}"
    cd "$PROJECT_DIR"
    cargo build --release --bin chronik-server 2>&1 | tail -3

    echo -e "  ${DIM}Building Docker image...${END}"
    mkdir -p artifacts/linux/amd64
    cp target/release/chronik-server artifacts/linux/amd64/
    docker build -f Dockerfile.binary -t "chronik-server:${IMAGE_TAG}" --build-arg TARGETARCH=amd64 .

    echo -e "  ${DIM}Distributing image to cluster nodes...${END}"
    for node in "${NODES[@]}"; do
        echo -e "    → $node"
        docker save "chronik-server:${IMAGE_TAG}" | ssh "$node" docker load 2>/dev/null || \
            echo -e "    ${RED}Warning: Could not push to $node${END}"
    done
    echo -e "  ${GREEN}Image built and distributed${END}"
else
    echo -e "\n${DIM}Phase 0b: Skipped (--skip-build)${END}"
fi

# ── Phase 1: Deploy Cluster ───────────────────────────────────────────

if [ "$SKIP_DEPLOY" = false ]; then
    echo -e "\n${BLUE}Phase 1: Deploy ChronikCluster (3 nodes)${END}"

    kubectl apply -f "$SCRIPT_DIR/00-namespace.yaml"
    kubectl apply -f "$SCRIPT_DIR/90-chronik-thunderbird-cluster.yaml"
    kubectl apply -f "$SCRIPT_DIR/91-chronik-thunderbird-service.yaml"
    echo -e "  ${GREEN}Cluster manifests applied${END}"
else
    echo -e "\n${DIM}Phase 1: Skipped (--skip-deploy)${END}"
fi

# ── Phase 2: Wait for Cluster Ready ───────────────────────────────────

echo -e "\n${BLUE}Phase 2: Wait for cluster ready${END}"

echo -e "  ${DIM}Waiting for all 3 pods to be Running...${END}"
for i in $(seq 1 120); do
    READY_COUNT=$(kubectl get pods -n "$NAMESPACE" \
        -l "app.kubernetes.io/instance=${CLUSTER_NAME}" \
        --field-selector=status.phase=Running \
        -o name 2>/dev/null | wc -l || echo 0)
    if [ "$READY_COUNT" -ge 3 ]; then
        echo -e "  ${GREEN}All 3 pods running${END} (~$((i*3))s)"
        break
    fi
    if [ "$i" -eq 120 ]; then
        echo -e "  ${RED}Pods not ready after 360s${END}"
        kubectl get pods -n "$NAMESPACE" -l "app.kubernetes.io/instance=${CLUSTER_NAME}"
        exit 1
    fi
    if [ $((i % 10)) -eq 0 ]; then
        echo -e "  ${DIM}$READY_COUNT/3 pods ready ($((i*3))s)...${END}"
    fi
    sleep 3
done

# Wait for Raft leader election + Unified API
echo -e "  ${DIM}Waiting for Raft leader + Unified API...${END}"
sleep 15

# Check health on each node
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"
    HEALTH=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/health 2>/dev/null || echo "")
    if echo "$HEALTH" | grep -q '"status":"ok"'; then
        echo -e "  ${GREEN}Node $node_id: healthy${END}"
    else
        echo -e "  ${RED}Node $node_id: not responding${END}"
    fi
done

# ── Phase 3: Load WANDS Products ──────────────────────────────────────

if [ "$SKIP_LOAD" = false ]; then
    echo -e "\n${BLUE}Phase 3: Load WANDS products (42K)${END}"

    # Delete old job if exists
    kubectl delete job wands-loader -n "$NAMESPACE" 2>/dev/null || true
    sleep 3

    kubectl apply -f "$SCRIPT_DIR/100-wands-loader-configmap.yaml"
    kubectl apply -f "$SCRIPT_DIR/101-wands-loader-job.yaml"

    # Wait for loader to complete
    LOAD_START=$(date +%s)
    for i in $(seq 1 300); do  # Up to 10 minutes
        STATUS=$(kubectl get job wands-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
        FAILED=$(kubectl get job wands-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")

        if [ "$STATUS" = "True" ]; then
            LOAD_ELAPSED=$(( $(date +%s) - LOAD_START ))
            echo -e "  ${GREEN}Products loaded${END} (${LOAD_ELAPSED}s)"
            break
        fi
        if [ "$FAILED" = "True" ]; then
            echo -e "  ${RED}Loader failed${END}"
            kubectl logs -n "$NAMESPACE" job/wands-loader --tail=30
            exit 1
        fi
        if [ $((i % 15)) -eq 0 ]; then
            LAST_LINE=$(kubectl logs -n "$NAMESPACE" job/wands-loader --tail=1 2>/dev/null || echo "loading...")
            echo -e "  ${DIM}$LAST_LINE${END}"
        fi
        sleep 2
    done

    kubectl logs -n "$NAMESPACE" job/wands-loader > "$RESULTS_DIR/loader.log" 2>&1
else
    echo -e "\n${DIM}Phase 3: Skipped (--skip-load)${END}"
fi

# ── Phase 4: Wait for Indexing ─────────────────────────────────────────

echo -e "\n${BLUE}Phase 4: Wait for text + vector + columnar indexing${END}"

# Wait for text search to be ready
echo -e "  ${DIM}Waiting for text index...${END}"
INDEXED=false
for i in $(seq 1 300); do
    PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"$TOPIC\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"sofa\"},\"k\":1,\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
    if [ "$COUNT" -gt 0 ]; then
        INDEXED=true
        echo -e "  ${GREEN}Text index ready${END} (${i}s)"
        break
    fi
    if [ $((i % 30)) -eq 0 ]; then
        echo -e "  ${DIM}Indexing... ${i}s${END}"
    fi
    sleep 1
done

if [ "$INDEXED" != "true" ]; then
    echo -e "  ${RED}Text index not ready after 300s${END}"
    exit 1
fi

# Wait for vector indexing (needs OpenAI embedding — can take 5-15 min for 42K)
echo -e "  ${DIM}Waiting for vector indexing (42K embeddings via OpenAI)...${END}"
VECTOR_READY=false
for i in $(seq 1 1800); do  # Up to 30 minutes
    STATS=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf "http://localhost:6092/_vector/$TOPIC/stats" 2>/dev/null || echo '{}')
    TOTAL_VECTORS=$(echo "$STATS" | jq '.total_vectors // 0' 2>/dev/null || echo "0")

    if [ "$TOTAL_VECTORS" -gt 0 ]; then
        echo -e "  ${GREEN}Vector index has ${TOTAL_VECTORS} vectors${END} (${i}s)"

        # Wait until indexing stabilizes (2 consecutive checks with same count)
        sleep 30
        STATS2=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
            curl -sf "http://localhost:6092/_vector/$TOPIC/stats" 2>/dev/null || echo '{}')
        TOTAL2=$(echo "$STATS2" | jq '.total_vectors // 0' 2>/dev/null || echo "0")

        if [ "$TOTAL2" -eq "$TOTAL_VECTORS" ] || [ "$TOTAL2" -ge 40000 ]; then
            VECTOR_READY=true
            echo -e "  ${GREEN}Vector indexing complete: ${TOTAL2} vectors${END}"
            break
        fi
        echo -e "  ${DIM}Still indexing: ${TOTAL_VECTORS} → ${TOTAL2} vectors...${END}"
    fi

    if [ $((i % 60)) -eq 0 ]; then
        echo -e "  ${DIM}Waiting for embeddings... ${i}s (${TOTAL_VECTORS} vectors so far)${END}"
    fi
    sleep 1
done

if [ "$VECTOR_READY" != "true" ]; then
    echo -e "  ${RED}Warning: Vector indexing incomplete (${TOTAL_VECTORS} vectors)${END}"
    echo -e "  ${DIM}Continuing with available vectors...${END}"
fi

# Check SQL readiness
echo -e "  ${DIM}Checking SQL tables...${END}"
SQL_PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_sql \
    -H 'Content-Type: application/json' \
    -d "{\"query\":\"SELECT COUNT(*) as cnt FROM wands_hot\"}" \
    2>/dev/null || echo '{}')
SQL_COUNT=$(echo "$SQL_PROBE" | jq '.[0].cnt // 0' 2>/dev/null || echo "0")
echo -e "  ${GREEN}SQL hot buffer: ~${SQL_COUNT} rows${END}"

# ── Phase 5: Sanity Checks ────────────────────────────────────────────

echo -e "\n${BLUE}Phase 5: Sanity checks${END}"

# Text search
echo -e "  ${CYAN}Text search:${END}"
for q in "leather sofa" "dining table" "bedroom furniture"; do
    R=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/${TOPIC}/_search \
        -H 'Content-Type: application/json' \
        -d "{\"query\":{\"match\":{\"_all\":\"$q\"}},\"size\":5}" \
        2>/dev/null || echo '{}')
    RC=$(echo "$R" | jq '.hits.total.value // 0' 2>/dev/null || echo "0")
    echo -e "    \"$q\" → ${RC} hits"
done

# Vector search
echo -e "  ${CYAN}Vector search:${END}"
for q in "comfortable sofa" "modern lighting"; do
    R=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf "http://localhost:6092/_vector/$TOPIC/search" \
        -H 'Content-Type: application/json' \
        -d "{\"query\":\"$q\",\"k\":5}" \
        2>/dev/null || echo '{}')
    RC=$(echo "$R" | jq '.results | length' 2>/dev/null || echo "0")
    echo -e "    \"$q\" → ${RC} results"
done

# SQL
echo -e "  ${CYAN}SQL:${END}"
R=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_sql \
    -H 'Content-Type: application/json' \
    -d "{\"query\":\"SELECT COUNT(*) as cnt FROM wands_hot\"}" \
    2>/dev/null || echo '{}')
CNT=$(echo "$R" | jq '.[0].cnt // 0' 2>/dev/null || echo "0")
echo -e "    COUNT(*) = ${CNT}"

# ── Phase 6: Quality Evaluation ───────────────────────────────────────

echo -e "\n${BLUE}Phase 6: Run WANDS quality evaluation (480 queries x 4 modes)${END}"

# Delete old job if exists
kubectl delete job wands-quality -n "$NAMESPACE" 2>/dev/null || true
sleep 3

kubectl apply -f "$SCRIPT_DIR/102-wands-quality-configmap.yaml"
kubectl apply -f "$SCRIPT_DIR/103-wands-quality-job.yaml"

# Wait for evaluation to complete
EVAL_START=$(date +%s)
for i in $(seq 1 720); do  # Up to 24 minutes
    STATUS=$(kubectl get job wands-quality -n "$NAMESPACE" \
        -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
    FAILED=$(kubectl get job wands-quality -n "$NAMESPACE" \
        -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")

    if [ "$STATUS" = "True" ]; then
        EVAL_ELAPSED=$(( $(date +%s) - EVAL_START ))
        echo -e "  ${GREEN}Evaluation complete${END} (${EVAL_ELAPSED}s)"
        break
    fi
    if [ "$FAILED" = "True" ]; then
        echo -e "  ${RED}Evaluation failed${END}"
        kubectl logs -n "$NAMESPACE" job/wands-quality --tail=50
        break
    fi
    if [ $((i % 30)) -eq 0 ]; then
        LAST_LINE=$(kubectl logs -n "$NAMESPACE" job/wands-quality --tail=1 2>/dev/null || echo "evaluating...")
        echo -e "  ${DIM}$LAST_LINE${END}"
    fi
    sleep 2
done

# ── Phase 7: Collect Results ──────────────────────────────────────────

echo -e "\n${BLUE}Phase 7: Collect results${END}"

# Evaluation logs (contains NDCG/Precision/MRR table)
kubectl logs -n "$NAMESPACE" job/wands-quality > "$RESULTS_DIR/quality-eval.log" 2>&1 || true

# Copy JSON report from hostPath
if [ -f "$DATASET_DIR/results/wands-quality-report.json" ]; then
    cp "$DATASET_DIR/results/wands-quality-report.json" "$RESULTS_DIR/"
    echo -e "  ${GREEN}Quality report copied${END}"
fi

# Server logs
for node_id in 1 2 3; do
    kubectl logs "${CLUSTER_NAME}-${node_id}" -n "$NAMESPACE" --tail=500 \
        > "$RESULTS_DIR/node${node_id}.log" 2>&1 || true
done

# Prometheus metrics
for node_id in 1 2 3; do
    kubectl exec "${CLUSTER_NAME}-${node_id}" -n "$NAMESPACE" -- \
        curl -sf "http://localhost:$((13000 + node_id))/metrics" \
        > "$RESULTS_DIR/metrics-node${node_id}.txt" 2>&1 || true
done

echo -e "  ${GREEN}Results collected${END}"

# ── Phase 8: Summary ──────────────────────────────────────────────────

echo -e "\n${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "${BOLD}  DV-1: WANDS Search Quality Results${END}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo ""

# Print quality evaluation output
if [ -f "$RESULTS_DIR/quality-eval.log" ]; then
    echo -e "  ${CYAN}Quality Metrics:${END}"
    # Extract the results table from the evaluation log
    grep -A 20 "WANDS Search Quality Results" "$RESULTS_DIR/quality-eval.log" 2>/dev/null || \
        cat "$RESULTS_DIR/quality-eval.log"
    echo ""
fi

# Print JSON report summary if available
if [ -f "$RESULTS_DIR/wands-quality-report.json" ]; then
    echo -e "  ${CYAN}JSON Report Summary:${END}"
    jq -r '.modes | to_entries[] | "    \(.key): NDCG=\(.value.ndcg_at_k) Prec=\(.value.precision_at_k) MRR=\(.value.mrr) p50=\(.value.latency_p50_ms)ms"' \
        "$RESULTS_DIR/wands-quality-report.json" 2>/dev/null || true
    echo ""

    echo -e "  ${CYAN}Pass/Fail:${END}"
    jq -r '.success_criteria | "    BM25 NDCG >= 0.50: \(if .bm25_pass then "PASS" else "FAIL" end) (\(.bm25_ndcg_actual))\n    Hybrid NDCG >= 0.58: \(if .hybrid_pass then "PASS" else "FAIL" end) (\(.hybrid_ndcg_actual))"' \
        "$RESULTS_DIR/wands-quality-report.json" 2>/dev/null || true
    echo ""
fi

echo -e "  ${CYAN}Published Baselines:${END}"
echo -e "    ES BM25:           ~0.50-0.56"
echo -e "    ES Hybrid (kNN):   ~0.58-0.65"
echo -e "    Agentic (LLM):     ~0.65-0.72"

echo ""
echo -e "  ${CYAN}Embedding Cache (SV-1):${END}"
grep -E "embedding_cache" "$RESULTS_DIR/metrics-node1.txt" 2>/dev/null || \
    echo -e "    ${DIM}(check metrics for cache hit/miss rates)${END}"

echo ""
echo -e "  Results saved to: ${DIM}$RESULTS_DIR/${END}"
echo -e "  Files:"
ls -lh "$RESULTS_DIR/" 2>/dev/null | while IFS= read -r line; do
    echo -e "    ${DIM}$line${END}"
done
echo ""
