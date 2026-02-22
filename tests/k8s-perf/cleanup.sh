#!/usr/bin/env bash
set -euo pipefail

NAMESPACE="chronik-perf"

echo "=== Cleaning up chronik-perf ==="

echo "Deleting k6 test runs..."
kubectl delete testrun --all -n "$NAMESPACE" 2>/dev/null || true

echo "Deleting ServiceMonitors..."
kubectl delete servicemonitor --all -n "$NAMESPACE" 2>/dev/null || true

echo "Deleting Grafana dashboard..."
kubectl delete configmap chronik-perf-dashboard -n observability 2>/dev/null || true

echo "Deleting deployments..."
kubectl delete deployment --all -n "$NAMESPACE" 2>/dev/null || true

echo "Deleting services..."
kubectl delete service --all -n "$NAMESPACE" 2>/dev/null || true

echo "Deleting configmaps..."
kubectl delete configmap --all -n "$NAMESPACE" 2>/dev/null || true

echo "Deleting Chronik resources..."
kubectl delete chronikstandalone --all -n "$NAMESPACE" 2>/dev/null || true

echo "Waiting for pods to terminate..."
sleep 10

# Remove finalizers if stuck
for resource in $(kubectl get chronikstandalone -n "$NAMESPACE" -o name 2>/dev/null); do
    kubectl patch "$resource" -n "$NAMESPACE" --type=json \
        -p='[{"op": "remove", "path": "/metadata/finalizers"}]' 2>/dev/null || true
done

echo "Deleting PVCs..."
kubectl delete pvc --all -n "$NAMESPACE" 2>/dev/null || true

echo "Deleting namespace..."
kubectl delete namespace "$NAMESPACE" 2>/dev/null || true

echo "Stopping operator if running..."
pkill -f "chronik-operator" 2>/dev/null || true

echo "Cleanup complete."
