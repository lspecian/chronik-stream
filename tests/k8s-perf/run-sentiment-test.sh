#!/usr/bin/env bash
#
# DV-3: Sentiment Labeling Pipeline Validation
# =============================================
#
# Labels Amazon Appliances reviews with sentiment and category, then
# evaluates label quality through search and SQL analytics.
#
# Phases:
#   0. Pre-checks (source topic exists, cluster running)
#   1. Rating-based labeling (DV-3a — fast, zero-cost)
#   2. Wait for labeled topic indexing
#   3. Run label quality evaluation
#   4. (Optional) LLM-based labeling (DV-3b — Ollama or OpenAI)
#   5. Wait for LLM-labeled topic indexing
#   6. Run LLM label quality evaluation
#   7. Collect results
#   8. Summary
#
# Usage:
#   ./run-sentiment-test.sh                  # Full run (rating only)
#   ./run-sentiment-test.sh --with-llm       # Include LLM labeling
#   ./run-sentiment-test.sh --skip-deploy    # Skip cluster deploy
#   ./run-sentiment-test.sh --skip-rating    # Skip rating labeling (already done)
#   ./run-sentiment-test.sh --source amazon-grocery  # Use different source topic
#   ./run-sentiment-test.sh --max 50000      # Limit records to label
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
NAMESPACE="chronik-perf"
CLUSTER_NAME="chronik-thunderbird"
SOURCE_TOPIC="amazon-appliances"
RESULTS_DIR="$SCRIPT_DIR/results/sentiment-$(date +%Y%m%d-%H%M%S)"

SKIP_DEPLOY=false
SKIP_RATING=false
WITH_LLM=false
MAX_RECORDS=0

for arg in "$@"; do
    case "$arg" in
        --skip-deploy) SKIP_DEPLOY=true ;;
        --skip-rating) SKIP_RATING=true ;;
        --with-llm)    WITH_LLM=true ;;
        --source)      ;; # Handled below
        --max)         ;; # Handled below
    esac
done
# Parse --source TOPIC and --max N
for i in $(seq 1 $#); do
    if [ "${!i}" = "--source" ]; then
        j=$((i+1))
        SOURCE_TOPIC="${!j}"
    fi
    if [ "${!i}" = "--max" ]; then
        j=$((i+1))
        MAX_RECORDS="${!j}"
    fi
done

RATING_DEST="${SOURCE_TOPIC}-labeled"
LLM_DEST="${SOURCE_TOPIC}-llm-labeled"
TABLE_HOT=$(echo "${RATING_DEST}" | tr '-' '_')_hot

GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
CYAN='\033[96m'
DIM='\033[2m'
BOLD='\033[1m'
END='\033[0m'

echo -e "${BOLD}DV-3: Sentiment Labeling Pipeline Validation${END}"
echo -e "${DIM}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "  Source topic: $SOURCE_TOPIC"
echo -e "  Rating dest:  $RATING_DEST"
[ "$WITH_LLM" = true ] && echo -e "  LLM dest:     $LLM_DEST"
echo -e "  Max records:  $([ "$MAX_RECORDS" = "0" ] && echo "all" || echo "$MAX_RECORDS")"
echo -e "  Results:      $RESULTS_DIR"
mkdir -p "$RESULTS_DIR"

# ── Phase 0: Pre-checks ─────────────────────────────────────────────

echo -e "\n${BLUE}Phase 0: Pre-checks${END}"

# Check cluster is running
READY_COUNT=$(kubectl get pods -n "$NAMESPACE" \
    -l "app.kubernetes.io/instance=${CLUSTER_NAME}" \
    --field-selector=status.phase=Running -o name 2>/dev/null | wc -l || echo 0)
if [ "$READY_COUNT" -lt 3 ]; then
    echo -e "  ${RED}Cluster not ready (${READY_COUNT}/3 pods)${END}"
    if [ "$SKIP_DEPLOY" = false ]; then
        echo -e "  ${DIM}Deploying cluster...${END}"
        kubectl apply -f "$SCRIPT_DIR/00-namespace.yaml"
        kubectl apply -f "$SCRIPT_DIR/90-chronik-thunderbird-cluster.yaml"
        kubectl apply -f "$SCRIPT_DIR/91-chronik-thunderbird-service.yaml"
        echo -e "\n${BLUE}Waiting for cluster ready${END}"
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
    else
        echo -e "  ${RED}Use --skip-deploy=false or start cluster first${END}"
        exit 1
    fi
else
    echo -e "  ${GREEN}Cluster running (${READY_COUNT} pods)${END}"
fi

# Check source topic has data
SOURCE_TABLE=$(echo "${SOURCE_TOPIC}" | tr '-' '_')_hot
SQL_PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_sql -H 'Content-Type: application/json' \
    -d "{\"query\":\"SELECT COUNT(*) as cnt FROM ${SOURCE_TABLE}\"}" 2>/dev/null || echo '[]')
SOURCE_COUNT=$(echo "$SQL_PROBE" | jq '.[0].cnt // 0' 2>/dev/null || echo "0")
if [ "$SOURCE_COUNT" -eq 0 ]; then
    echo -e "  ${RED}Source topic '${SOURCE_TOPIC}' has no data — run DV-2a first${END}"
    exit 1
fi
echo -e "  ${GREEN}Source topic has ~${SOURCE_COUNT} records${END}"

# ── Phase 1: Rating-based labeling (DV-3a) ──────────────────────────

if [ "$SKIP_RATING" = false ]; then
    echo -e "\n${BLUE}Phase 1: Rating-based sentiment labeling${END}"
    echo -e "  ${DIM}Source: ${SOURCE_TOPIC} → Dest: ${RATING_DEST}${END}"

    kubectl delete job sentiment-rating-labeler -n "$NAMESPACE" 2>/dev/null || true
    sleep 3
    kubectl apply -f "$SCRIPT_DIR/115-sentiment-rating-configmap.yaml"

    # Apply job with overridden env vars if needed
    if [ "$SOURCE_TOPIC" != "amazon-appliances" ] || [ "$MAX_RECORDS" != "0" ]; then
        cat "$SCRIPT_DIR/116-sentiment-rating-job.yaml" | \
            sed "s/value: \"amazon-appliances\"\$/value: \"${SOURCE_TOPIC}\"/" | \
            sed "s/value: \"amazon-appliances-labeled\"/value: \"${RATING_DEST}\"/" | \
            sed "s/value: \"0\"\$/value: \"${MAX_RECORDS}\"/" | \
            kubectl apply -f -
    else
        kubectl apply -f "$SCRIPT_DIR/116-sentiment-rating-job.yaml"
    fi

    LABEL_START=$(date +%s)
    for i in $(seq 1 1800); do
        STATUS=$(kubectl get job sentiment-rating-labeler -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
        FAILED=$(kubectl get job sentiment-rating-labeler -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")
        if [ "$STATUS" = "True" ]; then
            echo -e "  ${GREEN}Labeled${END} ($(($(date +%s) - LABEL_START))s)"
            break
        fi
        [ "$FAILED" = "True" ] && { echo -e "  ${RED}Failed${END}"; kubectl logs -n "$NAMESPACE" job/sentiment-rating-labeler --tail=30; exit 1; }
        [ $((i % 30)) -eq 0 ] && echo -e "  ${DIM}$(kubectl logs -n "$NAMESPACE" job/sentiment-rating-labeler --tail=1 2>/dev/null)${END}"
        sleep 2
    done
    kubectl logs -n "$NAMESPACE" job/sentiment-rating-labeler > "$RESULTS_DIR/rating-labeler.log" 2>&1
else
    echo -e "\n${DIM}Phase 1: Skipped${END}"
fi

# ── Phase 2: Wait for labeled topic indexing ─────────────────────────

echo -e "\n${BLUE}Phase 2: Wait for labeled topic indexing${END}"
for i in $(seq 1 300); do
    PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
        curl -sf http://localhost:6092/_query -H 'Content-Type: application/json' \
        -d "{\"sources\":[{\"topic\":\"$RATING_DEST\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"positive\"},\"k\":1,\"result_format\":\"merged\"}" \
        2>/dev/null || echo '{}')
    COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
    if [ "$COUNT" -gt 0 ]; then
        echo -e "  ${GREEN}Text index ready${END} (${i}s)"
        break
    fi
    [ $((i % 30)) -eq 0 ] && echo -e "  ${DIM}Indexing... ${i}s${END}"
    sleep 1
done
echo -e "  ${DIM}Waiting 60s for full index stability...${END}"
sleep 60

# Check SQL
LABEL_SQL=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
    curl -sf http://localhost:6092/_sql -H 'Content-Type: application/json' \
    -d "{\"query\":\"SELECT COUNT(*) as cnt FROM ${TABLE_HOT}\"}" 2>/dev/null || echo '[]')
LABEL_COUNT=$(echo "$LABEL_SQL" | jq '.[0].cnt // 0' 2>/dev/null || echo "0")
echo -e "  ${GREEN}SQL: ~${LABEL_COUNT} labeled rows${END}"

# ── Phase 3: Label quality evaluation ────────────────────────────────

echo -e "\n${BLUE}Phase 3: Label quality evaluation${END}"
kubectl delete job label-quality-eval -n "$NAMESPACE" 2>/dev/null || true
sleep 3
kubectl apply -f "$SCRIPT_DIR/119-label-quality-configmap.yaml"

# Apply with correct topic if overridden
if [ "$RATING_DEST" != "amazon-appliances-labeled" ]; then
    cat "$SCRIPT_DIR/120-label-quality-job.yaml" | \
        sed "s/value: \"amazon-appliances-labeled\"/value: \"${RATING_DEST}\"/" | \
        kubectl apply -f -
else
    kubectl apply -f "$SCRIPT_DIR/120-label-quality-job.yaml"
fi

for i in $(seq 1 120); do
    STATUS=$(kubectl get job label-quality-eval -n "$NAMESPACE" \
        -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
    FAILED=$(kubectl get job label-quality-eval -n "$NAMESPACE" \
        -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")
    if [ "$STATUS" = "True" ]; then
        echo -e "  ${GREEN}Evaluation complete${END}"
        break
    fi
    [ "$FAILED" = "True" ] && { echo -e "  ${RED}Failed${END}"; kubectl logs -n "$NAMESPACE" job/label-quality-eval --tail=30; break; }
    [ $((i % 10)) -eq 0 ] && echo -e "  ${DIM}Running... ($((i*5))s)${END}"
    sleep 5
done
kubectl logs -n "$NAMESPACE" job/label-quality-eval > "$RESULTS_DIR/rating-quality.log" 2>&1

# ── Phase 4-6: LLM labeling (optional) ──────────────────────────────

if [ "$WITH_LLM" = true ]; then
    echo -e "\n${BLUE}Phase 4: LLM-based sentiment labeling${END}"
    echo -e "  ${DIM}Source: ${SOURCE_TOPIC} → Dest: ${LLM_DEST}${END}"

    kubectl delete job sentiment-llm-labeler -n "$NAMESPACE" 2>/dev/null || true
    sleep 3
    kubectl apply -f "$SCRIPT_DIR/117-sentiment-llm-configmap.yaml"

    if [ "$SOURCE_TOPIC" != "amazon-appliances" ] || [ "$MAX_RECORDS" != "0" ]; then
        LLM_MAX="$MAX_RECORDS"
        [ "$LLM_MAX" = "0" ] && LLM_MAX="10000"  # Default to 10K for LLM
        cat "$SCRIPT_DIR/118-sentiment-llm-job.yaml" | \
            sed "s/value: \"amazon-appliances\"\$/value: \"${SOURCE_TOPIC}\"/" | \
            sed "s/value: \"amazon-appliances-llm-labeled\"/value: \"${LLM_DEST}\"/" | \
            sed "s/value: \"10000\"/value: \"${LLM_MAX}\"/" | \
            kubectl apply -f -
    else
        kubectl apply -f "$SCRIPT_DIR/118-sentiment-llm-job.yaml"
    fi

    LLM_START=$(date +%s)
    for i in $(seq 1 7200); do  # Up to 4 hours
        STATUS=$(kubectl get job sentiment-llm-labeler -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
        FAILED=$(kubectl get job sentiment-llm-labeler -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")
        if [ "$STATUS" = "True" ]; then
            echo -e "  ${GREEN}LLM labeled${END} ($(($(date +%s) - LLM_START))s)"
            break
        fi
        [ "$FAILED" = "True" ] && { echo -e "  ${RED}Failed${END}"; kubectl logs -n "$NAMESPACE" job/sentiment-llm-labeler --tail=30; break; }
        [ $((i % 60)) -eq 0 ] && echo -e "  ${DIM}$(kubectl logs -n "$NAMESPACE" job/sentiment-llm-labeler --tail=1 2>/dev/null)${END}"
        sleep 2
    done
    kubectl logs -n "$NAMESPACE" job/sentiment-llm-labeler > "$RESULTS_DIR/llm-labeler.log" 2>&1

    echo -e "\n${BLUE}Phase 5: Wait for LLM-labeled topic indexing${END}"
    LLM_TABLE=$(echo "${LLM_DEST}" | tr '-' '_')_hot
    for i in $(seq 1 300); do
        PROBE=$(kubectl exec "${CLUSTER_NAME}-1" -n "$NAMESPACE" -- \
            curl -sf http://localhost:6092/_query -H 'Content-Type: application/json' \
            -d "{\"sources\":[{\"topic\":\"$LLM_DEST\",\"modes\":[\"text\"]}],\"q\":{\"text\":\"complaint\"},\"k\":1,\"result_format\":\"merged\"}" \
            2>/dev/null || echo '{}')
        COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
        if [ "$COUNT" -gt 0 ]; then
            echo -e "  ${GREEN}LLM text index ready${END} (${i}s)"
            break
        fi
        [ $((i % 30)) -eq 0 ] && echo -e "  ${DIM}Indexing... ${i}s${END}"
        sleep 1
    done
    sleep 60

    echo -e "\n${BLUE}Phase 6: LLM label quality evaluation${END}"
    kubectl delete job label-quality-eval -n "$NAMESPACE" 2>/dev/null || true
    sleep 3
    cat "$SCRIPT_DIR/120-label-quality-job.yaml" | \
        sed "s/value: \"amazon-appliances-labeled\"/value: \"${LLM_DEST}\"/" | \
        kubectl apply -f -

    for i in $(seq 1 120); do
        STATUS=$(kubectl get job label-quality-eval -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Complete")].status}' 2>/dev/null || echo "")
        FAILED=$(kubectl get job label-quality-eval -n "$NAMESPACE" \
            -o jsonpath='{.status.conditions[?(@.type=="Failed")].status}' 2>/dev/null || echo "")
        if [ "$STATUS" = "True" ]; then
            echo -e "  ${GREEN}LLM evaluation complete${END}"
            break
        fi
        [ "$FAILED" = "True" ] && { echo -e "  ${RED}Failed${END}"; kubectl logs -n "$NAMESPACE" job/label-quality-eval --tail=30; break; }
        [ $((i % 10)) -eq 0 ] && echo -e "  ${DIM}Running... ($((i*5))s)${END}"
        sleep 5
    done
    kubectl logs -n "$NAMESPACE" job/label-quality-eval > "$RESULTS_DIR/llm-quality.log" 2>&1
else
    echo -e "\n${DIM}Phase 4-6: Skipped (use --with-llm to enable)${END}"
fi

# ── Phase 7: Collect ─────────────────────────────────────────────────

echo -e "\n${BLUE}Phase 7: Collect${END}"
for nid in 1 2 3; do
    kubectl logs "${CLUSTER_NAME}-${nid}" -n "$NAMESPACE" --tail=200 > "$RESULTS_DIR/node${nid}.log" 2>&1 || true
    kubectl exec "${CLUSTER_NAME}-${nid}" -n "$NAMESPACE" -- curl -sf "http://localhost:$((13000+nid))/metrics" > "$RESULTS_DIR/metrics-node${nid}.txt" 2>&1 || true
done

# Copy quality report from host path if available
if [ -f "/home/ubuntu/datasets/amazon/results/label-quality-report.json" ]; then
    cp "/home/ubuntu/datasets/amazon/results/label-quality-report.json" "$RESULTS_DIR/" 2>/dev/null || true
fi

# ── Phase 8: Summary ────────────────────────────────────────────────

echo -e "\n${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
echo -e "${BOLD}  DV-3: Sentiment Labeling Results${END}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"

echo -e "\n  ${CYAN}Rating-based labeling (DV-3a):${END}"
if [ -f "$RESULTS_DIR/rating-quality.log" ]; then
    grep -E '^\s*\[' "$RESULTS_DIR/rating-quality.log" 2>/dev/null || echo "  (check rating-quality.log)"
    echo ""
    grep -E 'Results:|ALL TESTS|SOME TESTS' "$RESULTS_DIR/rating-quality.log" 2>/dev/null || true
else
    echo "  (no results)"
fi

if [ "$WITH_LLM" = true ]; then
    echo -e "\n  ${CYAN}LLM-based labeling (DV-3b):${END}"
    if [ -f "$RESULTS_DIR/llm-quality.log" ]; then
        grep -E '^\s*\[' "$RESULTS_DIR/llm-quality.log" 2>/dev/null || echo "  (check llm-quality.log)"
        echo ""
        grep -E 'Results:|ALL TESTS|SOME TESTS' "$RESULTS_DIR/llm-quality.log" 2>/dev/null || true
    else
        echo "  (no results)"
    fi
fi

echo -e "\n  ${CYAN}Success Criteria:${END}"
echo -e "    All labeled records indexed and searchable"
echo -e "    Sentiment queries return relevant results"
echo -e "    SQL hot buffer accessible"
echo -e "    Search latency < 50ms p50"
echo -e "\n  Results: ${DIM}$RESULTS_DIR/${END}"
echo ""
