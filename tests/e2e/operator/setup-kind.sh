#!/bin/bash
# E2E test setup: Create a kind cluster and install the operator.
#
# Prerequisites:
#   - kind (https://kind.sigs.k8s.io/)
#   - kubectl
#   - helm (optional, for Helm-based install)
#   - Docker
#
# Usage:
#   ./tests/e2e/operator/setup-kind.sh
#   ./tests/e2e/operator/run-tests.sh
#   ./tests/e2e/operator/teardown-kind.sh

set -euo pipefail

CLUSTER_NAME="${KIND_CLUSTER:-chronik-operator-test}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

echo "=== Creating kind cluster: $CLUSTER_NAME ==="
kind create cluster --name "$CLUSTER_NAME" --wait 60s

echo "=== Building operator binary ==="
cd "$ROOT_DIR"
cargo build --release --bin chronik-operator

echo "=== Generating and applying CRDs ==="
cargo run --release --bin chronik-operator -- crd-gen | kubectl apply -f -

echo "=== Verifying CRDs are installed ==="
kubectl get crd chronikstandalones.chronik.io
kubectl get crd chronikclusters.chronik.io
kubectl get crd chroniktopics.chronik.io
kubectl get crd chronikusers.chronik.io
kubectl get crd chronikautoscalers.chronik.io

echo "=== Starting operator in background ==="
RUST_LOG=info cargo run --release --bin chronik-operator -- start \
    --leader-election=false \
    --log-level=debug \
    &
OPERATOR_PID=$!
echo "Operator PID: $OPERATOR_PID"
echo "$OPERATOR_PID" > /tmp/chronik-operator-e2e.pid

# Wait for operator to connect
sleep 5

echo "=== Kind cluster ready for E2E tests ==="
echo "Run: ./tests/e2e/operator/run-tests.sh"
