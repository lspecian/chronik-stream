# Production Deployment Guide

This guide covers best practices for deploying Chronik Stream in production environments.

## System Requirements

### Hardware Requirements

#### Controller Nodes (3 minimum)
- CPU: 4+ cores
- Memory: 8GB RAM
- Storage: 50GB SSD
- Network: 1Gbps

#### Ingest Nodes (5+ recommended)
- CPU: 8+ cores
- Memory: 16GB RAM
- Storage: 100GB SSD (local cache)
- Network: 10Gbps

#### Storage Requirements
- Metadata: 10GB SSD per controller node
- Object Storage: Based on retention and throughput

### Software Requirements
- Kubernetes 1.25+
- Object Storage (S3, GCS, or Azure Blob)

## Pre-Production Checklist

### Security
- [ ] Enable TLS for all connections
- [ ] Configure SASL authentication
- [ ] Set up ACLs for topics
- [ ] Enable audit logging
- [ ] Configure network policies
- [ ] Set up secrets management
- [ ] Enable encryption at rest

### High Availability
- [ ] Deploy 3+ controller nodes
- [ ] Configure replication factor ≥ 3
- [ ] Configure pod disruption budgets
- [ ] Enable leader election
- [ ] Set up health checks
- [ ] Configure automatic failover
- [ ] Enable Raft consensus for metadata

### Monitoring
- [ ] Deploy Prometheus
- [ ] Configure alerts
- [ ] Set up Grafana dashboards
- [ ] Enable distributed tracing
- [ ] Configure log aggregation
- [ ] Set up PagerDuty integration
- [ ] Create runbooks

### Performance
- [ ] Tune JVM settings
- [ ] Configure OS parameters
- [ ] Set up connection pooling
- [ ] Enable compression
- [ ] Configure batch sizes
- [ ] Tune flush intervals
- [ ] Set up caching

## Deployment

### 1. Namespace and RBAC

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: chronik-prod
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: chronik-operator
  namespace: chronik-prod
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: chronik-operator
rules:
  - apiGroups: [""]
    resources: ["pods", "services", "configmaps", "secrets"]
    verbs: ["*"]
  - apiGroups: ["apps"]
    resources: ["deployments", "statefulsets"]
    verbs: ["*"]
  - apiGroups: ["chronik.stream"]
    resources: ["chronikclusters"]
    verbs: ["*"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: chronik-operator
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: chronik-operator
subjects:
  - kind: ServiceAccount
    name: chronik-operator
    namespace: chronik-prod
```

### 2. Storage Configuration

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: chronik-ssd
provisioner: kubernetes.io/aws-ebs
parameters:
  type: gp3
  iops: "10000"
  throughput: "250"
allowVolumeExpansion: true
volumeBindingMode: WaitForFirstConsumer
```

### 3. Chronik Cluster Configuration

```yaml
apiVersion: chronik.stream/v1alpha1
kind: ChronikCluster
metadata:
  name: production
  namespace: chronik-prod
spec:
  controllers: 3
  ingestNodes: 10
  
  storage:
    backend:
      s3:
        bucket: chronik-prod-data
        region: us-east-1
        endpoint: https://s3.amazonaws.com
    size: 1Ti
    storageClass: chronik-ssd
  
  metadataStorage:
    type: sled
    volumeSize: 10Gi
    storageClass: chronik-ssd
  
  resources:
    controller:
      cpu: "4"
      memory: "8Gi"
      cpuLimit: "8"
      memoryLimit: "16Gi"
    ingest:
      cpu: "8"
      memory: "16Gi"
      cpuLimit: "16"
      memoryLimit: "32Gi"
  
  monitoring:
    prometheus: true
    tracing: true
    otlpEndpoint: jaeger-collector.monitoring:4317
  
  security:
    tls:
      enabled: true
      certSecret: chronik-tls
    authentication:
      enabled: true
      mechanisms:
        - SCRAM-SHA-256
        - SCRAM-SHA-512
```

### 5. Network Policies

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: chronik-network-policy
  namespace: chronik-prod
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/part-of: chronik
  policyTypes:
  - Ingress
  - Egress
  ingress:
  - from:
    - namespaceSelector:
        matchLabels:
          name: chronik-clients
    ports:
    - protocol: TCP
      port: 9092
  - from:
    - podSelector:
        matchLabels:
          app.kubernetes.io/part-of: chronik
  egress:
  - to:
    - podSelector:
        matchLabels:
          app.kubernetes.io/part-of: chronik
  - to:
    - namespaceSelector: {}
    ports:
    - protocol: TCP
      port: 443  # S3
    - protocol: TCP
      port: 53   # DNS
```

## Configuration Tuning

### OS Tuning

Add to `/etc/sysctl.conf`:
```bash
# Network optimizations
net.core.rmem_max = 134217728
net.core.wmem_max = 134217728
net.ipv4.tcp_rmem = 4096 87380 134217728
net.ipv4.tcp_wmem = 4096 65536 134217728
net.core.netdev_max_backlog = 5000
net.ipv4.tcp_congestion_control = bbr

# File handle limits
fs.file-max = 1000000
fs.nr_open = 1000000

# Virtual memory
vm.max_map_count = 262144
vm.swappiness = 1
```

### Chronik Configuration

```yaml
# config/production.yaml
server:
  num_network_threads: 16
  num_io_threads: 32
  socket_send_buffer_bytes: 1048576
  socket_receive_buffer_bytes: 1048576
  max_connections: 10000

storage:
  segment_size: 1073741824  # 1GB
  compression_type: zstd
  compression_level: 3
  flush_interval_ms: 100
  flush_messages: 10000

replication:
  factor: 3
  min_insync_replicas: 2
  ack_timeout_ms: 30000

performance:
  batch_size: 16384
  linger_ms: 5
  buffer_memory: 67108864  # 64MB
  prefetch_count: 10
```

## Monitoring and Alerts

### Key Metrics to Monitor

1. **Availability**
   - Cluster health status
   - Node availability
   - Leader election status

2. **Performance**
   - Message throughput (messages/sec)
   - Produce latency (p50, p95, p99)
   - Fetch latency (p50, p95, p99)
   - Consumer lag

3. **Resources**
   - CPU utilization
   - Memory usage
   - Disk I/O
   - Network bandwidth

4. **Errors**
   - Failed requests
   - Replication errors
   - Connection errors

### Alert Rules

```yaml
groups:
  - name: chronik
    rules:
    - alert: ChronikNodeDown
      expr: up{job="chronik"} == 0
      for: 5m
      annotations:
        summary: "Chronik node is down"
    
    - alert: HighProduceLatency
      expr: chronik_produce_latency_seconds{quantile="0.99"} > 0.1
      for: 10m
      annotations:
        summary: "High produce latency detected"
    
    - alert: HighConsumerLag
      expr: chronik_consumer_lag > 100000
      for: 15m
      annotations:
        summary: "Consumer lag is high"
    
    - alert: DiskSpaceLow
      expr: node_filesystem_avail_bytes{mountpoint="/data"} / node_filesystem_size_bytes < 0.1
      for: 5m
      annotations:
        summary: "Low disk space on data volume"
```

## Backup and Recovery

### Backup Strategy

1. **Metadata Backup**
   - Sled database: Daily snapshots of metadata volume
   - Retention: 30 days

2. **Data Backup**
   - Object storage: Cross-region replication
   - Segment snapshots: Weekly

3. **Configuration Backup**
   - Git repository for all configurations
   - Kubernetes resources: Daily export

### Recovery Procedures

1. **Controller Failure**
   - Automatic failover to standby
   - No manual intervention required

2. **Metadata Recovery**
   - Restore Sled database from volume snapshot
   - Verify metadata consistency
   - Restart controller nodes

3. **Data Recovery**
   - Restore segments from object storage
   - Rebuild indexes
   - Verify data integrity

## Maintenance

### Rolling Updates

```bash
# Update controller nodes
kubectl set image statefulset/chronik-controller controller=chronik/controller:v1.2.0 -n chronik-prod

# Update ingest nodes (10% at a time)
kubectl rollout pause deployment/chronik-ingest -n chronik-prod
kubectl set image deployment/chronik-ingest ingest=chronik/ingest:v1.2.0 -n chronik-prod
kubectl rollout resume deployment/chronik-ingest -n chronik-prod
```

### Scaling

```bash
# Scale ingest nodes
kubectl scale deployment/chronik-ingest --replicas=15 -n chronik-prod

# Add controller node
kubectl scale statefulset/chronik-controller --replicas=5 -n chronik-prod
```

## Troubleshooting

### Common Issues

1. **High Memory Usage**
   - Check batch sizes
   - Verify compression settings
   - Review cache configuration

2. **Slow Queries**
   - Check index health
   - Verify segment sizes
   - Review query patterns

3. **Replication Lag**
   - Check network latency
   - Verify disk I/O
   - Review replica configuration

### Debug Commands

```bash
# Check cluster status
chronik-ctl cluster info

# View controller logs
kubectl logs -n chronik-prod chronik-controller-0 -f

# Check consumer group status
chronik-ctl group get my-group

# Analyze topic partitions
chronik-ctl topic get my-topic --detailed
```

## Security Hardening

### Network Security
- Use private subnets for internal communication
- Configure firewall rules
- Enable network encryption
- Use VPN for management access

### Access Control
- Implement RBAC
- Use service accounts
- Rotate credentials regularly
- Audit access logs

### Data Protection
- Enable encryption at rest
- Use encrypted object storage
- Implement key rotation
- Secure backup storage

## Capacity Planning

### Sizing Guidelines

| Workload | Messages/sec | Ingest Nodes | Storage/day |
|----------|--------------|--------------|-------------|
| Small    | 10K          | 3            | 1TB         |
| Medium   | 100K         | 10           | 10TB        |
| Large    | 1M           | 50           | 100TB       |
| XLarge   | 10M          | 200          | 1PB         |

### Growth Planning
- Monitor usage trends
- Plan for 50% headroom
- Scale incrementally
- Test before scaling