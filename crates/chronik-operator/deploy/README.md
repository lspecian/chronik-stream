# Chronik Operator Deployment

This directory contains Kubernetes manifests for deploying the Chronik Stream operator.

## Files

- `namespace.yaml` - Creates the chronik-operator-system namespace
- `rbac.yaml` - Service account, cluster role, and cluster role binding
- `deployment.yaml` - Operator deployment with security context and health checks
- `service.yaml` - Service for metrics and health endpoints
- `crd.yaml` - ChronikCluster Custom Resource Definition (auto-generated)
- `kustomization.yaml` - Kustomize configuration for easy deployment

## Quick Start

### Option 1: Direct kubectl apply

```bash
# Install in order
kubectl apply -f namespace.yaml
kubectl apply -f crd.yaml
kubectl apply -f rbac.yaml
kubectl apply -f service.yaml
kubectl apply -f deployment.yaml
```

### Option 2: Using Kustomize

```bash
# Apply all manifests with kustomize
kubectl apply -k .
```

### Option 3: Using operator binary

```bash
# The operator can install its own CRDs
./chronik-operator --install-crds
```

## Verification

Check that the operator is running:

```bash
kubectl get pods -n chronik-operator-system
kubectl logs -n chronik-operator-system deployment/chronik-operator
```

Check that the CRD is installed:

```bash
kubectl get crd chronikclusters.chronik.stream
kubectl api-resources | grep chronik
```

## Monitoring

The operator exposes metrics on port 8080 and health checks on port 8081:

```bash
# Port forward to access metrics
kubectl port-forward -n chronik-operator-system svc/chronik-operator-metrics 8080:8080

# Access metrics
curl http://localhost:8080/metrics

# Health check
curl http://localhost:8080/health
```

## Configuration

The operator can be configured via environment variables in the deployment:

- `RUST_LOG` - Log level (default: info)
- `OPERATOR_NAMESPACE` - Operator namespace (auto-detected)
- `OPERATOR_NAME` - Operator name (default: chronik-operator)

## Security

The operator runs with minimal privileges:

- Non-root user (UID 65534)
- Read-only root filesystem
- No privilege escalation
- Dropped capabilities
- Security context enforcement

## Troubleshooting

### Operator not starting

Check logs:
```bash
kubectl logs -n chronik-operator-system deployment/chronik-operator
```

Common issues:
- RBAC permissions not properly configured
- CRD installation failed
- Image pull failures

### CRD not found

Manually install CRDs:
```bash
kubectl apply -f crd.yaml
```

Or regenerate from source:
```bash
cd /path/to/chronik-operator
cargo run --bin crd-gen > deploy/crd.yaml
```

### Permission denied errors

Check RBAC configuration:
```bash
kubectl auth can-i create chronikclusters --as=system:serviceaccount:chronik-operator-system:chronik-operator
kubectl describe clusterrole chronik-operator
kubectl describe clusterrolebinding chronik-operator
```