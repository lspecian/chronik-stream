# Chronik Operator

Kubernetes operator for [Chronik Stream](https://github.com/chronik-stream/chronik-stream), managing standalone instances and Raft clusters with autoscaling support.

## Features

- **ChronikStandalone** — Single-node deployments with persistent storage
- **ChronikCluster** — Multi-node Raft clusters with automatic TOML config generation, rolling updates, and PodDisruptionBudgets
- **ChronikTopic** — Declarative topic management (partitions, replication, columnar storage, vector search)
- **ChronikUser** — User authentication (SASL) and ACL management with auto-generated credential Secrets
- **ChronikAutoScaler** — Operator-managed scaling based on CPU, memory, disk, and produce throughput metrics

## Quick Start

### Install CRDs and Operator

```bash
# Using Helm
helm install chronik-operator charts/chronik-operator/

# Or apply CRDs manually
cargo run --bin chronik-operator -- crd-gen | kubectl apply -f -
```

### Deploy a Standalone Instance

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikStandalone
metadata:
  name: my-chronik
spec:
  image: ghcr.io/chronik-stream/chronik-server:latest
  storage:
    size: 50Gi
```

### Deploy a 3-Node Cluster

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

### Create a Topic

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
```

## Architecture

```
┌─────────────────────────────────────────────────┐
│              chronik-operator                    │
│                                                  │
│  ┌──────────────┐  ┌──────────────┐             │
│  │  Leader       │  │  Metrics     │             │
│  │  Election     │  │  Server      │             │
│  │  (Lease)      │  │  :8080       │             │
│  └──────────────┘  └──────────────┘             │
│                                                  │
│  ┌─────────────────────────────────────────┐    │
│  │           Controller Manager             │    │
│  │                                          │    │
│  │  Standalone  Cluster  Topic  User  Auto  │    │
│  │  Controller  Ctrl     Ctrl   Ctrl  Scaler│    │
│  └─────────────────────────────────────────┘    │
└─────────────────────────────────────────────────┘
         │              │             │
         ▼              ▼             ▼
   ┌──────────┐  ┌──────────┐  ┌──────────┐
   │ Pods,    │  │ Chronik  │  │ K8s      │
   │ Services,│  │ Admin    │  │ Metrics  │
   │ PVCs,    │  │ API      │  │ API      │
   │ ConfigMap│  │          │  │          │
   └──────────┘  └──────────┘  └──────────┘
```

## Configuration

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--log-level` | `info` | Log level (trace, debug, info, warn, error) |
| `--metrics-addr` | `0.0.0.0:8080` | Metrics and health server bind address |
| `--leader-election` | `true` | Enable Lease-based leader election for HA |

### Environment Variables

| Variable | Description |
|----------|-------------|
| `POD_NAME` | Pod name for leader election identity (set via downward API) |
| `POD_NAMESPACE` | Namespace for leader election Lease (set via downward API) |
| `RUST_LOG` | Override log level |

## Development

```bash
# Build
cargo build -p chronik-operator

# Run tests
cargo test -p chronik-operator

# Generate CRDs
cargo run --bin chronik-operator -- crd-gen

# Run locally (requires kubeconfig)
cargo run --bin chronik-operator -- start --leader-election=false
```

## Helm Chart

See [charts/chronik-operator/](../../charts/chronik-operator/) for the Helm chart.

```bash
# Install
helm install chronik-operator charts/chronik-operator/

# With custom values
helm install chronik-operator charts/chronik-operator/ \
  --set replicaCount=2 \
  --set leaderElection.enabled=true

# Template (dry-run)
helm template chronik-operator charts/chronik-operator/
```

## Examples

See [examples/k8s/](../../examples/k8s/) for complete example manifests:

- `standalone-basic.yaml` — Minimal single-node deployment
- `standalone-s3.yaml` — Standalone with S3 storage, columnar, and vector search
- `cluster-3node.yaml` — 3-node Raft cluster
- `cluster-5node-production.yaml` — Full production cluster with all features
- `topic-orders.yaml` — Basic topic
- `topic-events-columnar.yaml` — Topic with columnar and vector search
- `user-app-producer.yaml` — Producer user with write ACLs
- `user-readonly-consumer.yaml` — Read-only consumer user
- `autoscaler.yaml` — Autoscaler policy
- `secrets/` — Example Secret templates

## License

Apache-2.0
