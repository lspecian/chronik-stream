# Chronik Stream Kubernetes Operator

The Chronik Stream Operator manages ChronikCluster resources in Kubernetes, automating the deployment and lifecycle management of Chronik Stream clusters.

## Features

- **Custom Resource Definition (CRD)**: Define Chronik clusters declaratively
- **StatefulSet Management**: Deploy controller, ingest, and search nodes as StatefulSets
- **Service Discovery**: Automatic service creation for node communication
- **Configuration Management**: ConfigMaps for cluster configuration
- **Secret Management**: Integration with Kubernetes secrets for credentials
- **Autoscaling**: HorizontalPodAutoscaler support for dynamic scaling
- **High Availability**: PodDisruptionBudget for maintaining availability
- **Security**: Pod and container security contexts, TLS support
- **Monitoring**: Prometheus metrics and OpenTelemetry tracing
- **Resource Management**: CPU and memory limits/requests
- **Storage**: Support for S3, GCS, Azure, and local storage backends
- **Scheduling**: Node selectors, tolerations, and affinity rules

## Installation

### Prerequisites

- Kubernetes 1.24+
- kubectl configured to access your cluster
- Appropriate RBAC permissions

### Deploy the Operator

```bash
# Deploy using kubectl
kubectl apply -f deploy/operator/operator.yaml

# Or using Kustomize
kubectl apply -k deploy/operator/

# Verify the operator is running
kubectl -n chronik-operator-system get pods
```

## Usage

### Basic Cluster

Create a basic Chronik cluster:

```yaml
apiVersion: chronik.stream/v1alpha1
kind: ChronikCluster
metadata:
  name: my-chronik
  namespace: default
spec:
  controllers: 3
  ingestNodes: 2
  storage:
    backend:
      local: {}
    size: 10Gi
  metastore:
    database: Postgres
    connection:
      host: postgres.default.svc.cluster.local
      port: 5432
      database: chronik
      credentialsSecret: postgres-credentials
```

Apply the manifest:

```bash
kubectl apply -f my-cluster.yaml
```

### Production Cluster

For production deployments, use the comprehensive example:

```bash
kubectl apply -f deploy/operator/examples/production-cluster.yaml
```

## Configuration

### Storage Backends

#### S3
```yaml
storage:
  backend:
    s3:
      bucket: my-bucket
      region: us-east-1
      endpoint: null  # Optional custom endpoint
```

#### Google Cloud Storage
```yaml
storage:
  backend:
    gcs:
      bucket: my-bucket
```

#### Azure Blob Storage
```yaml
storage:
  backend:
    azure:
      container: my-container
      account: mystorageaccount
```

#### Local Storage
```yaml
storage:
  backend:
    local: {}
```

### Autoscaling

Enable autoscaling for components:

```yaml
autoscaling:
  controllers:
    minReplicas: 3
    maxReplicas: 10
    targetCpuUtilizationPercentage: 80
    targetMemoryUtilizationPercentage: 80
  ingestNodes:
    minReplicas: 2
    maxReplicas: 20
    targetCpuUtilizationPercentage: 70
```

### Security

Configure security settings:

```yaml
security:
  tlsEnabled: true
  tlsSecret: chronik-tls
  podSecurityContext:
    runAsUser: 1000
    runAsNonRoot: true
  containerSecurityContext:
    allowPrivilegeEscalation: false
    readOnlyRootFilesystem: true
```

### Monitoring

Enable monitoring integrations:

```yaml
monitoring:
  prometheus: true
  tracing: true
  otlpEndpoint: http://otel-collector:4317
```

## Status

The operator updates the ChronikCluster status with:

- **Phase**: Current state (Pending, Creating, Running, Updating, Failed, Deleting)
- **Endpoints**: Service endpoints for all components
- **Conditions**: Detailed status conditions
- **Ready Replicas**: Number of ready pods per component

Check cluster status:

```bash
kubectl get chronikclusters
kubectl describe chronikcluster my-chronik
```

## Troubleshooting

### Operator Logs
```bash
kubectl -n chronik-operator-system logs deployment/chronik-operator
```

### Cluster Events
```bash
kubectl describe chronikcluster my-chronik
kubectl get events --field-selector involvedObject.name=my-chronik
```

### Pod Status
```bash
kubectl get pods -l chronik.stream/cluster=my-chronik
kubectl describe pod <pod-name>
```

## Development

### Building
```bash
cargo build --release --bin chronik-operator
```

### Running Tests
```bash
cargo test -p chronik-operator
```

### Running Locally
```bash
# Set KUBECONFIG if needed
export KUBECONFIG=~/.kube/config

# Run the operator
RUST_LOG=debug cargo run --bin chronik-operator
```

## Architecture

The operator follows the Kubernetes controller pattern:

1. **Watch**: Monitor ChronikCluster resources for changes
2. **Reconcile**: Ensure actual state matches desired state
3. **Update**: Update resource status and create/update child resources

### Components Created

For each ChronikCluster, the operator creates:

- **StatefulSets**: For controller, ingest, and search nodes
- **Services**: Headless services for StatefulSets and load balancer for ingress
- **ConfigMaps**: Cluster configuration
- **ServiceAccounts**: For pod identity
- **PodDisruptionBudgets**: For high availability
- **HorizontalPodAutoscalers**: For autoscaling

### Finalizers

The operator uses finalizers to ensure proper cleanup when a ChronikCluster is deleted.

## Contributing

See the main project CONTRIBUTING.md for guidelines.

## License

Same as the Chronik Stream project.