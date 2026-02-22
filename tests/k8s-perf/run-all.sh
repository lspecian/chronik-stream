#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
NAMESPACE="chronik-perf"
RESULTS_DIR="$SCRIPT_DIR/results/$(date +%Y%m%d-%H%M%S)"
CTR="sudo /snap/microk8s/current/bin/ctr -n k8s.io -a /var/snap/microk8s/common/run/containerd.sock"
NODES="dell-1 dell-2 dell-3"
MAX_LOAD=false

# Parse flags
for arg in "$@"; do
    case $arg in
        --max-load) MAX_LOAD=true ;;
        *) echo "Unknown flag: $arg"; exit 1 ;;
    esac
done

if [ "$MAX_LOAD" = true ]; then
    echo "=== Chronik K8s MAX LOAD Performance Test ==="
else
    echo "=== Chronik K8s Performance Test Suite ==="
fi
echo "Results: $RESULTS_DIR"
mkdir -p "$RESULTS_DIR"

# ── Phase 0: Build ──────────────────────────────────────────────────
echo ""
echo "--- Phase 0: Build chronik-perf ---"
cd "$REPO_ROOT"
cargo build --release -p chronik-perf

TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT
cp "$REPO_ROOT/target/release/chronik-perf" "$TMPDIR/"
cp "$SCRIPT_DIR/Dockerfile" "$TMPDIR/"

docker build -t chronik-perf:latest "$TMPDIR/"
docker save chronik-perf:latest -o "$TMPDIR/chronik-perf.tar"

echo "Importing image to cluster nodes..."
for node in $NODES; do
    echo "  → $node"
    scp -q "$TMPDIR/chronik-perf.tar" "$node:/tmp/"
    ssh "$node" "$CTR images import /tmp/chronik-perf.tar && rm /tmp/chronik-perf.tar"
done
echo "Image imported to all nodes."

# ── Phase 1: Deploy Chronik ─────────────────────────────────────────
echo ""
echo "--- Phase 1: Deploy Chronik server ---"

# Start operator if not running
OPERATOR_PID=""
if ! pgrep -f "chronik-operator" >/dev/null 2>&1; then
    echo "Starting operator..."
    "$REPO_ROOT/target/release/chronik-operator" start --leader-election=false \
        &>"$RESULTS_DIR/operator.log" &
    OPERATOR_PID=$!
    echo "  Operator PID: $OPERATOR_PID"
    sleep 3
fi

kubectl apply -f "$SCRIPT_DIR/00-namespace.yaml"
kubectl apply -f "$SCRIPT_DIR/01-chronik-standalone.yaml"

echo "Waiting for Chronik pod..."
for i in $(seq 1 60); do
    PHASE=$(kubectl get pod chronik-bench -n "$NAMESPACE" -o jsonpath='{.status.phase}' 2>/dev/null || echo "")
    READY=$(kubectl get pod chronik-bench -n "$NAMESPACE" -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null || echo "")
    if [ "$PHASE" = "Running" ] && [ "$READY" = "true" ]; then
        echo "  Chronik ready after ~$((i*3))s"
        break
    fi
    sleep 3
done

# ── Phase 2: Deploy Ingestor ────────────────────────────────────────
echo ""
echo "--- Phase 2: Deploy Ingestor (6 replicas) ---"
kubectl apply -f "$SCRIPT_DIR/10-ingestor-deployment.yaml"
kubectl apply -f "$SCRIPT_DIR/11-ingestor-service.yaml"

echo "Waiting for ingestor pods..."
kubectl rollout status deployment/ingestor -n "$NAMESPACE" --timeout=120s

# ── Phase 3: Deploy Consumer ────────────────────────────────────────
echo ""
echo "--- Phase 3: Deploy Consumer (3 replicas) ---"
kubectl apply -f "$SCRIPT_DIR/20-consumer-deployment.yaml"
kubectl apply -f "$SCRIPT_DIR/21-consumer-service.yaml"

echo "Waiting for consumer pods..."
kubectl rollout status deployment/consumer -n "$NAMESPACE" --timeout=120s

# ── Phase 4: Deploy Monitoring ─────────────────────────────────────
echo ""
echo "--- Phase 4: Deploy Monitoring (Prometheus + Grafana) ---"

# Metrics service for Chronik
kubectl apply -f "$SCRIPT_DIR/02-chronik-metrics-service.yaml"

# ServiceMonitors
kubectl apply -f "$SCRIPT_DIR/40-servicemonitor-chronik.yaml"
kubectl apply -f "$SCRIPT_DIR/41-servicemonitor-ingestor.yaml"
kubectl apply -f "$SCRIPT_DIR/42-servicemonitor-consumer.yaml"

# Grafana dashboard
kubectl apply -f "$SCRIPT_DIR/50-grafana-dashboard-configmap.yaml"

echo "  ServiceMonitors created (Prometheus will discover in ~30s)"
echo "  Grafana dashboard deployed"

# Get Grafana URL
GRAFANA_IP=$(kubectl get svc kube-prom-stack-grafana -n observability -o jsonpath='{.spec.clusterIP}' 2>/dev/null || echo "")
if [ -n "$GRAFANA_IP" ]; then
    echo "  Grafana: http://$GRAFANA_IP/d/chronik-perf-load-test (admin:prom-operator)"
fi

# ── Phase 5: Run k6 Load Test ───────────────────────────────────────
echo ""
if [ "$MAX_LOAD" = true ]; then
    echo "--- Phase 5: Run k6 MAX LOAD test (~9.5 min, 6 pods, up to 5000 VUs) ---"
    kubectl apply -f "$SCRIPT_DIR/33-k6-max-load-configmap.yaml"
    kubectl apply -f "$SCRIPT_DIR/32-k6-max-load-test.yaml"
    K6_NAME="k6-chronik-max-load"
    K6_LABEL="k6_cr=k6-chronik-max-load"
    WAIT_ITERS=180  # 15 min max wait
else
    echo "--- Phase 5: Run k6 load test (~4 min) ---"
    kubectl apply -f "$SCRIPT_DIR/30-k6-configmap.yaml"
    kubectl apply -f "$SCRIPT_DIR/31-k6-load-test.yaml"
    K6_NAME="k6-chronik-load"
    K6_LABEL="k6_cr=k6-chronik-load"
    WAIT_ITERS=120
fi

echo "Waiting for k6 test to complete..."
for i in $(seq 1 $WAIT_ITERS); do
    STAGE=$(kubectl get testrun "$K6_NAME" -n "$NAMESPACE" -o jsonpath='{.status.stage}' 2>/dev/null || echo "")
    if [ "$STAGE" = "finished" ] || [ "$STAGE" = "error" ]; then
        echo "  k6 test $STAGE after ~$((i*5))s"
        break
    fi
    sleep 5
done

# ── Phase 6: Collect Results ────────────────────────────────────────
echo ""
echo "--- Phase 6: Collect results ---"

# k6 logs
kubectl logs -n "$NAMESPACE" -l "$K6_LABEL" --tail=-1 \
    > "$RESULTS_DIR/k6-output.log" 2>&1 || true

# Collect metrics from all ingestor pods via port-forward
echo "Collecting ingestor metrics..."
for pod in $(kubectl get pods -n "$NAMESPACE" -l app=ingestor -o jsonpath='{.items[*].metadata.name}' 2>/dev/null); do
    kubectl port-forward -n "$NAMESPACE" "$pod" 18080:8080 &>/dev/null &
    PF_PID=$!
    sleep 1
    curl -s http://localhost:18080/metrics >> "$RESULTS_DIR/ingestor-metrics.json" 2>/dev/null || true
    echo "" >> "$RESULTS_DIR/ingestor-metrics.json"
    kill $PF_PID 2>/dev/null || true
    wait $PF_PID 2>/dev/null || true
done

# Collect metrics from all consumer pods via port-forward
echo "Collecting consumer metrics..."
for pod in $(kubectl get pods -n "$NAMESPACE" -l app=consumer -o jsonpath='{.items[*].metadata.name}' 2>/dev/null); do
    kubectl port-forward -n "$NAMESPACE" "$pod" 18081:8081 &>/dev/null &
    PF_PID=$!
    sleep 1
    curl -s http://localhost:18081/metrics >> "$RESULTS_DIR/consumer-metrics.json" 2>/dev/null || true
    echo "" >> "$RESULTS_DIR/consumer-metrics.json"
    kill $PF_PID 2>/dev/null || true
    wait $PF_PID 2>/dev/null || true
done

# Ingestor logs
kubectl logs -n "$NAMESPACE" -l app=ingestor --tail=100 \
    > "$RESULTS_DIR/ingestor.log" 2>&1 || true

# Consumer logs
kubectl logs -n "$NAMESPACE" -l app=consumer --tail=100 \
    > "$RESULTS_DIR/consumer.log" 2>&1 || true

# Chronik server logs
kubectl logs -n "$NAMESPACE" chronik-bench --tail=200 \
    > "$RESULTS_DIR/chronik-server.log" 2>&1 || true

# ── Phase 7: Summary ────────────────────────────────────────────────
echo ""
echo "=== Test Complete ==="
echo ""
echo "Results:"
ls -lh "$RESULTS_DIR/"
echo ""

if [ -f "$RESULTS_DIR/consumer-metrics.json" ]; then
    echo "Consumer metrics:"
    cat "$RESULTS_DIR/consumer-metrics.json"
    echo ""
fi

if [ -f "$RESULTS_DIR/ingestor-metrics.json" ]; then
    echo "Ingestor metrics:"
    cat "$RESULTS_DIR/ingestor-metrics.json"
    echo ""
fi

echo "k6 summary (last 50 lines):"
tail -50 "$RESULTS_DIR/k6-output.log" 2>/dev/null || echo "(no k6 output)"
echo ""

if [ -n "${GRAFANA_IP:-}" ]; then
    echo "Grafana dashboard: http://$GRAFANA_IP/d/chronik-perf-load-test"
    echo "  credentials: admin / prom-operator"
    echo ""
fi

echo "To clean up: $SCRIPT_DIR/cleanup.sh"
