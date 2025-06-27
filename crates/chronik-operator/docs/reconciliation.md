# Chronik Operator Reconciliation Logic

This document describes the enhanced reconciliation logic implemented for the Chronik Stream Kubernetes operator.

## Overview

The reconciliation logic is the core of the Kubernetes operator, responsible for ensuring that the desired state (as defined in ChronikCluster custom resources) matches the actual state in the cluster.

## Architecture

### Controller Flow

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   Watch Events  │───▶│   Reconciler    │───▶│  Update Status  │
│                 │    │                 │    │                 │
│ - Create        │    │ - Validate      │    │ - Phase         │
│ - Update        │    │ - Create/Update │    │ - Conditions    │
│ - Delete        │    │ - Monitor       │    │ - Endpoints     │
└─────────────────┘    └─────────────────┘    └─────────────────┘
```

### Component Structure

- **Controller**: Event loop and error handling
- **Reconciler**: Core reconciliation logic  
- **ResourceGenerator**: Kubernetes resource templates
- **Finalizer**: Cleanup handling

## Key Features

### 1. Comprehensive Validation

The reconciler validates cluster specifications before creating any resources:

```rust
fn validate_cluster_spec(&self, cluster: &ChronikCluster) -> Result<(), String> {
    // Validate controller count (odd numbers recommended)
    // Validate ingest/search node counts
    // Validate storage configuration
    // Validate resource requirements
    // Validate autoscaling configuration
}
```

**Validation Rules:**
- Controller count must be positive (warns if even number)
- Ingest node count must be positive
- Search node count cannot be negative
- Storage backend must have proper configuration
- Resource requests must be valid Kubernetes formats
- Autoscaling min/max replicas must be logical

### 2. Structured Error Handling

The operator implements sophisticated error handling with proper classification:

**Error Types:**
- **Transient Errors** (HTTP 5xx, network issues): Short retry interval (30-60s)
- **Permanent Errors** (HTTP 4xx, validation): Long retry interval (5-10min)
- **Unknown Errors**: Medium retry interval (2min)

**Error Reporting:**
- Structured logging with tracing spans
- Status updates with error conditions
- Prometheus metrics (when enabled)

### 3. Resource Management

The reconciler manages Kubernetes resources in dependency order:

1. **RBAC Resources** (ServiceAccounts, Roles, RoleBindings)
2. **ConfigMaps** (Configuration data)
3. **StatefulSets** (Controllers, Ingest nodes)
4. **Services** (Network exposure)
5. **Deployments** (Search nodes, if enabled)
6. **PodDisruptionBudgets** (High availability)
7. **HorizontalPodAutoscalers** (Autoscaling)

### 4. Status Tracking

The operator maintains comprehensive status information:

```yaml
status:
  phase: Running
  observedGeneration: 2
  lastUpdated: "2024-01-01T00:00:00Z"
  controllers:
    - "cluster-controller-0.cluster-controller.namespace.svc.cluster.local:9090"
    - "cluster-controller-1.cluster-controller.namespace.svc.cluster.local:9090"
  ingestNodes:
    - "cluster-ingest-0.cluster-ingest.namespace.svc.cluster.local:9092"
  searchNodes:
    - "cluster-search-0.cluster-search.namespace.svc.cluster.local:9093"
  readyReplicas:
    controllers: 3
    ingestNodes: 3
    searchNodes: 2
  conditions:
    - type: "Ready"
      status: "True"
      lastTransitionTime: "2024-01-01T00:00:00Z"
      reason: "AllComponentsReady"
      message: "All Chronik Stream components are running"
```

### 5. Health and Observability

The operator includes built-in health checks and metrics:

**Health Endpoints:**
- `/health` - Basic health check
- `/ready` - Readiness probe
- `/metrics` - Prometheus metrics

**Observability Features:**
- Structured logging with correlation IDs
- Distributed tracing spans
- Performance metrics (reconciliation duration)
- Error rate tracking

## Reconciliation Phases

### 1. Pending
Initial state when cluster is first created.

### 2. Creating
Resources are being created for the first time.

### 3. Running
All components are healthy and operational.

### 4. Updating
Spec has changed and resources are being updated.

### 5. Failed
Reconciliation encountered a permanent error.

### 6. Deleting
Cluster is being deleted (finalizer cleanup).

## Resource Generation

The operator uses a template-based approach for generating Kubernetes resources:

```rust
// Example: StatefulSet generation
pub fn controller_stateful_set(&self) -> StatefulSet {
    StatefulSet {
        metadata: ObjectMeta {
            name: Some(format!("{}-controller", self.cluster_name)),
            namespace: Some(self.namespace.clone()),
            owner_references: Some(vec![self.owner_ref.clone()]),
            labels: Some(self.common_labels()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            replicas: Some(self.cluster.spec.controllers),
            selector: LabelSelector {
                match_labels: Some(self.controller_labels()),
            },
            template: self.controller_pod_template(),
            // ... additional configuration
        }),
        ..Default::default()
    }
}
```

## Testing Strategy

### Integration Tests

1. **Full Reconciliation Flow**
   - Create cluster → verify resources → update spec → verify changes

2. **Error Handling**
   - Invalid specs → verify validation errors
   - Network failures → verify retry behavior

3. **Idempotency**
   - Multiple reconciliations → verify no duplicate resources

### Unit Tests

1. **Validation Logic**
   - Test all validation rules
   - Edge cases and boundary conditions

2. **Resource Generation**
   - Verify template correctness
   - Test different configurations

3. **Status Management**
   - Phase transitions
   - Condition updates

## Configuration

### Environment Variables

- `RUST_LOG` - Log level configuration
- `OPERATOR_NAMESPACE` - Operator's own namespace
- `OPERATOR_NAME` - Operator instance name

### Operator Configuration

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: chronik-operator
spec:
  template:
    spec:
      containers:
      - name: operator
        env:
        - name: RUST_LOG
          value: "info,chronik_operator=debug"
        ports:
        - name: http-metrics
          containerPort: 8080
        - name: http-health
          containerPort: 8081
        livenessProbe:
          httpGet:
            path: /health
            port: http-health
        readinessProbe:
          httpGet:
            path: /ready
            port: http-health
```

## Best Practices

### 1. Resource Ownership
- All created resources have owner references
- Kubernetes handles garbage collection automatically
- Finalizers ensure proper cleanup

### 2. Idempotency
- Reconciliation can be run multiple times safely
- Uses server-side apply for updates
- Handles concurrent modifications

### 3. Performance
- Efficient resource queries with field selectors
- Batched status updates
- Minimal API calls

### 4. Security
- Minimal RBAC permissions
- Non-root container execution
- Read-only filesystem
- Security contexts enforced

## Troubleshooting

### Common Issues

1. **CRD Not Found**
   ```
   Error: Custom resource definition not found
   Solution: Ensure CRDs are installed before starting operator
   ```

2. **Permission Denied**
   ```
   Error: Forbidden: User cannot create statefulsets
   Solution: Check RBAC configuration
   ```

3. **Resource Conflicts**
   ```
   Error: StatefulSet already exists
   Solution: Check for existing resources with same name
   ```

### Debugging

1. **Check operator logs**
   ```bash
   kubectl logs -n chronik-operator-system deployment/chronik-operator
   ```

2. **Check cluster status**
   ```bash
   kubectl describe chronikcluster my-cluster
   ```

3. **Verify resources**
   ```bash
   kubectl get all -l app.kubernetes.io/managed-by=chronik-operator
   ```

## Future Enhancements

1. **Advanced Scheduling**
   - Pod topology spread constraints
   - Node affinity optimization

2. **Rolling Updates**
   - Blue-green deployments
   - Canary releases

3. **Backup Integration**
   - Automated backup scheduling
   - Point-in-time recovery

4. **Multi-cluster Support**
   - Cross-cluster replication
   - Federated management