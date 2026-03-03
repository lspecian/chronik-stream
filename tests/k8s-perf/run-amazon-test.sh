#!/usr/bin/env bash
#
# DV-2a: Amazon Appliances Scale Validation (602K reviews)
# ========================================================
#
# Deploys Chronik cluster, loads 602K Amazon Appliances reviews, waits for
# text + SQL indexing, then benchmarks with k6 at progressive VU levels.
#
# Phases:
#   0. Download dataset (wget if missing)
#   1. Deploy ChronikCluster (3 nodes via operator CRD)
#   2. Wait for cluster ready (all pods + Raft leader)
#   3. Load Amazon Appliances reviews (602K records, ~3-5 min)
#   4. Wait for indexing (Tantivy + Parquet)
#   5. Sanity checks (text search + SQL)
#   6. Run k6 benchmark (text + SQL, 1000 VUs)
#   7. Collect results
#   8. Summary
#
# Usage:
#   ./run-amazon-test.sh                 # Full run (build + deploy + load + test)
#   ./run-amazon-test.sh --skip-build    # Skip build phase
#   ./run-amazon-test.sh --skip-deploy   # Skip cluster deploy (already running)
#   ./run-amazon-test.sh --skip-load     # Skip data load (already loaded)
#   ./run-amazon-test.sh --quick         # Load 100K records only
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
NAMESPACE="chronik-perf"
CLUSTER_NAME="chronik-thunderbird"
IMAGE_TAG="dv2a-amazon"
TOPIC="amazon-appliances"
RESULTS_DIR="$SCRIPT_DIR/results/amazon-appliances-$(date +%Y%m%d-%H%M%S)"
NODES=("dell-1" "dell-2" "dell-3")
DATASET_DIR="/home/ubuntu/datasets/amazon"
DATA_FILE="Appliances.jsonl"
DOWNLOAD_URL="https://huggingface.co/datasets/McAuley-Lab/Amazon-Reviews-2023/resolve/main/raw/review_categories/Appliances.jsonl?download=true"

# Parse flags
SKIP_BUILD=false
SKIP_DEPLOY=false
SKIP_LOAD=false
MAX_RECORDS=0  # 0 = all 602K

for arg in "$@"; do
    case "$arg" in
        --skip-build)  SKIP_BUILD=true ;;
        --skip-deploy) SKIP_DEPLOY=true ;;
        --skip-load)   SKIP_LOAD=true ;;
        --quick)       MAX_RECORDS=100000 ;;
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

echo -e "${BOLD}DV-2a: Amazon Appliances Scale Validation${END}"
echo -e "${DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "  Dataset:  Amazon Reviews 2023 — Appliances (602K reviews)"
echo -e "  Results:  $RESULTS_DIR"
if [ "$MAX_RECORDS" -gt 0 ]; then
    echo -e "  ${CYAN}Quick mode: ${MAX_RECORDS} records only${END}"
fi
mkdir -p "$RESULTS_DIR"

# ── Phase 0: Download Dataset ─────────────────────────────────────────

echo -e "\n${BLUE}Phase 0: Dataset${END}"

mkdir -p "$DATASET_DIR"
if [ -f "$DATASET_DIR/$DATA_FILE" ]; then
    FILE_SIZE=$(du -h "$DATASET_DIR/$DATA_FILE" | cut -f1)
    echo -e "  ${GREEN}Dataset found${END} ($DATA_FILE, ${FILE_SIZE})"
else
    echo -e "  ${DIM}Downloading Amazon Appliances dataset...${END}"
    wget -q --show-progress -O "$DATASET_DIR/$DATA_FILE" "$DOWNLOAD_URL"
    if [ -f "$DATASET_DIR/$DATA_FILE" ]; then
        FILE_SIZE=$(du -h "$DATASET_DIR/$DATA_FILE" | cut -f1)
        echo -e "  ${GREEN}Downloaded${END} (${FILE_SIZE})"
    else
        echo -e "  ${RED}ERROR: Download failed${END}"
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

# ── Phase 3: Load Amazon Reviews ──────────────────────────────────────

if [ "$SKIP_LOAD" = false ]; then
    echo -e "\n${BLUE}Phase 3: Load Amazon Appliances reviews${END}"

    if [ "$MAX_RECORDS" -gt 0 ]; then
        echo -e "  ${CYAN}Quick mode: loading $MAX_RECORDS records${END}"
    else
        echo -e "  ${DIM}Loading all ~602K reviews...${END}"
    fi

    # Delete old job if exists
    kubectl delete job amazon-appliances-loader -n "$NAMESPACE" 2>/dev/null || true
    sleep 3

    kubectl apply -f "$SCRIPT_DIR/105-amazon-loader-configmap.yaml"

    # Apply job, overriding MAX_RECORDS if needed
    if [ "$MAX_RECORDS" -gt 0 ]; then
        cat "$SCRIPT_DIR/106-amazon-loader-job.yaml" | \
            sed "s/value: \"0\"/value: \"${MAX_RECORDS}\"/" | \
            kubectl apply -f -
    else
        kubectl apply -f "$SCRIPT_DIR/106-amazon-loader-job.yaml"
    fi

    # Wait for loader to complete
    LOAD_START=$(date +%s)
    for i in $(seq 1 900); do  # Up to 30 minutes
        STATUS=$(kubectl get job amazon-appliances-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
        FAILED=$(kubectl get job amazon-appliances-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")

        if [ "$STATUS" = "True" ]; then
            LOAD_ELAPSED=$(( $(date +%s) - LOAD_START ))
            echo -e "  ${GREEN}Reviews loaded${END} (${LOAD_ELAPSED}s)"
            break
        fi
        if [ "$FAILED" = "True" ]; then
            echo -e "  ${RED}Loader failed${END}"
            kubectl logs -n "$NAMESPACE" job/amazon-appliances-loader --tail=30
            exit 1
        fi
        if [ $((i % 15)) -eq 0 ]; then
            LAST_LINE=$(kubectl logs -n "$NAMESPACE" job/amazon-appliances-loader --tail=1 2>/dev/null || echo "loading...")
            echo -e "  ${DIM}$LAST_LINE${END}"
        fi
        sleep 2
    done

    kubectl logs -n "$NAMESPACE" job/amazon-appliances-loader > "$RESULTS_DIR/loader.log" 2>&1
else
    echo -e "\n${DIM}Phase 3: Skipped (--skip-load)${END}"
fi

# ── Phase 4: Wait for Indexing ─────────────────────────────────────────

echo -e "\n${BLUE}Phase 4: Wait for Tantivy + Parquet indexing${END}"

echo -e "  ${DIM}Waiting for text index...${END}"
TABLE_HOT=$(echo "${TOPIC}" | tr '-' '_')_hot
INDEXED=false
for i in $(seq 1 300); do
    PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"$TOPIC\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"blender\"},\"k\":1,\"result_format\":\"merged\"}" \
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

# Wait additional time for full indexing
echo -e "  ${DIM}Waiting 60s for full index build...${END}"
sleep 60

# Check SQL readiness
echo -e "  ${DIM}Checking SQL tables...${END}"
SQL_PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_sql \
    -H 'Content-Type: application/json' \
    -d "{\"query\":\"SELECT COUNT(*) as cnt FROM ${TABLE_HOT}\"}" \
    2>/dev/null || echo '{}')
SQL_COUNT=$(echo "$SQL_PROBE" | jq '.[0].cnt // 0' 2>/dev/null || echo "0")
echo -e "  ${GREEN}SQL hot buffer: ~${SQL_COUNT} rows${END}"

# ── Phase 5: Sanity Checks ────────────────────────────────────────────

echo -e "\n${BLUE}Phase 5: Sanity checks${END}"

# Text search on each node
echo -e "  ${CYAN}Text search per node:${END}"
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"
    for q in "blender not working" "dishwasher leaking" "warranty"; do
        R=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
            curl -sf http://localhost:6092/_query \
            -H 'Content-Type: application/json' \
            -d "{\"sources\":[{\"topic\":\"$TOPIC\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"$q\"},\"k\":5,\"rank\":{\"profile\":\"relevance\"},\"result_format\":\"merged\"}" \
            2>/dev/null || echo '{}')
        RC=$(echo "$R" | jq '.results | length' 2>/dev/null || echo "0")
        LATENCY=$(echo "$R" | jq '.stats.latency_ms // 0' 2>/dev/null || echo "?")
        echo -e "    Node $node_id | \"$q\" → ${RC} results (${LATENCY}ms)"
    done
done

# SQL on each node
echo -e "\n  ${CYAN}SQL per node:${END}"
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"
    R=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_sql \
        -H 'Content-Type: application/json' \
        -d "{\"query\":\"SELECT COUNT(*) as cnt FROM ${TABLE_HOT}\"}" \
        2>/dev/null || echo '{}')
    CNT=$(echo "$R" | jq '.[0].cnt // 0' 2>/dev/null || echo "0")

    R2=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_sql \
        -H 'Content-Type: application/json' \
        -d "{\"query\":\"SELECT rating, COUNT(*) as cnt FROM ${TABLE_HOT} GROUP BY rating ORDER BY rating DESC\"}" \
        2>/dev/null || echo '{}')
    GROUPS=$(echo "$R2" | jq 'length // 0' 2>/dev/null || echo "0")

    echo -e "    Node $node_id | COUNT = ${CNT}, GROUP BY rating → ${GROUPS} groups"
done

echo "Sanity check complete" >> "$RESULTS_DIR/sanity.log"

# ── Phase 6: Run k6 Benchmark ─────────────────────────────────────────

echo -e "\n${BLUE}Phase 6: Run k6 scale benchmark (~5 min, 4 pods)${END}"

# Clean old test runs
kubectl delete testrun k6-amazon-scale -n "$NAMESPACE" 2>/dev/null || true
sleep 3

kubectl apply -f "$SCRIPT_DIR/107-amazon-scale-configmap.yaml"
kubectl apply -f "$SCRIPT_DIR/108-amazon-scale-test.yaml"

echo -e "  ${DIM}Waiting for k6 test to complete...${END}"
for i in $(seq 1 150); do
    STAGE=$(kubectl get testrun k6-amazon-scale -n "$NAMESPACE" \
        -o jsonpath='{.status.stage}' 2>/dev/null || echo "")
    if [ "$STAGE" = "finished" ] || [ "$STAGE" = "error" ]; then
        echo -e "  ${GREEN}k6 test ${STAGE}${END} (~$((i*5))s)"
        break
    fi
    if [ $((i % 12)) -eq 0 ]; then
        echo -e "  ${DIM}Running... ($((i*5))s)${END}"
    fi
    sleep 5
done

# ── Phase 7: Collect Results ──────────────────────────────────────────

echo -e "\n${BLUE}Phase 7: Collect results${END}"

# k6 output
kubectl logs -n "$NAMESPACE" -l k6_cr=k6-amazon-scale --tail=-1 \
    > "$RESULTS_DIR/k6-output.log" 2>&1 || true

# Server logs from each node
for node_id in 1 2 3; do
    kubectl logs "${CLUSTER_NAME}-${node_id}" -n "$NAMESPACE" --tail=500 \
        > "$RESULTS_DIR/node${node_id}.log" 2>&1 || true
done

# Prometheus metrics from each node
for node_id in 1 2 3; do
    kubectl exec "${CLUSTER_NAME}-${node_id}" -n "$NAMESPACE" -- \
        curl -sf "http://localhost:$((13000 + node_id))/metrics" \
        > "$RESULTS_DIR/metrics-node${node_id}.txt" 2>&1 || true
done

echo -e "  ${GREEN}Results collected${END}"

# ── Phase 8: Summary ──────────────────────────────────────────────────

echo -e "\n${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "${BOLD}  DV-2a: Amazon Appliances Scale Test Results (602K reviews)${END}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo ""

echo -e "  ${CYAN}Success Criteria:${END}"
echo -e "    Text search p50 < 10ms at 602K docs"
echo -e "    SQL COUNT p50 < 20ms"
echo -e "    SQL GROUP BY p50 < 50ms"
echo -e "    Error rate < 5%"
echo ""

echo -e "  ${CYAN}k6 Summary:${END}"
grep -E "text_latency|sql_latency|total_latency|text_errors|sql_errors|total_errors|iterations|vus" \
    "$RESULTS_DIR/k6-output.log" 2>/dev/null | tail -20 || \
    echo "  (check $RESULTS_DIR/k6-output.log)"

echo ""
echo -e "  Results saved to: ${DIM}$RESULTS_DIR/${END}"
echo -e "  Files:"
ls -lh "$RESULTS_DIR/" 2>/dev/null | while IFS= read -r line; do
    echo -e "    ${DIM}$line${END}"
done
echo ""
