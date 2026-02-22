#!/bin/bash
# Teardown the kind cluster used for E2E tests.
#
# Usage:
#   ./tests/e2e/operator/teardown-kind.sh

set -euo pipefail

CLUSTER_NAME="${KIND_CLUSTER:-chronik-operator-test}"

echo "=== Stopping operator ==="
if [ -f /tmp/chronik-operator-e2e.pid ]; then
    PID=$(cat /tmp/chronik-operator-e2e.pid)
    kill "$PID" 2>/dev/null || true
    rm -f /tmp/chronik-operator-e2e.pid
    echo "Stopped operator (PID $PID)"
fi

echo "=== Deleting kind cluster: $CLUSTER_NAME ==="
kind delete cluster --name "$CLUSTER_NAME"

echo "=== Cleanup complete ==="
