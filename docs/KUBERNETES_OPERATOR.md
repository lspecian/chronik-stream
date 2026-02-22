# Chronik Kubernetes Operator — User Guide

The Chronik Operator manages Chronik Stream deployments on Kubernetes, providing declarative CRDs for standalone instances, Raft clusters, topics, users, and autoscaling.

## Table of Contents

1. [Installation](#installation)
2. [CRD Reference](#crd-reference)
3. [Standalone Deployments](#standalone-deployments)
4. [Cluster Deployments](#cluster-deployments)
5. [Topic Management](#topic-management)
6. [User & ACL Management](#user--acl-management)
7. [Autoscaling](#autoscaling)
8. [Monitoring](#monitoring)
9. [Troubleshooting](#troubleshooting)

---

## Installation

### Prerequisites

- Kubernetes 1.26+
- Helm 3.x (for Helm installation)

### Using Helm

```bash
helm install chronik-operator charts/chronik-operator/ \
  --namespace chronik-system \
  --create-namespace
```

### Custom Values

```bash
helm install chronik-operator charts/chronik-operator/ \
  --namespace chronik-system \
  --create-namespace \
  --set replicaCount=2 \
  --set leaderElection.enabled=true \
  --set logLevel=debug
```

### Manual CRD Installation

```bash
# Generate and apply CRDs
cargo run --bin chronik-operator -- crd-gen | kubectl apply -f -

# Run the operator
cargo run --bin chronik-operator -- start
```

---

## CRD Reference

| CRD | API Group | Kind | Short Name | Scope |
|-----|-----------|------|------------|-------|
| Standalone | chronik.io/v1alpha1 | ChronikStandalone | csa | Namespaced |
| Cluster | chronik.io/v1alpha1 | ChronikCluster | ccl | Namespaced |
| Topic | chronik.io/v1alpha1 | ChronikTopic | ctopic | Namespaced |
| User | chronik.io/v1alpha1 | ChronikUser | cuser | Namespaced |
| AutoScaler | chronik.io/v1alpha1 | ChronikAutoScaler | cas | Namespaced |

### Listing Resources

```bash
kubectl get chronikstandalones   # or: kubectl get csa
kubectl get chronikclusters      # or: kubectl get ccl
kubectl get chroniktopics        # or: kubectl get ctopic
kubectl get chronikusers         # or: kubectl get cuser
kubectl get chronikautoscalers   # or: kubectl get cas
```

---

## Standalone Deployments

A ChronikStandalone creates a single-node Chronik deployment with:
- 1 Pod running `chronik-server start`
- 1 PersistentVolumeClaim for data
- 1 Service for Kafka client access
- 1 ConfigMap (if needed)

### Basic Example

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikStandalone
metadata:
  name: my-chronik
spec:
  image: ghcr.io/chronik-stream/chronik-server:latest
  resources:
    requests:
      cpu: "500m"
      memory: "1Gi"
    limits:
      cpu: "2"
      memory: "4Gi"
  storage:
    size: 50Gi
```

### With S3 Tiered Storage

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikStandalone
metadata:
  name: chronik-s3
spec:
  image: ghcr.io/chronik-stream/chronik-server:latest
  storage:
    size: 100Gi
    storageClass: gp3
  objectStore:
    backend: s3
    bucket: my-chronik-data
    region: us-east-1
    credentialsSecret: chronik-s3-credentials
  columnarEnabled: true
  vectorSearch:
    enabled: true
    provider: openai
    model: text-embedding-3-small
    apiKeySecret:
      name: chronik-openai-key
      key: api-key
```

### Key Fields

| Field | Default | Description |
|-------|---------|-------------|
| `image` | `ghcr.io/chronik-stream/chronik-server:latest` | Container image |
| `storage.size` | `50Gi` | PVC size |
| `storage.storageClass` | `standard` | Storage class |
| `kafkaPort` | `9092` | Kafka listener port |
| `unifiedApiPort` | `6092` | Unified API port |
| `walProfile` | `medium` | WAL commit profile |
| `produceProfile` | `balanced` | Produce flush profile |
| `serviceType` | `ClusterIP` | Service type (ClusterIP, NodePort, LoadBalancer) |
| `objectStore` | none | S3/GCS/Azure tiered storage |
| `columnarEnabled` | `false` | Enable columnar storage |
| `vectorSearch` | none | Vector search configuration |

---

## Cluster Deployments

A ChronikCluster creates a multi-node Raft cluster with:
- N Pods (one per node, minimum 3)
- N PersistentVolumeClaims
- N per-node Services (for admin API)
- N ConfigMaps (auto-generated TOML configs)
- 1 Headless Service (for DNS-based peer discovery)
- 1 PodDisruptionBudget

### 3-Node Cluster

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikCluster
metadata:
  name: chronik-cluster
spec:
  replicas: 3
  storage:
    size: 100Gi
  adminApiKeySecret:
    name: chronik-admin-key
    key: api-key
```

**Required secret:**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: chronik-admin-key
type: Opaque
stringData:
  api-key: "your-strong-random-key"
```

### Key Fields

| Field | Default | Description |
|-------|---------|-------------|
| `replicas` | `3` | Number of nodes (minimum 3 for Raft quorum) |
| `replicationFactor` | `3` | Topic replication factor |
| `minInsyncReplicas` | `2` | Minimum in-sync replicas |
| `walPort` | `9291` | WAL replication port |
| `raftPort` | `5001` | Raft consensus port |
| `antiAffinity` | `preferred` | Pod anti-affinity (`preferred` or `required`) |
| `adminApiKeySecret` | none | Secret containing admin API key |
| `updateStrategy.maxUnavailable` | `1` | Max nodes unavailable during rolling update |
| `autoRecover` | `true` | Enable automatic quorum recovery |

### How Config Generation Works

The operator automatically generates a TOML config for each node with:
- Correct `node_id` (1-based)
- Bind addresses (`0.0.0.0:port`)
- Advertised addresses using Kubernetes DNS (`{name}-{i}.{name}-headless.{ns}.svc.cluster.local`)
- Full peer list for Raft cluster formation

### Scaling

```bash
# Scale up
kubectl patch chronikcluster chronik-cluster --type merge -p '{"spec":{"replicas":5}}'

# Scale down
kubectl patch chronikcluster chronik-cluster --type merge -p '{"spec":{"replicas":3}}'
```

The operator handles graceful node removal through the admin API.

---

## Topic Management

ChronikTopic provides declarative topic lifecycle management.

### Basic Topic

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikTopic
metadata:
  name: orders
spec:
  clusterRef:
    name: chronik-cluster
  partitions: 6
  replicationFactor: 3
  config:
    retention.ms: "604800000"
```

### Topic with Columnar + Vector Search

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikTopic
metadata:
  name: events
spec:
  clusterRef:
    name: chronik-cluster
  partitions: 12
  columnar:
    enabled: true
    format: parquet
    compression: zstd
  vectorSearch:
    enabled: true
    embeddingProvider: openai
    embeddingModel: text-embedding-3-small
    field: value
  searchable: true
```

### Key Behaviors

- **Partition increase**: Supported (never decreases)
- **Config changes**: Detected and applied automatically
- **Deletion**: Controlled by `deletionPolicy` (`delete` or `retain`)
- **ClusterRef**: Points to a `ChronikCluster` or `ChronikStandalone` by name

### Targeting a Standalone

```yaml
spec:
  clusterRef:
    name: my-chronik
    kind: ChronikStandalone
```

---

## User & ACL Management

ChronikUser manages authentication credentials and access control.

### Producer User

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikUser
metadata:
  name: app-producer
spec:
  clusterRef:
    name: chronik-cluster
  authentication:
    type: sasl-scram-sha-256
  authorization:
    acls:
      - resource:
          type: topic
          name: orders
        operations: [Write, Describe]
        effect: Allow
```

### What the Operator Creates

1. A Kubernetes **Secret** named `{user-name}-credentials` containing:
   - `username` — The user's name
   - `password` — Auto-generated 32-character password
   - `sasl.mechanism` — Authentication mechanism
   - `bootstrap.servers` — Kafka bootstrap address

2. The SASL user in Chronik via the admin API

3. ACL entries for each rule (expanded: one entry per operation)

### ACL Rule Structure

```yaml
acls:
  - resource:
      type: topic          # topic, group, cluster, transactionalId
      name: my-topic       # Resource name or "*" for all
      patternType: literal  # literal or prefixed
    operations:
      - Read
      - Write
      - Describe
    effect: Allow           # Allow or Deny
```

### Retrieving Credentials

```bash
kubectl get secret app-producer-credentials -o jsonpath='{.data.password}' | base64 -d
```

---

## Autoscaling

ChronikAutoScaler monitors cluster metrics and adjusts `spec.replicas` on the target ChronikCluster.

### Example

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikAutoScaler
metadata:
  name: chronik-autoscaler
spec:
  clusterRef:
    name: chronik-cluster
  minReplicas: 3
  maxReplicas: 9
  metrics:
    - metricType: cpu
      targetValue: 70
      tolerancePercent: 10
    - metricType: disk
      targetValue: 75
      tolerancePercent: 5
    - metricType: produce_throughput
      targetValue: 50000
      tolerancePercent: 15
```

### Metrics Sources

| Metric | Source | Description |
|--------|--------|-------------|
| `cpu` | K8s Metrics API | Average CPU usage across nodes |
| `memory` | K8s Metrics API | Average memory usage across nodes |
| `disk` | Prometheus `/metrics` | Average disk usage from `chronik_storage_usage_bytes` |
| `produce_throughput` | Prometheus `/metrics` | Message rate from `chronik_messages_received_total` |

### Scaling Behavior

- **Tolerance band**: No action within `targetValue +/- tolerancePercent`
- **Stabilization**: Requires N consecutive breaches before acting (default: 3 up, 6 down)
- **Cooldowns**: Separate up/down cooldowns (default: 10min up, 30min down)
- **Step size**: Scales by 1 node at a time
- **Limits**: Never below `minReplicas` or above `maxReplicas`

### Status

```bash
kubectl get chronikautoscalers
# NAME                 CLUSTER           CURRENT   DESIRED   ACTIVE   AGE
# chronik-autoscaler   chronik-cluster   3         3         true     5m
```

---

## Monitoring

The operator exposes Prometheus metrics on the configured metrics port (default: 8080).

### Endpoints

| Path | Description |
|------|-------------|
| `/metrics` | Prometheus metrics |
| `/healthz` | Liveness probe |
| `/readyz` | Readiness probe |

### Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `chronik_operator_reconciliations_total` | Counter | controller, result | Total reconciliations |
| `chronik_operator_reconciliation_duration_seconds` | Histogram | controller | Reconciliation duration |
| `chronik_operator_leader` | Gauge | — | Leader status (1/0) |

### Prometheus ServiceMonitor

```yaml
# Enable in Helm values
metrics:
  serviceMonitor:
    enabled: true
    interval: 30s
```

---

## Troubleshooting

### Operator Logs

```bash
kubectl logs -l app.kubernetes.io/name=chronik-operator -f
```

### Check CRD Status

```bash
# Standalone
kubectl describe chronikstandalone my-chronik

# Cluster
kubectl describe chronikcluster chronik-cluster

# Topic
kubectl describe chroniktopic orders
```

### Common Issues

**Operator not starting:**
- Check RBAC: `kubectl get clusterrole chronik-operator`
- Check ServiceAccount: `kubectl get sa chronik-operator`
- Check leader election: Look for "Waiting for leadership" in logs

**Cluster stuck in Bootstrapping:**
- Ensure admin API key secret exists
- Check PVC provisioning: `kubectl get pvc`
- Check pod events: `kubectl describe pod chronik-cluster-0`

**Topic creation failing:**
- Verify the target cluster is in `Running` phase
- Check admin API connectivity from operator pod
- Verify admin API key is correct

**AutoScaler not scaling:**
- Check `kubectl get cas` for Active=true
- Verify metrics-server is installed (for CPU/memory)
- Check operator logs for metric scraping errors
