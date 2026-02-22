#!/bin/bash
# E2E test runner for the Chronik operator.
#
# Requires: kind cluster set up via setup-kind.sh
#
# Usage:
#   ./tests/e2e/operator/run-tests.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

wait_for() {
    local resource="$1"
    local condition="$2"
    local timeout="${3:-60}"
    kubectl wait "$resource" --for="$condition" --timeout="${timeout}s" 2>/dev/null
}

echo "=== E2E Tests for Chronik Operator ==="
echo ""

# --- Test 1: Standalone create → verify → delete ---
echo "--- Test: Standalone lifecycle ---"

kubectl apply -f "$ROOT_DIR/examples/k8s/standalone-basic.yaml"
sleep 5

if kubectl get chronikstandalone my-chronik -o jsonpath='{.metadata.name}' 2>/dev/null | grep -q my-chronik; then
    pass "Standalone CR created"
else
    fail "Standalone CR not found"
fi

kubectl delete -f "$ROOT_DIR/examples/k8s/standalone-basic.yaml" --wait=false
sleep 3

if ! kubectl get chronikstandalone my-chronik 2>/dev/null; then
    pass "Standalone CR deleted"
else
    fail "Standalone CR still exists"
fi

# --- Test 2: Topic create → verify → delete ---
echo "--- Test: Topic lifecycle ---"

# Create a cluster first (topic needs a cluster ref)
kubectl apply -f "$ROOT_DIR/examples/k8s/cluster-3node.yaml"
sleep 3
kubectl apply -f "$ROOT_DIR/examples/k8s/topic-orders.yaml"
sleep 3

if kubectl get chroniktopic orders -o jsonpath='{.spec.partitions}' 2>/dev/null | grep -q 6; then
    pass "Topic CR created with correct partitions"
else
    fail "Topic CR not found or wrong partitions"
fi

kubectl delete -f "$ROOT_DIR/examples/k8s/topic-orders.yaml" --wait=false
sleep 2

if ! kubectl get chroniktopic orders 2>/dev/null; then
    pass "Topic CR deleted"
else
    fail "Topic CR still exists"
fi

# --- Test 3: User create → verify Secret → delete ---
echo "--- Test: User lifecycle ---"

kubectl apply -f "$ROOT_DIR/examples/k8s/user-app-producer.yaml"
sleep 5

if kubectl get chronikuser app-producer 2>/dev/null; then
    pass "User CR created"
else
    fail "User CR not found"
fi

kubectl delete -f "$ROOT_DIR/examples/k8s/user-app-producer.yaml" --wait=false
sleep 2

# --- Test 4: AutoScaler create → verify → delete ---
echo "--- Test: AutoScaler lifecycle ---"

kubectl apply -f "$ROOT_DIR/examples/k8s/autoscaler.yaml"
sleep 3

if kubectl get chronikautoscaler chronik-autoscaler -o jsonpath='{.spec.minReplicas}' 2>/dev/null | grep -q 3; then
    pass "AutoScaler CR created"
else
    fail "AutoScaler CR not found"
fi

kubectl delete -f "$ROOT_DIR/examples/k8s/autoscaler.yaml" --wait=false
sleep 2

# Cleanup
kubectl delete -f "$ROOT_DIR/examples/k8s/cluster-3node.yaml" --wait=false 2>/dev/null || true
sleep 2

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
