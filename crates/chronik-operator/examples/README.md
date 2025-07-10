# ChronikCluster Examples

This directory contains example YAML manifests for deploying Chronik Stream clusters using the Kubernetes operator.

## Examples

### basic-cluster.yaml
A basic cluster configuration with default settings suitable for development and testing environments.

- 3 controllers, 3 ingest nodes, 3 search nodes
- Local storage backend
- Default resource allocations
- Minimal configuration

### minimal-cluster.yaml
The smallest possible cluster configuration for local development or testing.

- 1 controller, 1 ingest node, no search nodes
- Local storage with minimal size
- Suitable for single-node testing

### production-cluster.yaml
A comprehensive production-ready configuration with all available features enabled.

- High availability with 5 controllers
- Autoscaling enabled for all components
- S3 storage backend
- TLS and security hardening
- Monitoring and observability
- Pod disruption budgets
- Affinity and anti-affinity rules

### cloud-storage-cluster.yaml
Example configuration using cloud storage (AWS S3) with moderate resource requirements.

- AWS S3 storage backend
- TiKV distributed metadata storage
- Balanced resource allocation
- Basic monitoring enabled

## Usage

1. Create a namespace for Chronik Stream:
```bash
kubectl create namespace chronik-stream
```

2. Create the necessary secrets (adjust as needed):
```bash
# For cloud storage examples, create cloud credentials
kubectl create secret generic aws-credentials \
  --from-literal=access-key-id=your-access-key \
  --from-literal=secret-access-key=your-secret-key \
  -n chronik-stream
```

3. Apply the ChronikCluster manifest:
```bash
kubectl apply -f basic-cluster.yaml
```

4. Check the cluster status:
```bash
kubectl get chronikclusters -n chronik-stream
kubectl describe chronikcluster basic-cluster -n chronik-stream
```

## Customization

You can customize these examples by:

- Adjusting resource requests and limits
- Changing storage backend and size
- Modifying replica counts
- Adding custom annotations and labels
- Configuring autoscaling parameters
- Setting up monitoring and security features

Refer to the CRD schema documentation for all available configuration options.