#!/usr/bin/env bash
#
# SV-2b Re-run: Vector Search Validation with 5-Bug Fixes
# ========================================================
#
# Re-runs the SV-2b vector benchmark after applying fixes for:
#   Fix 1: Per-topic vector.enabled config race (auto_create + merge)
#   Fix 2: Vector stats loading_complete flag
#   Fix 3: Operator spec-hash for env change detection
#   Fix 4: valueFromSecret in cluster manifest
#   Fix 5: WalIndexer backfill for vector embeddings
#
# Usage:
#   ./run-sv2b-rerun.sh                  # Full run (build + deploy + load + test)
#   ./run-sv2b-rerun.sh --skip-build     # Reuse existing image
#   ./run-sv2b-rerun.sh --skip-load      # Data already loaded
#   ./run-sv2b-rerun.sh --quick          # 100K lines (fast validation)
#   ./run-sv2b-rerun.sh --backfill-only  # Just run backfill + benchmark (data loaded)
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
NAMESPACE="chronik-perf"
CLUSTER_NAME="chronik-thunderbird"
IMAGE_TAG="sv2b-fixes"
VECTOR_TOPIC="thunderbird-vector"
TEXT_TOPIC="thunderbird"
RESULTS_DIR="$SCRIPT_DIR/results/sv2b-rerun-$(date +%Y%m%d-%H%M%S)"
NODES=("dell-1" "dell-2" "dell-3")

# Parse flags
SKIP_BUILD=false
SKIP_LOAD=false
BACKFILL_ONLY=false
MAX_LINES=1000000  # 1M for SV-2b (vs 16.6M for SV-2a)

for arg in "$@"; do
    case "$arg" in
        --skip-build)    SKIP_BUILD=true ;;
        --skip-load)     SKIP_LOAD=true ;;
        --backfill-only) BACKFILL_ONLY=true; SKIP_BUILD=true; SKIP_LOAD=true ;;
        --quick)         MAX_LINES=100000 ;;
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

echo -e "${BOLD}SV-2b Re-run: Vector Search + 5-Bug Fixes${END}"
echo -e "${DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "  Results: $RESULTS_DIR"
echo -e "  Vector topic: $VECTOR_TOPIC"
echo -e "  Max lines: $MAX_LINES"
mkdir -p "$RESULTS_DIR"

# Helper: get first healthy node's pod name
get_healthy_pod() {
    for node_id in 1 2 3; do
        local pod="${CLUSTER_NAME}-${node_id}"
        if kubectl exec "$pod" -n "$NAMESPACE" -- curl -sf http://localhost:6092/health &>/dev/null; then
            echo "$pod"
            return
        fi
    done
    echo ""
}

# ── Phase 0: Build ────────────────────────────────────────────────────────

if [ "$SKIP_BUILD" = false ]; then
    echo -e "\n${BLUE}Phase 0: Build image with 5-bug fixes${END}"

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
    echo -e "\n${DIM}Phase 0: Skipped (--skip-build)${END}"
fi

# ── Phase 1: Deploy Cluster ──────────────────────────────────────────────

if [ "$BACKFILL_ONLY" = false ]; then
    echo -e "\n${BLUE}Phase 1: Deploy ChronikCluster (3 nodes, sv2b-fixes image)${END}"

    # Ensure namespace and secret exist
    kubectl create namespace "$NAMESPACE" 2>/dev/null || true
    kubectl apply -f "$SCRIPT_DIR/95-openai-secret.yaml"

    # Update the cluster manifest to use our new image tag
    # (Temporarily patch for this run)
    cat "$SCRIPT_DIR/90-chronik-thunderbird-cluster.yaml" | \
        sed "s|image: chronik-server:.*|image: chronik-server:${IMAGE_TAG}|" | \
        kubectl apply -f -

    kubectl apply -f "$SCRIPT_DIR/91-chronik-thunderbird-service.yaml"

    # ── Phase 2: Wait for Cluster Ready ──────────────────────────────────

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

    # Wait for Raft leader + Unified API
    echo -e "  ${DIM}Waiting for Raft leader + Unified API...${END}"
    sleep 15

    # Health check each node
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
fi

# ── Phase 3: Load Vector Data ──────────────────────────────────────────

if [ "$SKIP_LOAD" = false ] && [ "$BACKFILL_ONLY" = false ]; then
    echo -e "\n${BLUE}Phase 3: Load vector data (${MAX_LINES} messages → ${VECTOR_TOPIC})${END}"

    # Delete old loader job
    kubectl delete job thunderbird-vector-loader -n "$NAMESPACE" 2>/dev/null || true
    sleep 3

    # Apply loader config + script
    kubectl apply -f "$SCRIPT_DIR/92-thunderbird-loader-configmap.yaml"

    # Apply vector loader job with MAX_LINES override
    cat "$SCRIPT_DIR/95-thunderbird-vector-loader-job.yaml" | \
        sed "s/value: \"1000000\"/value: \"${MAX_LINES}\"/" | \
        kubectl apply -f -

    # Wait for loader to complete
    LOAD_START=$(date +%s)
    echo -e "  ${DIM}Loading data (this takes ~10-30 min for 1M lines)...${END}"
    for i in $(seq 1 2700); do
        STATUS=$(kubectl get job thunderbird-vector-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
        FAILED=$(kubectl get job thunderbird-vector-loader -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")

        if [ "$STATUS" = "True" ]; then
            LOAD_ELAPSED=$(( $(date +%s) - LOAD_START ))
            echo -e "  ${GREEN}Data loaded${END} (${LOAD_ELAPSED}s)"
            break
        fi
        if [ "$FAILED" = "True" ]; then
            echo -e "  ${RED}Data loader failed${END}"
            kubectl logs -n "$NAMESPACE" job/thunderbird-vector-loader --tail=30
            exit 1
        fi
        if [ $((i % 30)) -eq 0 ]; then
            LAST_LINE=$(kubectl logs -n "$NAMESPACE" job/thunderbird-vector-loader --tail=1 2>/dev/null || echo "loading...")
            echo -e "  ${DIM}$LAST_LINE${END}"
        fi
        sleep 2
    done

    kubectl logs -n "$NAMESPACE" job/thunderbird-vector-loader > "$RESULTS_DIR/vector-loader.log" 2>&1
else
    echo -e "\n${DIM}Phase 3: Skipped (--skip-load or --backfill-only)${END}"
fi

# ── Phase 4: Wait for Vector Indexing ──────────────────────────────────

echo -e "\n${BLUE}Phase 4: Wait for vector indexing${END}"

echo -e "  ${DIM}Checking vector stats on each node (Fix 2 validation)...${END}"
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"

    for attempt in $(seq 1 60); do
        STATS=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
            curl -sf "http://localhost:6092/_vector/${VECTOR_TOPIC}/stats" 2>/dev/null || echo '{}')
        TOTAL=$(echo "$STATS" | jq '.total_vectors // 0' 2>/dev/null || echo "0")
        LOADING=$(echo "$STATS" | jq '.loading // true' 2>/dev/null || echo "true")

        if [ "$LOADING" = "false" ] && [ "$TOTAL" -gt 0 ]; then
            echo -e "  ${GREEN}Node $node_id: ${TOTAL} vectors indexed, loading=false${END}"
            break
        fi
        if [ "$attempt" -eq 60 ]; then
            echo -e "  ${CYAN}Node $node_id: ${TOTAL} vectors, loading=${LOADING} (after 300s)${END}"
        fi
        if [ $((attempt % 10)) -eq 0 ]; then
            echo -e "  ${DIM}Node $node_id: ${TOTAL} vectors, loading=${LOADING} (${attempt}*5s)...${END}"
        fi
        sleep 5
    done
done

# Save stats for report
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"
    kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf "http://localhost:6092/_vector/${VECTOR_TOPIC}/stats" \
        > "$RESULTS_DIR/vector-stats-node${node_id}.json" 2>&1 || true
done

# ── Phase 4b: Backfill if needed ───────────────────────────────────────

echo -e "\n${BLUE}Phase 4b: Check if backfill needed (Fix 5 validation)${END}"

# Check if any node has 0 vectors despite data being loaded
NEEDS_BACKFILL=false
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"
    TOTAL=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf "http://localhost:6092/_vector/${VECTOR_TOPIC}/stats" 2>/dev/null | \
        jq '.total_vectors // 0' 2>/dev/null || echo "0")
    if [ "$TOTAL" -eq 0 ]; then
        NEEDS_BACKFILL=true
        echo -e "  ${CYAN}Node $node_id has 0 vectors — backfill needed${END}"
    fi
done

if [ "$NEEDS_BACKFILL" = true ] || [ "$BACKFILL_ONLY" = true ]; then
    echo -e "  ${DIM}Running backfill via /_vector/${VECTOR_TOPIC}/backfill...${END}"
    POD=$(get_healthy_pod)
    if [ -n "$POD" ]; then
        BACKFILL_START=$(date +%s)
        BACKFILL_RESULT=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
            curl -sf -X POST "http://localhost:6092/_vector/${VECTOR_TOPIC}/backfill" \
            -H 'Content-Type: application/json' \
            -d '{"partition": null}' 2>/dev/null || echo '{"error": "failed"}')

        BACKFILL_ELAPSED=$(( $(date +%s) - BACKFILL_START ))
        echo "$BACKFILL_RESULT" > "$RESULTS_DIR/backfill-result.json"
        echo -e "  ${GREEN}Backfill complete${END} (${BACKFILL_ELAPSED}s)"
        echo -e "  ${DIM}$BACKFILL_RESULT${END}"

        # Wait for backfilled vectors to be available
        echo -e "  ${DIM}Waiting 30s for backfilled vectors to settle...${END}"
        sleep 30

        # Re-check stats after backfill
        for node_id in 1 2 3; do
            POD_N="${CLUSTER_NAME}-${node_id}"
            TOTAL=$(kubectl exec "$POD_N" -n "$NAMESPACE" -- \
                curl -sf "http://localhost:6092/_vector/${VECTOR_TOPIC}/stats" 2>/dev/null | \
                jq '.total_vectors // 0' 2>/dev/null || echo "0")
            echo -e "  ${GREEN}Node $node_id post-backfill: ${TOTAL} vectors${END}"
        done
    else
        echo -e "  ${RED}No healthy pod found for backfill${END}"
    fi
else
    echo -e "  ${GREEN}All nodes have vectors — no backfill needed${END}"
fi

# ── Phase 5: Sanity Checks ────────────────────────────────────────────

echo -e "\n${BLUE}Phase 5: Sanity checks (all 3 query types)${END}"

echo -e "  ${CYAN}Vector search per node:${END}"
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"
    R=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"${VECTOR_TOPIC}\",\"modes\":[\"vector\"]}],\"q\":{\"text\":\"disk failure causing data loss\"},\"k\":5,\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    RC=$(echo "$R" | jq '.results | length' 2>/dev/null || echo "0")
    LATENCY=$(echo "$R" | jq '.stats.latency_ms // 0' 2>/dev/null || echo "?")
    # Check cosine similarity of top result
    SIM=$(echo "$R" | jq '.results[0].score // 0' 2>/dev/null || echo "?")
    echo -e "    Node $node_id | ${RC} results, top_score=${SIM}, latency=${LATENCY}ms"
done

echo -e "\n  ${CYAN}Hybrid search per node (profile=relevance):${END}"
for node_id in 1 2 3; do
    POD="${CLUSTER_NAME}-${node_id}"
    R=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"${VECTOR_TOPIC}\",\"modes\":[\"text\",\"vector\"]}],\"q\":{\"text\":\"kernel panic memory allocation\"},\"k\":5,\"rank\":{\"profile\":\"relevance\"},\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    RC=$(echo "$R" | jq '.results | length' 2>/dev/null || echo "0")
    LATENCY=$(echo "$R" | jq '.stats.latency_ms // 0' 2>/dev/null || echo "?")
    echo -e "    Node $node_id | ${RC} results, latency=${LATENCY}ms"
done

echo -e "\n  ${CYAN}Ranking profile comparison:${END}"
POD=$(get_healthy_pod)
if [ -n "$POD" ]; then
    echo -e "  ${DIM}Query: 'hardware error correctable ECC memory' on ${POD}${END}"

    # Default ranking
    R_DEF=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"${VECTOR_TOPIC}\",\"modes\":[\"text\",\"vector\"]}],\"q\":{\"text\":\"hardware error correctable ECC memory\"},\"k\":5,\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    echo -e "    Default profile: $(echo "$R_DEF" | jq '.results | length' 2>/dev/null || echo 0) results"
    echo "$R_DEF" | jq -c '.results[:3][] | {offset, score}' 2>/dev/null | while read -r line; do
        echo -e "      $line"
    done

    # Relevance ranking
    R_REL=$(kubectl exec "$POD" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query \
        -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"${VECTOR_TOPIC}\",\"modes\":[\"text\",\"vector\"]}],\"q\":{\"text\":\"hardware error correctable ECC memory\"},\"k\":5,\"rank\":{\"profile\":\"relevance\"},\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    echo -e "    Relevance profile: $(echo "$R_REL" | jq '.results | length' 2>/dev/null || echo 0) results"
    echo "$R_REL" | jq -c '.results[:3][] | {offset, score}' 2>/dev/null | while read -r line; do
        echo -e "      $line"
    done

    # Save ranking comparison
    echo "$R_DEF" > "$RESULTS_DIR/ranking-default.json"
    echo "$R_REL" > "$RESULTS_DIR/ranking-relevance.json"
fi

# ── Phase 6: Run k6 Vector Benchmark ──────────────────────────────────

echo -e "\n${BLUE}Phase 6: Run k6 vector benchmark (~5 min, 4 pods, mixed mode)${END}"

# Clean old test runs
kubectl delete testrun k6-thunderbird-vector -n "$NAMESPACE" 2>/dev/null || true
sleep 3

# Apply updated k6 script + test run
kubectl apply -f "$SCRIPT_DIR/96-thunderbird-vector-configmap.yaml"
kubectl apply -f "$SCRIPT_DIR/97-thunderbird-vector-test.yaml"

echo -e "  ${DIM}Waiting for k6 test to complete...${END}"
for i in $(seq 1 150); do
    STAGE=$(kubectl get testrun k6-thunderbird-vector -n "$NAMESPACE" \
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
kubectl logs -n "$NAMESPACE" -l k6_cr=k6-thunderbird-vector --tail=-1 \
    > "$RESULTS_DIR/k6-output.log" 2>&1 || true

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

# Final vector stats
for node_id in 1 2 3; do
    kubectl exec "${CLUSTER_NAME}-${node_id}" -n "$NAMESPACE" -- \
        curl -sf "http://localhost:6092/_vector/${VECTOR_TOPIC}/stats" \
        > "$RESULTS_DIR/final-vector-stats-node${node_id}.json" 2>&1 || true
done

echo -e "  ${GREEN}Results collected${END}"

# ── Phase 8: Summary ─────────────────────────────────────────────────

echo -e "\n${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "${BOLD}  SV-2b Re-run: Vector Search Validation Results${END}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo ""

echo -e "  ${CYAN}Fixes Applied:${END}"
echo -e "    Fix 1: Per-topic vector.enabled race condition (auto_create + merge)"
echo -e "    Fix 2: Vector stats loading_complete flag"
echo -e "    Fix 3: Operator spec-hash for env change detection"
echo -e "    Fix 4: valueFromSecret in cluster manifest"
echo -e "    Fix 5: WalIndexer backfill for vector embeddings"
echo ""

echo -e "  ${CYAN}SV-2b Success Criteria:${END}"
echo -e "    Vector search p50 < 500ms (warm)"
echo -e "    Hybrid search p50 < 500ms"
echo -e "    Cosine similarity > 0.5 for semantic queries"
echo -e "    All 3 nodes have vectors indexed"
echo -e "    Error rate < 5%"
echo ""

echo -e "  ${CYAN}Vector Stats (per node):${END}"
for node_id in 1 2 3; do
    STATS_FILE="$RESULTS_DIR/final-vector-stats-node${node_id}.json"
    if [ -f "$STATS_FILE" ]; then
        TOTAL=$(cat "$STATS_FILE" | jq '.total_vectors // 0' 2>/dev/null || echo "?")
        LOADING=$(cat "$STATS_FILE" | jq '.loading // "?"' 2>/dev/null || echo "?")
        echo -e "    Node $node_id: ${TOTAL} vectors, loading=${LOADING}"
    else
        echo -e "    Node $node_id: (no stats)"
    fi
done
echo ""

echo -e "  ${CYAN}k6 Benchmark Summary:${END}"
grep -E "vector_latency|hybrid_latency|text_latency|total_latency|vector_errors|hybrid_errors|text_errors|total_errors|rank_default_latency|rank_relevance_latency|iterations|vus" \
    "$RESULTS_DIR/k6-output.log" 2>/dev/null | tail -30 || \
    echo "  (check $RESULTS_DIR/k6-output.log)"

echo ""
echo -e "  ${CYAN}Embedding Cache:${END}"
grep -E "embedding_cache" "$RESULTS_DIR/metrics-node1.txt" 2>/dev/null || \
    echo "  (no embedding cache metrics)"

echo ""
echo -e "  Results saved to: ${DIM}$RESULTS_DIR/${END}"
echo -e "  Files:"
ls -lh "$RESULTS_DIR/" 2>/dev/null | while IFS= read -r line; do
    echo -e "    ${DIM}$line${END}"
done
echo ""
