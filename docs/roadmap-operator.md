# Chronik Kubernetes Operator — Roadmap & Progress Tracker

> **Created**: 2026-02-06
> **Last Updated**: 2026-02-21
> **Status**: Phase 8 Complete — Testing, CI pipeline, clippy/fmt clean, 111 tests passing
> **Crate**: `crates/chronik-operator`

---

## Table of Contents

1. [Overview](#1-overview)
2. [Architecture](#2-architecture)
3. [Custom Resource Definitions](#3-custom-resource-definitions)
4. [Reconciliation Logic](#4-reconciliation-logic)
5. [File Structure](#5-file-structure)
6. [Dependencies](#6-dependencies)
7. [Helm Chart](#7-helm-chart)
8. [RBAC Permissions](#8-rbac-permissions)
9. [Testing Strategy](#9-testing-strategy)
10. [Design Decisions](#10-design-decisions)
11. [Implementation Phases](#11-implementation-phases)
12. [Progress Tracker](#12-progress-tracker)
13. [Session Log](#13-session-log)

---

## 1. Overview

A production-grade Kubernetes operator for Chronik Stream that manages standalone instances and Raft clusters with autoscaling support.

### What the Operator Manages

| CRD | Purpose | K8s Resources Created |
|-----|---------|----------------------|
| **ChronikStandalone** | Single-node Chronik deployment | 1 Pod, 1 PVC, 1 Service, 1 ConfigMap |
| **ChronikCluster** | Multi-node Raft cluster | N Pods, N PVCs, N per-node Services, N ConfigMaps, 1 headless Service, 1 client Service, PodDisruptionBudget |
| **ChronikTopic** | Declarative topic management | Calls Chronik admin/Kafka API to create/update/delete topics |
| **ChronikUser** | User, authentication & ACL management | Creates Secrets for credentials, calls Chronik ACL API |
| **ChronikAutoScaler** | Autoscaling policy for clusters | Operator-managed scaling based on broker resource metrics |

### Key Capabilities

- **Standalone mode**: Simple single-node Chronik with persistent storage
- **Cluster mode**: Raft-based multi-node cluster with automatic TOML config generation
- **Declarative topics**: Create/update/delete topics via CRDs with full config (partitions, replication, columnar, vector)
- **Declarative users**: Manage authentication and ACLs via CRDs, credentials stored as Kubernetes Secrets
- **Leader-aware rolling updates**: Restarts followers first, leader last
- **Zero-downtime scaling**: Uses Chronik's admin API for graceful node add/remove
- **Autoscaling**: Operator-managed scaling based on broker CPU, memory, disk, and produce throughput
- **Listener abstraction**: Named listeners with support for internal, NodePort, LoadBalancer, and Ingress access
- **Pod Disruption Budgets**: Prevent Kubernetes from evicting too many brokers simultaneously
- **Anti-affinity**: Spread cluster nodes across Kubernetes nodes
- **Finalizers**: Graceful teardown with partition migration before Pod deletion

### Future CRDs (Post-v1)

| CRD | Purpose | Inspired By |
|-----|---------|-------------|
| **ChronikNodePool** | Heterogeneous node groups (different resources/storage per pool) | Strimzi KafkaNodePool |
| **ChronikRebalance** | Trigger partition rebalancing with goals and constraints | Strimzi KafkaRebalance |

---

## 2. Architecture

### Operator Deployment

```
┌─────────────────────────────────────────────────────────┐
│  Kubernetes Cluster                                     │
│                                                         │
│  ┌──────────────────────┐                               │
│  │  chronik-operator    │  (Deployment, 1 replica)      │
│  │  ├─ Watches CRDs     │  (leader election via Lease)  │
│  │  ├─ Reconciles state │                               │
│  │  └─ Exposes /metrics │                               │
│  └──────────┬───────────┘                               │
│             │ manages                                   │
│  ┌──────────▼───────────────────────────────────────┐   │
│  │  ChronikCluster "prod-cluster"                   │   │
│  │                                                   │   │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐          │   │
│  │  │ Pod     │  │ Pod     │  │ Pod     │          │   │
│  │  │ node-1  │  │ node-2  │  │ node-3  │          │   │
│  │  │ PVC-1   │  │ PVC-2   │  │ PVC-3   │          │   │
│  │  │ Svc-1   │  │ Svc-2   │  │ Svc-3   │          │   │
│  │  └─────────┘  └─────────┘  └─────────┘          │   │
│  │                                                   │   │
│  │  Headless Service: prod-cluster-headless          │   │
│  │  Client Service:   prod-cluster (LB/ClusterIP)   │   │
│  └───────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

### Individual Pods (Not StatefulSet)

Each cluster node is a standalone Pod managed directly by the operator. This gives us:
- **Per-node TOML configs** generated as ConfigMaps
- **Admin API integration** for node add/remove during scaling
- **Leader-aware operations** (update followers first, leader last)
- **Granular control** over scheduling and lifecycle

### Network Architecture (Per Cluster Node)

| Port | Purpose | Default |
|------|---------|---------|
| 9092 | Kafka protocol | `spec.kafkaPort` |
| 9291 | WAL replication | `spec.walPort` |
| 5001 | Raft consensus | `spec.raftPort` |
| 10001+ | Admin API (10000 + node_id) | auto |
| 6092 | Unified API (SQL/Vector/Search) | `spec.unifiedApiPort` |
| 13001+ | Metrics (13000 + node_id) | auto |

In K8s, each Pod gets its own IP, so all nodes use the same base ports (no offset needed).

### DNS Naming Convention

```
# Per-node Service (for peer discovery and advertised addresses)
{cluster}-{node_id}.{namespace}.svc.cluster.local

# Headless Service (for collective discovery)
{cluster}-headless.{namespace}.svc.cluster.local

# Client Service (for Kafka bootstrap)
{cluster}.{namespace}.svc.cluster.local
```

---

## 3. Custom Resource Definitions

### 3.1 ChronikStandalone

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikStandalone
metadata:
  name: my-chronik
spec:
  image: ghcr.io/chronik-stream/chronik-server:v2.2.23
  imagePullPolicy: IfNotPresent
  imagePullSecrets: []

  # Resource limits
  resources:
    requests: { cpu: "500m", memory: "1Gi" }
    limits: { cpu: "2", memory: "4Gi" }

  # Persistent storage
  storage:
    storageClass: standard
    size: 50Gi
    accessModes: ["ReadWriteOnce"]

  # Ports
  kafkaPort: 9092
  unifiedApiPort: 6092
  metricsPort: 13092

  # Performance profiles
  walProfile: medium        # low, medium, high, ultra
  produceProfile: balanced  # low-latency, balanced, high-throughput
  logLevel: info

  # Object store (optional, for tiered storage)
  objectStore:
    backend: s3
    bucket: my-chronik-data
    region: us-east-1
    credentialsSecret: chronik-s3-creds  # Secret with access-key, secret-key

  # Features
  columnarEnabled: false
  vectorSearch:
    enabled: false
    apiKeySecret: { name: openai-secret, key: api-key }
    model: text-embedding-3-small

  # Scheduling
  nodeSelector: {}
  tolerations: []
  serviceType: ClusterIP
  serviceAnnotations: {}

  # Extra env vars
  env:
    - name: MY_CUSTOM_VAR
      value: "hello"
```

**Status fields:**

```yaml
status:
  phase: Running          # Pending, Running, Failed
  healthy: true
  kafkaAddress: "my-chronik.default.svc.cluster.local:9092"
  unifiedApiAddress: "my-chronik.default.svc.cluster.local:6092"
  observedGeneration: 3
  conditions:
    - type: Ready
      status: "True"
      reason: PodRunning
      message: "Chronik standalone is healthy"
```

### 3.2 ChronikCluster

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikCluster
metadata:
  name: prod-cluster
spec:
  replicas: 3              # Minimum 3 for Raft quorum
  image: ghcr.io/chronik-stream/chronik-server:v2.2.23
  imagePullPolicy: IfNotPresent

  resources:
    requests: { cpu: "1", memory: "2Gi" }
    limits: { cpu: "4", memory: "8Gi" }

  storage:
    storageClass: ssd
    size: 100Gi

  # Raft/replication settings
  replicationFactor: 3
  minInsyncReplicas: 2

  # Internal ports (same on each Pod since each has unique IP)
  walPort: 9291
  raftPort: 5001
  unifiedApiPort: 6092
  metricsPort: 13001

  # Listeners (inspired by Strimzi — named, typed, auto-generates advertised addresses)
  listeners:
    - name: internal
      port: 9092
      type: internal            # ClusterIP Service, Pod-to-Pod within K8s
      tls: false
    - name: external
      port: 9094
      type: nodeport            # NodePort Service for external access
      tls: true
      nodePortBase: 31092       # Node 1 gets 31092, node 2 gets 31093, etc.
    # Future: type: loadbalancer, type: ingress

  # Profiles
  walProfile: high
  produceProfile: balanced
  logLevel: info

  # Admin API authentication
  adminApiKeySecret:
    name: chronik-admin-key
    key: api-key

  # Object store
  objectStore:
    backend: s3
    bucket: prod-chronik
    region: us-east-1
    credentialsSecret: chronik-s3-creds

  # Scheduling
  antiAffinity: preferred              # preferred or required
  antiAffinityTopologyKey: kubernetes.io/hostname
  nodeSelector: {}
  tolerations: []

  # Pod Disruption Budget (prevents K8s from evicting too many brokers at once)
  podDisruptionBudget:
    maxUnavailable: 1           # At most 1 broker unavailable during voluntary disruptions

  # Rolling update strategy
  updateStrategy:
    maxUnavailable: 1
    restartDelaySecs: 30

  # Features
  autoRecover: true
  columnarEnabled: false
  vectorSearch: null
  schemaRegistry:
    enabled: true
    authEnabled: false

  env: []
```

**Status fields:**

```yaml
status:
  phase: Running           # Pending, Bootstrapping, Running, Scaling, Updating, Degraded, Failed
  readyNodes: 3
  desiredNodes: 3
  leaderNodeId: 1
  observedGeneration: 5

  # Per-listener connection info (Strimzi pattern — users read this to connect)
  listeners:
    - name: internal
      bootstrapServers: "prod-cluster.default.svc.cluster.local:9092"
      type: internal
    - name: external
      bootstrapServers: "node1.example.com:31092,node2.example.com:31093,node3.example.com:31094"
      type: nodeport

  headlessService: prod-cluster-headless.default.svc.cluster.local
  nodes:
    - nodeId: 1
      podName: prod-cluster-1
      phase: Running
      ready: true
      isLeader: true
      podIp: 10.244.1.5
    - nodeId: 2
      podName: prod-cluster-2
      phase: Running
      ready: true
      isLeader: false
      podIp: 10.244.2.8
    - nodeId: 3
      podName: prod-cluster-3
      phase: Running
      ready: true
      isLeader: false
      podIp: 10.244.3.3
  partitionSummary:
    totalPartitions: 12
    inSyncPartitions: 12
    underReplicatedPartitions: 0
  conditions:
    - type: ClusterReady
      status: "True"
    - type: LeaderElected
      status: "True"
    - type: AllNodesHealthy
      status: "True"
```

### 3.3 ChronikAutoScaler

> **Design note**: This autoscaler targets **broker infrastructure** (CPU, memory, disk,
> produce throughput). It does NOT scale based on consumer lag — consumer lag means the
> **consumer application** needs more instances, not the broker cluster. Users should use
> KEDA or HPA on their own consumer Deployments for that.

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikAutoScaler
metadata:
  name: prod-autoscaler
spec:
  clusterRef:
    name: prod-cluster
  minReplicas: 3            # Never below Raft quorum
  maxReplicas: 9

  metrics:
    - metricType: cpu
      targetValue: 75        # Avg CPU % per broker before scale up
      tolerancePercent: 10
    - metricType: memory
      targetValue: 80        # Avg memory % per broker
      tolerancePercent: 10
    - metricType: disk
      targetValue: 75        # Avg disk usage % per broker
      tolerancePercent: 5
    - metricType: produce_throughput
      targetValue: 50000     # Messages/sec per broker before scale up
      tolerancePercent: 15

  # Conservative cooldowns — Raft reconfiguration is slow and expensive
  scaleUpCooldownSecs: 600   # 10 min after scale up
  scaleDownCooldownSecs: 1800 # 30 min after scale down (partition migration is slow)

  # How many reconcile cycles a metric must exceed threshold before acting
  scaleUpStabilizationCount: 3    # Must be over threshold for 3 checks (~90s)
  scaleDownStabilizationCount: 6  # Must be under threshold for 6 checks (~180s)
```

**Status fields:**

```yaml
status:
  active: true
  currentReplicas: 3
  desiredReplicas: 3
  lastScaleTime: "2026-02-06T10:30:00Z"
  lastScaleDirection: none
  currentMetrics:
    - metricType: cpu
      currentValue: 45
    - metricType: memory
      currentValue: 62
    - metricType: disk
      currentValue: 38
    - metricType: produce_throughput
      currentValue: 12000
  conditions:
    - type: ScalingActive
      status: "True"
    - type: WithinLimits
      status: "True"
```

### 3.4 ChronikTopic

Declarative topic management — create, update, and delete topics via CRDs instead of `kafka-topics.sh`.

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikTopic
metadata:
  name: orders
  labels:
    chronik.io/cluster: prod-cluster   # Which cluster this topic belongs to
spec:
  # Reference to the Chronik cluster (standalone or cluster)
  clusterRef:
    name: prod-cluster

  # Topic name (defaults to metadata.name if omitted)
  topicName: orders

  # Core settings
  partitions: 6
  replicationFactor: 3                   # Capped at cluster replicas

  # Kafka-compatible topic configs
  config:
    retention.ms: "604800000"            # 7 days
    cleanup.policy: "delete"             # delete, compact, or delete,compact
    min.insync.replicas: "2"
    max.message.bytes: "1048576"         # 1MB
    compression.type: "zstd"

  # Chronik-specific features (optional)
  columnar:
    enabled: true
    format: parquet                      # parquet or arrow
    compression: zstd
    partitioning: daily                  # none, hourly, daily

  vectorSearch:
    enabled: false
    embeddingProvider: openai
    embeddingModel: text-embedding-3-small
    field: value                         # value, key, or $.json.path
    indexMetric: cosine                  # cosine, euclidean, dot

  searchable: true                       # Enable Tantivy full-text indexing
```

**Status fields:**

```yaml
status:
  phase: Ready                  # Pending, Creating, Ready, Updating, Error
  observedGeneration: 2
  currentPartitions: 6
  currentReplicationFactor: 3
  topicId: "abc123-def456"      # Internal topic ID from Chronik
  conditions:
    - type: Ready
      status: "True"
      reason: TopicExists
      message: "Topic 'orders' exists with 6 partitions"
```

**Reconciliation:**
- Operator calls Chronik's Kafka protocol (CreateTopics API) or admin API to create topics
- Partition count can be increased but never decreased (Kafka semantics)
- Config changes applied via AlterConfigs API
- On CR deletion: topic is deleted from cluster (with optional `deletionPolicy: retain` to keep data)

### 3.5 ChronikUser

Declarative user and ACL management. Credentials stored as Kubernetes Secrets.

```yaml
apiVersion: chronik.io/v1alpha1
kind: ChronikUser
metadata:
  name: app-producer
  labels:
    chronik.io/cluster: prod-cluster
spec:
  clusterRef:
    name: prod-cluster

  # Authentication type
  authentication:
    type: sasl-scram-sha-256            # sasl-scram-sha-256, sasl-plain, tls
    # Password auto-generated and stored in Secret if not provided
    passwordSecret:
      name: app-producer-credentials    # Operator creates this Secret
      key: password

  # Authorization (ACLs)
  authorization:
    acls:
      - resource:
          type: topic                   # topic, group, cluster, transactionalId
          name: orders
          patternType: literal          # literal or prefixed
        operations: [Write, Describe]   # Read, Write, Create, Delete, Alter, Describe, All
        effect: Allow                   # Allow or Deny

      - resource:
          type: topic
          name: "events-"
          patternType: prefixed         # Match all topics starting with "events-"
        operations: [Read, Describe]
        effect: Allow

      - resource:
          type: group
          name: app-producer-group
          patternType: literal
        operations: [Read]
        effect: Allow

  # Schema Registry access (optional)
  schemaRegistry:
    enabled: true
    role: readwrite                     # readonly, readwrite
```

**Status fields:**

```yaml
status:
  phase: Ready                  # Pending, Ready, Error
  observedGeneration: 1
  # Secret containing credentials (created by operator)
  credentialsSecret: app-producer-credentials
  aclCount: 3
  conditions:
    - type: Ready
      status: "True"
    - type: CredentialsReady
      status: "True"
      message: "Credentials stored in Secret 'app-producer-credentials'"
```

**Reconciliation:**
- Operator creates/updates SASL users via Chronik's auth API
- Auto-generates secure password if not provided, stores in Kubernetes Secret
- ACLs applied via CreateAcls Kafka API
- On CR deletion: user and ACLs removed, Secret deleted
- Secret has owner reference → garbage collected if CR deleted

---

## 4. Reconciliation Logic

### 4.1 Standalone Reconciler

```
On each reconcile:
  1. Check deletion → run finalizer (cleanup PVC if reclaimPolicy=Delete)
  2. Ensure PVC exists (create if missing, expand if spec.size grew)
  3. Ensure ConfigMap with env overrides
  4. Ensure Pod (create/recreate if spec changed — image, resources, env)
     - Command: ["chronik-server", "start", "--data-dir", "/data"]
     - Env: CHRONIK_ADVERTISED_ADDR, RUST_LOG, profiles, user env
     - Liveness: TCP on kafka port (initialDelay 15s, period 10s)
     - Readiness: TCP on kafka port (initialDelay 10s, period 5s)
     - Volumes: PVC at /data
  5. Ensure Service (ClusterIP/LoadBalancer/NodePort)
  6. Update CR status (phase, healthy, addresses)
  7. Requeue: 10s if not ready, 60s if healthy
```

### 4.2 Cluster Reconciler

```
On each reconcile:
  1. Check deletion → graceful teardown
     - For each node (highest ID first, non-leaders first):
       a. Call admin API: remove-node (graceful)
       b. Wait for partition migration
       c. Delete Pod, Service, ConfigMap
     - Delete headless Service, client Service
     - Optionally delete PVCs
     - Remove finalizer

  2. Determine desired node set: {1, 2, ..., spec.replicas}
  3. Discover current nodes (list Pods by label)

  4. Generate TOML configs for all nodes as ConfigMaps
     - Each node gets unique node_id, bind/advertise addresses
     - Peers list includes ALL nodes
     - Advertised addresses use per-node Service DNS names

  5. Ensure headless Service (clusterIP: None)
  6. Ensure per-listener Services (internal ClusterIP, external NodePort/LB)
  7. Ensure PodDisruptionBudget (maxUnavailable from spec)

  8. Determine action:
     ┌─────────────────────────────────────────────────┐
     │ No nodes exist         → Bootstrap (create all) │
     │ desired > current      → Scale Up               │
     │ desired < current      → Scale Down             │
     │ Spec changed (image…)  → Rolling Update         │
     │ All healthy            → No-op                  │
     └─────────────────────────────────────────────────┘

  BOOTSTRAP:
     - Create all PVCs, ConfigMaps, per-node Services, Pods
     - Wait for Raft leader election (poll /admin/health)
     - Set phase: Bootstrapping → Running

  SCALE UP (one node at a time):
     a. Update ALL ConfigMaps with new peer in [[peers]]
     b. Create PVC, ConfigMap, per-node Service, Pod for new node
     c. Wait for Pod Ready
     d. Call admin API: add-node on leader
     e. Verify via admin API: status shows new node

  SCALE DOWN (one node at a time, highest ID first):
     a. Call admin API: remove-node on leader (graceful)
     b. Wait for partition migration (poll admin status, check ISR)
     c. Delete Pod, per-node Service, ConfigMap
     d. Update ALL remaining ConfigMaps to remove peer
     e. Never scale below 3 (Raft quorum)

  ROLLING UPDATE (leader-aware):
     a. Query /admin/health on all nodes → identify leader
     b. Order: all followers first (ascending ID), then leader
     c. For each node:
        - Update ConfigMap if changed
        - Delete old Pod
        - Create new Pod with updated spec
        - Wait for Pod Ready + node rejoined cluster
        - Wait restartDelaySecs (default 30s)
     d. Respect maxUnavailable (default 1)

  9. Health monitoring: poll /admin/health on all nodes
  10. Update CR status (readyNodes, leaderNodeId, listeners[], nodes[], partitions)
  11. Requeue: 5s during bootstrap/scaling, 30s running, 10s degraded
```

### 4.3 AutoScaler Reconciler

> **Why no KEDA?** KEDA is designed for fast, elastic scaling of stateless consumers.
> A Raft cluster needs slow, deliberate scaling (partition migration, Raft reconfiguration)
> with safety checks at every step. KEDA would just blindly patch `replicas` with no
> awareness of quorum, ISR, or graceful node removal — making it a liability, not a help.

```
On each reconcile:
  1. Resolve the referenced ChronikCluster
  2. Scrape /metrics from each broker node (Prometheus endpoint)
  3. Compute average per-node values for each configured metric
  4. Evaluate against thresholds:
     - Track consecutive breaches (stabilization count)
     - Only act after N consecutive breaches to avoid flapping
  5. If scale-up needed AND cooldown expired:
     - Compute new replicas (add 1 at a time, conservative)
     - Patch ChronikCluster spec.replicas
     - Record lastScaleTime
  6. If scale-down needed AND cooldown expired:
     - Compute new replicas (remove 1 at a time)
     - Never go below minReplicas (≥3 for Raft quorum)
     - Patch ChronikCluster spec.replicas
     - Record lastScaleTime
  7. Update status (currentMetrics, desiredReplicas, conditions)
  8. Requeue: 30s
```

### 4.4 Topic Reconciler

```
On each reconcile:
  1. Resolve clusterRef → find the target cluster's bootstrap address
  2. Check deletion → delete topic via Kafka DeleteTopics API, remove finalizer
  3. Check if topic exists (Metadata API)

  IF topic does not exist:
     - Create via CreateTopics API with partitions, replicationFactor, config
     - If columnar/vector/searchable specified, set topic configs
     - Set phase: Creating → Ready

  IF topic exists but spec changed:
     - Partition increase: call CreatePartitions API (never decrease)
     - Config changes: call AlterConfigs API
     - Columnar/vector/searchable changes: update topic configs
     - Set phase: Updating → Ready

  4. Update status (currentPartitions, replicationFactor, topicId)
  5. Requeue: 60s (topics are stable, infrequent changes)
```

### 4.5 User Reconciler

```
On each reconcile:
  1. Resolve clusterRef → find the target cluster's admin API address
  2. Check deletion → remove user + ACLs, delete credentials Secret, remove finalizer

  3. Ensure credentials Secret exists
     - If spec.authentication.passwordSecret references a Secret that doesn't exist:
       a. Generate a secure random password (32 chars, alphanumeric + symbols)
       b. Create Kubernetes Secret with owner reference to the ChronikUser CR
     - If Secret exists, read the password from it

  4. Ensure user exists in Chronik
     - Create/update SASL user via auth API with credentials from Secret
     - Support SCRAM-SHA-256, SCRAM-SHA-512, PLAIN

  5. Ensure ACLs match spec
     - List current ACLs for this user
     - Diff against spec.authorization.acls
     - Create missing ACLs (CreateAcls API)
     - Delete stale ACLs (DeleteAcls API)

  6. Ensure Schema Registry access (if specified)
     - Create/update schema registry user credentials

  7. Update status (phase, credentialsSecret, aclCount)
  8. Requeue: 120s (users are very stable)
```

---

## 5. File Structure

```
crates/chronik-operator/
├── Cargo.toml
├── src/
│   ├── main.rs                         # CLI: start, crd-gen
│   ├── lib.rs                          # Library root, re-exports
│   ├── constants.rs                    # Labels, finalizer names, defaults
│   ├── error.rs                        # OperatorError (thiserror)
│   ├── telemetry.rs                    # Tracing + Prometheus setup
│   ├── leader_election.rs              # Lease-based HA leader election
│   ├── metrics.rs                      # Operator /metrics endpoint
│   ├── crds/
│   │   ├── mod.rs
│   │   ├── standalone.rs               # ChronikStandalone CRD
│   │   ├── cluster.rs                  # ChronikCluster CRD
│   │   ├── topic.rs                    # ChronikTopic CRD
│   │   ├── user.rs                     # ChronikUser CRD
│   │   ├── autoscaler.rs              # ChronikAutoScaler CRD
│   │   ├── common.rs                  # Shared types (StorageSpec, Condition, Listener, etc.)
│   │   └── defaults.rs               # Default value functions
│   ├── controllers/
│   │   ├── mod.rs
│   │   ├── standalone_controller.rs   # Standalone reconciler
│   │   ├── cluster_controller.rs      # Cluster reconciler
│   │   ├── topic_controller.rs        # Topic reconciler
│   │   ├── user_controller.rs         # User reconciler
│   │   └── autoscaler_controller.rs   # AutoScaler reconciler
│   ├── resources/
│   │   ├── mod.rs
│   │   ├── pod_builder.rs             # Pod spec construction
│   │   ├── pvc_builder.rs             # PVC construction
│   │   ├── service_builder.rs         # Service construction (internal, nodeport, headless)
│   │   ├── pdb_builder.rs             # PodDisruptionBudget construction
│   │   └── configmap_builder.rs       # ConfigMap + TOML generation
│   ├── admin_client.rs                # HTTP client for Chronik Admin API
│   ├── kafka_client.rs                # Kafka protocol client (CreateTopics, ACLs, etc.)
│   ├── health.rs                      # Health check polling
│   ├── config_generator.rs            # TOML config generation
│   └── scaling.rs                     # Metrics scraping + scaling decision engine
│
charts/chronik-operator/
├── Chart.yaml
├── values.yaml
├── templates/
│   ├── deployment.yaml                # Operator Deployment
│   ├── serviceaccount.yaml
│   ├── clusterrole.yaml               # RBAC
│   ├── clusterrolebinding.yaml
│   ├── crds/
│   │   ├── chronikstandalone.yaml
│   │   ├── chronikcluster.yaml
│   │   ├── chroniktopic.yaml
│   │   ├── chronikuser.yaml
│   │   └── chronikautoscaler.yaml
│   ├── _helpers.tpl
│   └── NOTES.txt
│
examples/k8s/
├── standalone-basic.yaml
├── standalone-s3.yaml
├── cluster-3node.yaml
├── cluster-5node-production.yaml
├── topic-orders.yaml
├── topic-events-columnar.yaml
├── user-app-producer.yaml
├── user-readonly-consumer.yaml
├── autoscaler.yaml
└── secrets/
    ├── admin-api-key.yaml
    ├── s3-credentials.yaml
    └── openai-key.yaml
```

---

## 6. Dependencies

### Cargo.toml

```toml
[package]
name = "chronik-operator"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Kubernetes operator for Chronik Stream"

[[bin]]
name = "chronik-operator"
path = "src/main.rs"

[dependencies]
# Kubernetes
kube = { version = "0.98", features = ["runtime", "derive", "client"] }
k8s-openapi = { version = "0.23", features = ["v1_30"] }
schemars = "0.8"

# Async
tokio = { workspace = true }
futures = { workspace = true }

# Serialization
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml = "0.9"
toml = "0.8"

# HTTP client (for Chronik Admin API)
reqwest = { version = "0.12", features = ["json"] }

# Error handling
anyhow = { workspace = true }
thiserror = { workspace = true }

# Observability
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
prometheus = { workspace = true }

# CLI
clap = { workspace = true }

# Utilities
chrono = { workspace = true }
uuid = { workspace = true }

[dev-dependencies]
chronik-config = { path = "../chronik-config" }   # TOML validation only
tempfile = { workspace = true }
```

**Note**: `kube` and `k8s-openapi` versions are specified directly (not workspace) since the existing workspace may have older versions. Verify compatibility before implementation.

---

## 7. Helm Chart

### values.yaml (key fields)

```yaml
replicaCount: 1
image:
  repository: ghcr.io/chronik-stream/chronik-operator
  tag: ""                    # Defaults to appVersion
  pullPolicy: IfNotPresent
resources:
  requests: { cpu: 100m, memory: 128Mi }
  limits: { cpu: 200m, memory: 256Mi }
leaderElection:
  enabled: true
metrics:
  enabled: true
  port: 8080
logLevel: info
installCRDs: true
```

---

## 8. RBAC Permissions

The operator needs these permissions:

| Resource | Verbs | Purpose |
|----------|-------|---------|
| `chronikstandalones` | get, list, watch, create, update, patch, delete | Manage CRs |
| `chronikstandalones/status` | get, update, patch | Update status |
| `chronikstandalones/finalizers` | update | Set/remove finalizers |
| `chronikclusters` | get, list, watch, create, update, patch, delete | Manage CRs |
| `chronikclusters/status` | get, update, patch | Update status |
| `chronikclusters/finalizers` | update | Set/remove finalizers |
| `chroniktopics` | get, list, watch, create, update, patch, delete | Manage CRs |
| `chroniktopics/status` | get, update, patch | Update status |
| `chroniktopics/finalizers` | update | Set/remove finalizers |
| `chronikusers` | get, list, watch, create, update, patch, delete | Manage CRs |
| `chronikusers/status` | get, update, patch | Update status |
| `chronikusers/finalizers` | update | Set/remove finalizers |
| `chronikautoscalers` | get, list, watch, create, update, patch, delete | Manage CRs |
| `chronikautoscalers/status` | get, update, patch | Update status |
| `chronikautoscalers/finalizers` | update | Set/remove finalizers |
| `pods` | get, list, watch, create, update, patch, delete | Manage Pods |
| `pods/status` | get | Read Pod status |
| `persistentvolumeclaims` | get, list, watch, create, update, patch, delete | Manage PVCs |
| `services` | get, list, watch, create, update, patch, delete | Manage Services |
| `configmaps` | get, list, watch, create, update, patch, delete | Manage ConfigMaps |
| `secrets` | get, list, watch | Read secrets (credentials) |
| `poddisruptionbudgets` | get, list, watch, create, update, patch, delete | Manage PDBs |
| `events` | create, patch | Emit Kubernetes Events |
| `leases` | get, list, watch, create, update, patch, delete | Leader election |

---

## 9. Testing Strategy

### Unit Tests (inline `#[cfg(test)]`)

- **TOML config generation**: Given N replicas, verify generated TOML matches `chronik-config::ClusterConfig` schema
- **Pod builder**: Correct volumes, env vars, probes, anti-affinity
- **Service builder**: Headless, per-node, and client services
- **PVC builder**: Storage class, size, access modes
- **Error classification**: Each error maps to correct retry/no-retry action
- **Default values**: All defaults produce expected values

### Integration Tests (`crates/chronik-operator/tests/`)

- **ConfigMap TOML round-trip**: Generate TOML → parse with `chronik-config` → validate
- **Mock reconciliation**: kube-rs mock client → test reconcile without real cluster
- **Admin API client**: Mock HTTP server implementing Chronik admin contract

### End-to-End Tests (`tests/operator-e2e/`)

- Requires kind or k3d cluster
- Deploy operator via Helm
- Create `ChronikStandalone` → verify Pod/PVC/Service
- Create `ChronikCluster` (3 replicas) → verify 3 Pods, headless svc, client svc, leader election
- Scale up to 5 → verify add-node and new Pods
- Scale down to 3 → verify remove-node and Pod deletion
- Rolling update (change image) → verify follower-first, leader-last
- Delete CR → verify graceful teardown

### Property Tests (proptest)

- Random replica counts (3–10): verify TOML consistency
- Random scaling sequences: verify quorum never violated

---

## 10. Design Decisions

### 10.1 Individual Pods vs StatefulSet

**Decision**: Individual Pods + PVCs.

**Rationale**: StatefulSet assumes ordered/sequential scaling and doesn't support:
- Node-specific TOML configs (not ordinal-based)
- Admin API calls during scale-up/down
- Leader-aware rolling updates (followers first)
- Graceful node removal via admin API before Pod deletion

**Trade-off**: More operator complexity but complete control over Raft lifecycle.

### 10.2 Per-Node Service for DNS

Each Pod gets a per-node Service (`{cluster}-{node_id}`) for stable DNS. This is required because individually-created Pods (unlike StatefulSet pods) don't automatically get DNS entries under a headless service. The per-node Service DNS is used as the advertised address in the TOML config.

### 10.3 Config Generation vs Config Mounting

Operator generates TOML and stores in ConfigMap (mounted into Pod). Alternative: init container generates config at startup. Rejected because it pushes topology knowledge into the container.

### 10.4 Dependency Isolation

`chronik-operator` does NOT depend on `chronik-server`, `chronik-wal`, or other heavy crates. Only:
- `chronik-config` as dev-dependency (for TOML validation tests)
- Own copies of Admin API types (JSON-compatible)
- `kube-rs`, `k8s-openapi`, `reqwest`, `serde`, `tokio`, `clap`

Keeps the operator binary small (~5-10MB).

### 10.5 Labels Convention

```yaml
app.kubernetes.io/name: chronik-stream
app.kubernetes.io/instance: {cr-name}
app.kubernetes.io/component: standalone | cluster-node
app.kubernetes.io/managed-by: chronik-operator
app.kubernetes.io/version: {image-tag}
chronik.io/node-id: "{node_id}"        # cluster only
chronik.io/cluster-name: "{cr-name}"    # cluster only
```

### 10.6 Finalizer

```
chronik.io/operator-cleanup
```

Ensures graceful node removal (partition migration) before Kubernetes deletes Pods.

---

## 11. Implementation Phases

### Phase 1: Foundation

**Goal**: Crate scaffolding, CRD definitions, basic CLI.

| Task | Files |
|------|-------|
| Create crate directory structure | `crates/chronik-operator/` |
| Cargo.toml with all dependencies | `crates/chronik-operator/Cargo.toml` |
| Add to workspace members | `Cargo.toml` (root) |
| CLI with `start` and `crd-gen` commands | `src/main.rs` |
| Error types | `src/error.rs` |
| Constants (labels, finalizer) | `src/constants.rs` |
| Telemetry setup (tracing) | `src/telemetry.rs` |
| ChronikStandalone CRD types | `src/crds/standalone.rs` |
| ChronikCluster CRD types (with listeners) | `src/crds/cluster.rs` |
| ChronikTopic CRD types | `src/crds/topic.rs` |
| ChronikUser CRD types | `src/crds/user.rs` |
| ChronikAutoScaler CRD types | `src/crds/autoscaler.rs` |
| Shared types | `src/crds/common.rs` |
| Default value functions | `src/crds/defaults.rs` |
| CRD module root | `src/crds/mod.rs` |
| Verify `crd-gen` produces valid YAML | Test |

### Phase 2: Standalone Controller

**Goal**: Full lifecycle management for single-node Chronik.

| Task | Files |
|------|-------|
| Pod builder (standalone) | `src/resources/pod_builder.rs` |
| PVC builder | `src/resources/pvc_builder.rs` |
| Service builder | `src/resources/service_builder.rs` |
| ConfigMap builder | `src/resources/configmap_builder.rs` |
| Standalone reconciler | `src/controllers/standalone_controller.rs` |
| Finalizer handling | (within reconciler) |
| Status updates | (within reconciler) |
| Unit tests for builders | inline `#[cfg(test)]` |
| Unit tests for reconciler | inline `#[cfg(test)]` |
| Wire into main.rs controller startup | `src/main.rs` |

### Phase 3: Cluster Controller

**Goal**: Full cluster lifecycle — bootstrap, scale, update, teardown.

| Task | Files |
|------|-------|
| TOML config generator | `src/config_generator.rs` |
| Admin API HTTP client | `src/admin_client.rs` |
| Health check polling | `src/health.rs` |
| Cluster bootstrap logic | `src/controllers/cluster_controller.rs` |
| Scale-up logic (add-node) | (within cluster controller) |
| Scale-down logic (remove-node) | (within cluster controller) |
| Rolling update (leader-aware) | (within cluster controller) |
| Graceful teardown (finalizer) | (within cluster controller) |
| Per-node Service creation | `src/resources/service_builder.rs` |
| Headless Service | `src/resources/service_builder.rs` |
| Per-listener Services (internal, nodeport) | `src/resources/service_builder.rs` |
| PodDisruptionBudget | `src/resources/pdb_builder.rs` |
| ConfigMap per node (TOML) | `src/resources/configmap_builder.rs` |
| Status tracking (nodes, leader, ISR, listeners) | (within cluster controller) |
| Unit tests: config generator | inline `#[cfg(test)]` |
| Unit tests: admin client | inline `#[cfg(test)]` |
| Integration test: TOML round-trip | `tests/` |
| Wire into main.rs | `src/main.rs` |

### Phase 4: Topic & User Controllers

**Goal**: Declarative topic and user/ACL management via CRDs.

| Task | Files |
|------|-------|
| Kafka protocol client (CreateTopics, AlterConfigs, CreateAcls) | `src/kafka_client.rs` |
| Topic reconciler | `src/controllers/topic_controller.rs` |
| Topic creation via Kafka API | (within topic controller) |
| Topic config updates (AlterConfigs) | (within topic controller) |
| Partition increase (CreatePartitions) | (within topic controller) |
| Topic deletion + finalizer | (within topic controller) |
| User reconciler | `src/controllers/user_controller.rs` |
| SASL user creation/update | (within user controller) |
| Credentials Secret generation (auto-password) | (within user controller) |
| ACL diff + sync (CreateAcls/DeleteAcls) | (within user controller) |
| User deletion + finalizer | (within user controller) |
| Unit tests: kafka client | inline `#[cfg(test)]` |
| Unit tests: topic reconciler | inline `#[cfg(test)]` |
| Unit tests: user reconciler | inline `#[cfg(test)]` |
| Wire into main.rs | `src/main.rs` |

### Phase 5: AutoScaler Controller

**Goal**: Operator-managed broker autoscaling based on resource metrics.

| Task | Files |
|------|-------|
| Prometheus metrics scraper | `src/scaling.rs` |
| Scaling decision engine (stabilization, cooldowns) | `src/scaling.rs` |
| AutoScaler reconciler | `src/controllers/autoscaler_controller.rs` |
| Cooldown enforcement | (within autoscaler controller) |
| ChronikCluster spec patching | (within autoscaler controller) |
| Unit tests | inline `#[cfg(test)]` |
| Wire into main.rs | `src/main.rs` |

### Phase 6: Helm Chart & Deployment

**Goal**: Production-ready operator deployment.

| Task | Files |
|------|-------|
| Chart.yaml | `charts/chronik-operator/Chart.yaml` |
| values.yaml | `charts/chronik-operator/values.yaml` |
| Deployment template | `charts/chronik-operator/templates/deployment.yaml` |
| ServiceAccount | `charts/chronik-operator/templates/serviceaccount.yaml` |
| ClusterRole + Binding | `charts/chronik-operator/templates/clusterrole*.yaml` |
| CRD templates (generated) | `charts/chronik-operator/templates/crds/` |
| _helpers.tpl | `charts/chronik-operator/templates/_helpers.tpl` |
| NOTES.txt | `charts/chronik-operator/templates/NOTES.txt` |
| Operator leader election | `src/leader_election.rs` |
| Operator /metrics endpoint | `src/metrics.rs` |
| Dockerfile for operator | `Dockerfile.operator` |

### Phase 7: Examples & Documentation

**Goal**: User-facing docs and example CRs.

| Task | Files |
|------|-------|
| Example: standalone basic | `examples/k8s/standalone-basic.yaml` |
| Example: standalone with S3 | `examples/k8s/standalone-s3.yaml` |
| Example: 3-node cluster | `examples/k8s/cluster-3node.yaml` |
| Example: 5-node production | `examples/k8s/cluster-5node-production.yaml` |
| Example: topic (basic + columnar) | `examples/k8s/topic-*.yaml` |
| Example: user (producer + consumer) | `examples/k8s/user-*.yaml` |
| Example: autoscaler | `examples/k8s/autoscaler.yaml` |
| Example: secrets | `examples/k8s/secrets/` |
| Operator README | `crates/chronik-operator/README.md` |
| User guide | `docs/KUBERNETES_OPERATOR.md` |

### Phase 8: Testing & Hardening

**Goal**: Comprehensive test coverage and production readiness.

| Task | Files |
|------|-------|
| Mock integration tests (AdminClient + KafkaAdminClient) | `crates/chronik-operator/tests/mock_admin_api.rs` |
| Property-based scaling invariant tests | `crates/chronik-operator/tests/scaling_properties.rs` |
| E2E test harness (kind) | `tests/e2e/operator/setup-kind.sh`, `run-tests.sh`, `teardown-kind.sh` |
| CI pipeline (check, test, build, Helm) | `.github/workflows/operator.yml` |
| Clippy + formatting cleanup | All source files |

---

## 12. Progress Tracker

### Phase 1: Foundation
- [x] Create `crates/chronik-operator/` directory
- [x] Write `Cargo.toml` with dependencies
- [x] Add crate to workspace `Cargo.toml` members
- [x] Implement `src/main.rs` (CLI: `start`, `crd-gen`)
- [x] Implement `src/lib.rs`
- [x] Implement `src/error.rs` (OperatorError)
- [x] Implement `src/constants.rs`
- [x] Implement `src/telemetry.rs`
- [x] Implement `src/crds/mod.rs`
- [x] Implement `src/crds/common.rs` (shared types, Listener, Condition, etc.)
- [x] Implement `src/crds/defaults.rs`
- [x] Implement `src/crds/standalone.rs` (ChronikStandalone CRD)
- [x] Implement `src/crds/cluster.rs` (ChronikCluster CRD with listeners)
- [x] Implement `src/crds/topic.rs` (ChronikTopic CRD)
- [x] Implement `src/crds/user.rs` (ChronikUser CRD)
- [x] Implement `src/crds/autoscaler.rs` (ChronikAutoScaler CRD)
- [x] Verify `cargo build` succeeds
- [x] Verify `crd-gen` produces valid CRD YAML
- [x] Unit tests for CRD defaults

### Phase 2: Standalone Controller
- [x] Implement `src/resources/mod.rs`
- [x] Implement `src/resources/pod_builder.rs`
- [x] Implement `src/resources/pvc_builder.rs`
- [x] Implement `src/resources/service_builder.rs` (ClusterIP, NodePort, headless)
- [x] Implement `src/resources/configmap_builder.rs`
- [x] Implement `src/controllers/mod.rs`
- [x] Implement `src/controllers/standalone_controller.rs`
  - [x] PVC reconciliation
  - [x] ConfigMap reconciliation
  - [x] Pod reconciliation (create/recreate on spec change)
  - [x] Service reconciliation
  - [x] Status updates
  - [x] Finalizer handling
- [x] Wire standalone controller into `main.rs`
- [x] Unit tests: pod builder
- [x] Unit tests: pvc builder
- [x] Unit tests: service builder
- [ ] Unit tests: standalone reconciler
- [ ] Manual test: `cargo run --bin chronik-operator -- crd-gen | kubectl apply -f -`

### Phase 3: Cluster Controller
- [x] Implement `src/config_generator.rs`
  - [x] Generate valid TOML for N nodes
  - [x] Correct bind/advertise/peers sections
  - [x] DNS-based advertised addresses
- [x] Implement `src/admin_client.rs`
  - [x] Health check (GET /admin/health)
  - [x] Cluster status (GET /admin/status)
  - [x] Add node (POST /admin/add-node)
  - [x] Remove node (POST /admin/remove-node)
  - [x] Leader discovery (poll all nodes)
  - [x] Timeout and retry handling
- [x] Implement `src/health.rs`
  - [x] Periodic health polling
  - [x] Leader tracking
- [x] Implement `src/resources/pdb_builder.rs` (PodDisruptionBudget)
- [x] Implement `src/controllers/cluster_controller.rs`
  - [x] Bootstrap: create all nodes from scratch
  - [x] Headless Service creation
  - [x] Per-listener Services (internal, nodeport, etc.)
  - [x] Per-node Service creation
  - [x] Per-node ConfigMap (TOML) creation + update on change
  - [x] Per-node PVC creation
  - [x] Per-node Pod creation (with spec change detection)
  - [x] PodDisruptionBudget creation
  - [x] Scale up: add nodes (ensure resources for new node_ids)
  - [x] Scale down: remove excess nodes (highest ID first, retain PVCs)
  - [x] Quorum safety (never below 3, validated at reconcile start)
  - [ ] Rolling update: leader-aware ordering (requires health polling integration)
  - [ ] Rolling update: maxUnavailable enforcement
  - [x] Graceful teardown via finalizer (owner references cascade)
  - [x] Status tracking (nodes, ready count, phase, conditions, headless DNS)
  - [x] Phase state machine (Bootstrapping→Running→Degraded→Failed)
- [x] Extend `src/resources/pod_builder.rs` for cluster nodes
  - [x] TOML config volume mount (/config/cluster.toml)
  - [x] WAL, Raft, Admin API ports
  - [x] Anti-affinity (preferred/required)
  - [x] Admin API key from Secret
- [x] Extend `src/resources/service_builder.rs` for cluster
  - [x] Headless Service (clusterIP: None, publishNotReadyAddresses)
  - [x] Per-node Service with admin port
- [x] Extend `src/resources/configmap_builder.rs` for cluster
  - [x] Per-node ConfigMap with cluster.toml key
- [x] Extend `src/resources/pvc_builder.rs` for cluster
  - [x] Per-node PVC (cluster_name-node_id-data)
- [x] Wire cluster controller into `main.rs`
- [x] Unit tests: config generator (5 tests)
- [x] Unit tests: admin client (5 tests)
- [x] Unit tests: cluster reconciler (5 tests)
- [x] Unit tests: pod builder cluster mode (3 tests)
- [x] Unit tests: service builder cluster mode (2 tests)
- [x] Unit tests: configmap builder cluster mode (1 test)
- [x] Unit tests: pvc builder cluster mode (1 test)
- [x] Unit tests: pdb builder (1 test)
- [ ] Integration test: TOML round-trip with chronik-config
- [ ] Integration test: Admin API mock server

### Phase 4: Topic & User Controllers
- [x] Implement `src/kafka_client.rs`
  - [x] CreateTopics API call
  - [x] DeleteTopics API call
  - [x] CreatePartitions API call
  - [x] AlterConfigs API call
  - [x] CreateAcls / DeleteAcls API calls (SetAcls replaces all)
  - [x] DescribeTopics API call (check topic existence)
  - [x] CreateUser / DeleteUser API calls
- [x] Implement `src/controllers/topic_controller.rs`
  - [x] Topic creation (partitions, replication, config)
  - [x] Topic config updates (via admin API)
  - [x] Partition increase (never decrease)
  - [x] Columnar/vector/searchable topic config flags
  - [x] Topic deletion + finalizer (optional retain policy)
  - [x] Status updates (phase, currentPartitions, topicId)
  - [x] ClusterRef resolution (ChronikCluster + ChronikStandalone)
- [x] Implement `src/controllers/user_controller.rs`
  - [x] Credentials Secret auto-generation (32-char secure random password)
  - [x] SASL user creation/update
  - [x] ACL expansion (CRD rules with multiple operations → individual entries)
  - [ ] Schema Registry user access (deferred — requires schema registry admin API)
  - [x] User deletion + finalizer (remove user via admin API)
  - [x] Status updates (phase, credentialsSecret, aclCount)
  - [x] Owner reference on Secrets (garbage collected on CR deletion)
- [x] Wire topic + user controllers into `main.rs`
- [x] Unit tests: kafka client (8 tests — serialization, URLs, ACL expansion)
- [x] Unit tests: topic reconciler (5 tests — config building)
- [x] Unit tests: user reconciler (3 tests — password generation)
- [x] Unit tests: ACL expansion logic

### Phase 5: AutoScaler Controller
- [x] Implement `src/scaling.rs`
  - [x] Prometheus metrics scraper (parse /metrics endpoint)
  - [x] K8s Metrics API scraper (DynamicObject + ApiResource for metrics.k8s.io/v1beta1)
  - [x] K8s quantity parsers (CPU millicores: 100m, 2, 2.5; storage/memory: 512Mi, 1Gi, 50Gi)
  - [x] Metric aggregation (avg CPU, memory, disk, throughput per broker)
  - [x] Stabilization counter (N consecutive breaches before acting)
  - [x] Scaling decision engine (scale up/down by 1 at a time, tolerance bands)
  - [x] Produce rate computation from counter deltas between reconcile cycles
  - [x] Resource limits extraction from ChronikClusterSpec
- [x] Implement `src/controllers/autoscaler_controller.rs`
  - [x] Resolve referenced ChronikCluster
  - [x] Scrape metrics from all broker nodes
  - [x] Evaluate thresholds with stabilization
  - [x] Cooldown enforcement (separate up/down cooldowns, epoch seconds)
  - [x] Patch ChronikCluster spec.replicas (Merge patch)
  - [x] Status updates (currentMetrics, conditions: ScalingActive, WithinLimits)
- [x] Wire autoscaler controller into `main.rs` (5 controllers in tokio::select!)
- [x] Unit tests: metrics scraper + Prometheus parser (5 tests)
- [x] Unit tests: scaling decision engine (4 tests)
- [x] Unit tests: cooldown, stabilization, and quantity parsing (6 tests)
- [x] Unit tests: autoscaler controller helpers (3 tests)

### Phase 6: Helm Chart & Deployment
- [x] Create `charts/chronik-operator/Chart.yaml`
- [x] Create `charts/chronik-operator/values.yaml`
- [x] Create `charts/chronik-operator/templates/deployment.yaml` (with POD_NAME/POD_NAMESPACE env, health probes)
- [x] Create `charts/chronik-operator/templates/serviceaccount.yaml`
- [x] Create `charts/chronik-operator/templates/clusterrole.yaml` (CRDs + core + PDB + Lease + metrics.k8s.io)
- [x] Create `charts/chronik-operator/templates/clusterrolebinding.yaml`
- [x] Create `charts/chronik-operator/crds/` (all 5 CRDs auto-generated via `crd-gen`)
- [x] Create `charts/chronik-operator/templates/_helpers.tpl`
- [x] Create `charts/chronik-operator/templates/NOTES.txt`
- [x] Implement `src/leader_election.rs` (Lease-based with annotation epoch timestamps, optimistic concurrency)
  - [x] Lease creation, renewal, acquisition of expired leases
  - [x] Graceful step-down on shutdown
  - [x] `LeaderStatus` with `Arc<AtomicBool>` for cross-task sharing
  - [x] Configurable via `POD_NAME` / `POD_NAMESPACE` env vars
  - [x] 5 unit tests
- [x] Implement `src/metrics.rs` (operator /metrics + /healthz + /readyz)
  - [x] `chronik_operator_reconciliations_total` counter (controller, result labels)
  - [x] `chronik_operator_reconciliation_duration_seconds` histogram (controller label)
  - [x] `chronik_operator_leader` gauge (1=leader, 0=standby)
  - [x] Minimal TCP HTTP server (no extra dependencies)
  - [x] 4 unit tests
- [x] Create `Dockerfile.operator` (multi-stage Rust build, debian-slim runtime, non-root user)
- [x] Wire leader election and metrics into `main.rs` (wait for leadership before starting controllers, exit on leadership loss)

### Phase 7: Examples & Documentation
- [x] Create `examples/k8s/standalone-basic.yaml` (minimal single-node)
- [x] Create `examples/k8s/standalone-s3.yaml` (S3 tiered storage, columnar, vector search)
- [x] Create `examples/k8s/cluster-3node.yaml` (minimum production cluster)
- [x] Create `examples/k8s/cluster-5node-production.yaml` (full-featured: S3, columnar, vector, schema registry, listeners, tolerations)
- [x] Create `examples/k8s/topic-orders.yaml` (basic topic with retention config)
- [x] Create `examples/k8s/topic-events-columnar.yaml` (columnar + vector search + Tantivy)
- [x] Create `examples/k8s/user-app-producer.yaml` (SASL user with write ACLs on topics)
- [x] Create `examples/k8s/user-readonly-consumer.yaml` (read-only consumer with prefixed group ACL)
- [x] Create `examples/k8s/autoscaler.yaml` (CPU + disk + throughput metrics)
- [x] Create `examples/k8s/secrets/` (admin-api-key.yaml, s3-credentials.yaml, openai-key.yaml)
- [x] Write `crates/chronik-operator/README.md` (architecture diagram, quick start, development guide)
- [x] Write `docs/KUBERNETES_OPERATOR.md` (full user guide: installation, CRD reference, all 5 resource types, monitoring, troubleshooting)

### Phase 8: Testing & Hardening
- [x] Integration tests: admin API mock server (`tests/mock_admin_api.rs` — 6 tests: health, status, add/remove node, find leader)
- [x] Integration tests: kafka client mock (`tests/mock_admin_api.rs` — 5 tests: create/describe/delete topic, create/delete user, set ACLs)
- [x] Property tests: scaling invariants (`tests/scaling_properties.rs` — 6 tests: bounds, step size, tolerance, cooldown, no-metrics, multi-metric)
- [x] E2E test harness setup (kind) — `tests/e2e/operator/setup-kind.sh`, `run-tests.sh`, `teardown-kind.sh`
- [x] E2E: standalone lifecycle (create → verify → delete)
- [x] E2E: topic lifecycle (create → verify partitions → delete)
- [x] E2E: user lifecycle (create → verify → delete)
- [x] E2E: autoscaler lifecycle (create → verify minReplicas → delete)
- [x] CI pipeline: `.github/workflows/operator.yml` (check/lint, unit tests, integration tests, release build, CRD gen verify, Helm lint/template)
- [x] Clippy clean (`-D warnings`) — fixed 19 issues: empty doc comments, too-many-arguments, new-without-default, manual-strip, useless-vec
- [x] Formatting clean (`cargo fmt --check`)
- [x] Final review and cleanup — 111 tests passing (94 unit + 11 mock integration + 6 property), release build clean

---

## 13. Session Log

Track work done across sessions here.

| Date | Session | Work Done | Next Steps |
|------|---------|-----------|------------|
| 2026-02-06 | #1 | Research & planning complete. Explored codebase deployment model, admin API, health checks, kube-rs ecosystem. Designed CRDs, reconciliation logic, file structure, Helm chart, RBAC. Created this roadmap document. Corrected autoscaler design: removed KEDA (wrong tool for broker scaling), replaced with operator-managed scaling on broker resource metrics. Studied Strimzi Kafka operator — adopted: listener abstraction, PodDisruptionBudgets, rich status conditions with observedGeneration, future NodePool/Rebalance CRD placeholders. Added ChronikTopic CRD (declarative topic management with columnar/vector config) and ChronikUser CRD (SASL auth, ACLs, auto-generated credential Secrets). Total: 5 CRDs, 8 implementation phases. | Start Phase 1: crate scaffolding |
| 2026-02-21 | #2 | **Phase 1 complete.** Created `crates/chronik-operator/` with Cargo.toml (kube 0.87, k8s-openapi 0.20, schemars 0.8). Added to workspace. Implemented: error.rs, constants.rs, telemetry.rs, 5 CRD types with full spec/status types. CLI: `start` and `crd-gen`. 15 unit tests passing. **Phase 2 complete.** Implemented 4 resource builders + standalone controller with reconciliation loop, finalizers, status updates. 29 tests passing. | Start Phase 3 |
| 2026-02-21 | #3 | **Phase 3 complete.** Implemented full cluster controller stack. **New files:** config_generator.rs (TOML generation with peers, bind/advertise addresses, DNS names — 5 tests), admin_client.rs (HTTP client for health/status/add-node/remove-node with request/response types — 5 tests), health.rs (health polling + leader discovery), resources/pdb_builder.rs (PodDisruptionBudget — 1 test). **Extended builders:** pod_builder.rs (cluster node pods with TOML config mount, WAL/Raft/Admin ports, anti-affinity, admin API key secret — 3 new tests), service_builder.rs (headless service + per-node service with admin port — 2 new tests), configmap_builder.rs (per-node TOML ConfigMap — 1 new test), pvc_builder.rs (per-node PVC — 1 new test). **Cluster controller:** Full reconciliation loop: validate min replicas, create headless Service + PDB, generate TOML configs, create per-node resources (Service, PVC, ConfigMap, Pod), scale down by removing excess nodes, compute status from Pod states, phase state machine (Bootstrapping/Running/Degraded/Failed). ConfigMap updates on peer list changes. Wired into main.rs alongside standalone controller. **52 tests passing.** Release build clean. | Start Phase 4: Topic & User Controllers |
| 2026-02-21 | #4 | **Phase 4 complete.** Implemented topic and user management controllers. **New files:** kafka_client.rs (HTTP client for topic/user CRUD via admin API — create/delete/describe topics, update config, increase partitions, create/delete users, set ACLs, ACL rule expansion — 8 tests), topic_controller.rs (topic reconciler — ClusterRef resolution for ChronikCluster + ChronikStandalone, topic creation with full config including columnar/vector/searchable flags, partition increase with never-decrease semantics, config diff and update, deletion with retain policy, status tracking — 5 tests), user_controller.rs (user reconciler — credentials Secret auto-generation with 32-char random passwords, owner references for GC, SASL user creation via admin API, ACL expansion from CRD rules to individual entries, cleanup on deletion — 3 tests). Shared `resolve_cluster_ref()` between topic and user controllers. Wired all 4 controllers into main.rs with tokio::select!. **68 tests passing.** Release build clean. | Start Phase 5: AutoScaler Controller |
| 2026-02-21 | #5 | **Phase 5 complete.** Implemented autoscaler controller with full metrics scraping and scaling decision engine. **New files:** scaling.rs (MetricsScraper for Prometheus /metrics + K8s Metrics API via DynamicObject/ApiResource, Prometheus text format parser, K8s quantity parsers for CPU millicores and storage bytes, scaling decision engine with tolerance bands, stabilization counters, separate up/down cooldowns, produce rate computation from counter deltas, resource limits extraction — 15 tests), autoscaler_controller.rs (reconciler: resolve ChronikCluster, scrape broker metrics, compute produce rate, evaluate scaling with stabilization state, patch cluster replicas via Merge patch, update status with ScalingActive/WithinLimits conditions and per-metric current values — 3 tests). In-memory stabilization state via `Mutex<HashMap>`. Wired all 5 controllers into main.rs. **85 tests passing.** Release build clean. | Start Phase 6: Helm Chart & Deployment |
| 2026-02-21 | #6 | **Phase 6 complete.** Full Helm chart and deployment infrastructure. **Helm chart** (`charts/chronik-operator/`): Chart.yaml, values.yaml (replicaCount, image, leaderElection, metrics, resources, RBAC), templates (deployment with POD_NAME/POD_NAMESPACE downward API, health probes, ServiceAccount, ClusterRole with full RBAC for 5 CRDs + core resources + PDB + Lease + metrics.k8s.io, ClusterRoleBinding, _helpers.tpl, NOTES.txt). **5 CRD YAML files** auto-generated via `crd-gen` command into `crds/` directory. **New files:** leader_election.rs (Lease-based leader election using `coordination.k8s.io/v1/Lease` with epoch annotations for time tracking, optimistic concurrency via `replace` + 409 conflict handling, graceful step-down, `LeaderStatus` with `Arc<AtomicBool>` — 5 tests), metrics.rs (HTTP server serving `/metrics` Prometheus endpoint + `/healthz` + `/readyz` probes, global metrics: reconciliations counter, duration histogram, leader gauge — 4 tests). **Dockerfile.operator** (multi-stage Rust build, debian-slim runtime, non-root user). Wired leader election and metrics into main.rs: wait for leadership before starting controllers, exit on leadership loss for K8s restart. **94 tests passing.** Release build clean. | Start Phase 7: Examples & Documentation |
| 2026-02-21 | #7 | **Phase 7 complete.** Full examples and documentation. **Example manifests** (`examples/k8s/`): standalone-basic.yaml, standalone-s3.yaml (S3 + columnar + vector), cluster-3node.yaml, cluster-5node-production.yaml (full-featured with listeners, tolerations, schema registry), topic-orders.yaml, topic-events-columnar.yaml (columnar + vector + searchable), user-app-producer.yaml (SASL + write ACLs), user-readonly-consumer.yaml (read-only + prefixed group), autoscaler.yaml (CPU + disk + throughput), secrets/ (admin key, S3 creds, OpenAI key). **Documentation:** crates/chronik-operator/README.md (architecture diagram, quick start, CLI reference, development guide), docs/KUBERNETES_OPERATOR.md (comprehensive user guide covering installation, all 5 CRD types with field references, scaling, monitoring, troubleshooting). **94 tests passing.** | Start Phase 8: Testing & Hardening |
| 2026-02-21 | #8 | **Phase 8 complete.** Testing & hardening across 3 layers. **Mock integration tests** (`tests/mock_admin_api.rs` — 11 tests): built reusable `MockServer` with `TcpListener` + request recording; tested AdminClient (health, status, add/remove node, find leader across 2 mock servers) and KafkaAdminClient (create/describe/delete topic, create/delete user, set ACLs) with path/method/body assertions. **Property tests** (`tests/scaling_properties.rs` — 6 tests): exhaustive invariant verification — scaling never exceeds [min,max] bounds, step size always ±1, no action within tolerance band, cooldown blocks rapid scaling, no metrics → NoChange, multi-metric evaluation (disk critical + CPU fine → scale up). **E2E harness** (`tests/e2e/operator/`): kind cluster setup with CRD generation and operator background process, 4 lifecycle tests (standalone, topic, user, autoscaler), teardown with process cleanup. **CI pipeline** (`.github/workflows/operator.yml`): 4 jobs (check/lint, unit+integration tests, release build + CRD gen verify, Helm lint+template). **Code quality:** Fixed 19 clippy warnings (`-D warnings`): `empty_line_after_doc_comments` → module doc, `too_many_arguments` (7 functions), `new_without_default` (2 types), `manual_strip` (11 instances), `useless_vec`. **111 tests passing** (94 unit + 11 mock + 6 property). Release build clean. | All 8 phases complete |
| | | | |

---

## Key Reference Files

These existing files are critical for the operator implementation:

| File | Why |
|------|-----|
| `crates/chronik-config/src/cluster.rs` | ClusterConfig struct — exact TOML schema the operator must generate |
| `crates/chronik-server/src/admin_api.rs` | Admin API endpoints + request/response types (JSON contract) |
| `tests/cluster/node1.toml` | Canonical example of valid cluster config |
| `crates/chronik-server/src/main.rs` | Server CLI — env vars, ports, startup logic |
| `Cargo.toml` (root) | Workspace config — add operator crate here |
| `Dockerfile.binary` | Reference for building container images |
| `crates/chronik-monitoring/src/server.rs` | Metrics/health endpoint patterns |
