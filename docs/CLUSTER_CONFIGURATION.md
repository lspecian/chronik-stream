# Chronik Cluster Configuration Guide

This guide explains how to configure Chronik for clustered deployment with static peer discovery.

## Overview

Chronik supports Raft-based clustering for high availability and data replication. This guide covers Phase 3 of the clustering implementation: static peer discovery via configuration files or environment variables.

## Configuration Methods

There are two ways to configure clustering:

1. **TOML Configuration File** (recommended for production)
2. **Environment Variables** (useful for containers/K8s)

### Method 1: TOML Configuration File

Create a `chronik-cluster.toml` file:

```toml
[cluster]
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[cluster.peers]]
id = 1
addr = "10.0.1.10:9092"
raft_port = 9093

[[cluster.peers]]
id = 2
addr = "10.0.1.11:9092"
raft_port = 9093

[[cluster.peers]]
id = 3
addr = "10.0.1.12:9092"
raft_port = 9093
```

Then run:

```bash
chronik-server --cluster-config chronik-cluster.toml standalone
```

### Method 2: Environment Variables

```bash
export CHRONIK_CLUSTER_ENABLED=true
export CHRONIK_NODE_ID=1
export CHRONIK_REPLICATION_FACTOR=3
export CHRONIK_MIN_INSYNC_REPLICAS=2
export CHRONIK_CLUSTER_PEERS=10.0.1.10:9092:9093,10.0.1.11:9092:9093,10.0.1.12:9092:9093

chronik-server standalone
```

## Configuration Parameters

### Cluster Settings

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `enabled` | bool | Yes | Enable clustering mode |
| `node_id` | u64 | Yes | Unique node identifier (must match a peer ID) |
| `replication_factor` | usize | Yes | Number of replicas per partition |
| `min_insync_replicas` | usize | Yes | Minimum in-sync replicas for writes |
| `peers` | array | Yes | List of all cluster nodes |

### Peer Configuration

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | u64 | Yes | Unique node identifier (positive integer) |
| `addr` | string | Yes | Kafka API address (hostname:port) |
| `raft_port` | u16 | Yes | Raft gRPC port (1024-65535) |

## Validation Rules

The cluster configuration is validated on startup with the following rules:

1. **node_id must be non-zero**: Node IDs start from 1
2. **node_id must exist in peers**: The configured node_id must match one of the peer IDs
3. **No duplicate node IDs**: Each peer must have a unique ID
4. **No duplicate addresses**: Each peer must have a unique address+port combination
5. **replication_factor <= peer count**: Cannot replicate to more nodes than exist
6. **min_insync_replicas <= replication_factor**: Cannot require more replicas than configured

## Example Deployments

### 3-Node Cluster (Production)

**Recommended configuration:**
- Replication Factor: 3 (all nodes)
- Min In-Sync Replicas: 2 (quorum)

**Node 1 (10.0.1.10):**
```bash
chronik-server --cluster-config cluster.toml --node-id 1 standalone
```

**Node 2 (10.0.1.11):**
```bash
chronik-server --cluster-config cluster.toml --node-id 2 standalone
```

**Node 3 (10.0.1.12):**
```bash
chronik-server --cluster-config cluster.toml --node-id 3 standalone
```

### 5-Node Cluster (High Availability)

**Recommended configuration:**
- Replication Factor: 5 (all nodes)
- Min In-Sync Replicas: 3 (quorum)

```toml
[cluster]
enabled = true
node_id = 1  # Change per node
replication_factor = 5
min_insync_replicas = 3

[[cluster.peers]]
id = 1
addr = "node1.example.com:9092"
raft_port = 9093

[[cluster.peers]]
id = 2
addr = "node2.example.com:9092"
raft_port = 9093

[[cluster.peers]]
id = 3
addr = "node3.example.com:9092"
raft_port = 9093

[[cluster.peers]]
id = 4
addr = "node4.example.com:9092"
raft_port = 9093

[[cluster.peers]]
id = 5
addr = "node5.example.com:9092"
raft_port = 9093
```

## Kubernetes Deployment

For Kubernetes StatefulSets, use environment variables with Helm templating:

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: chronik
spec:
  serviceName: chronik
  replicas: 3
  template:
    spec:
      containers:
      - name: chronik
        image: chronik-stream:latest
        env:
        - name: CHRONIK_CLUSTER_ENABLED
          value: "true"
        - name: CHRONIK_NODE_ID
          valueFrom:
            fieldRef:
              fieldPath: metadata.labels['statefulset.kubernetes.io/pod-name']
        - name: CHRONIK_REPLICATION_FACTOR
          value: "3"
        - name: CHRONIK_MIN_INSYNC_REPLICAS
          value: "2"
        - name: CHRONIK_CLUSTER_PEERS
          value: "chronik-0.chronik:9092:9093,chronik-1.chronik:9092:9093,chronik-2.chronik:9092:9093"
```

## Docker Compose Example

```yaml
version: '3.8'

services:
  chronik-1:
    image: chronik-stream:latest
    environment:
      CHRONIK_CLUSTER_ENABLED: "true"
      CHRONIK_NODE_ID: "1"
      CHRONIK_REPLICATION_FACTOR: "3"
      CHRONIK_MIN_INSYNC_REPLICAS: "2"
      CHRONIK_CLUSTER_PEERS: "chronik-1:9092:9093,chronik-2:9092:9093,chronik-3:9092:9093"
    ports:
      - "9092:9092"
      - "9093:9093"

  chronik-2:
    image: chronik-stream:latest
    environment:
      CHRONIK_CLUSTER_ENABLED: "true"
      CHRONIK_NODE_ID: "2"
      CHRONIK_REPLICATION_FACTOR: "3"
      CHRONIK_MIN_INSYNC_REPLICAS: "2"
      CHRONIK_CLUSTER_PEERS: "chronik-1:9092:9093,chronik-2:9092:9093,chronik-3:9092:9093"
    ports:
      - "9192:9092"
      - "9193:9093"

  chronik-3:
    image: chronik-stream:latest
    environment:
      CHRONIK_CLUSTER_ENABLED: "true"
      CHRONIK_NODE_ID: "3"
      CHRONIK_REPLICATION_FACTOR: "3"
      CHRONIK_MIN_INSYNC_REPLICAS: "2"
      CHRONIK_CLUSTER_PEERS: "chronik-1:9092:9093,chronik-2:9092:9093,chronik-3:9092:9093"
    ports:
      - "9292:9092"
      - "9293:9093"
```

## CLI Arguments

Override configuration via command-line arguments:

```bash
# Load from file and override node_id
chronik-server --cluster-config cluster.toml --node-id 2 standalone

# Specify config location via environment
CHRONIK_CLUSTER_CONFIG=/etc/chronik/cluster.toml chronik-server standalone
```

## Troubleshooting

### Configuration Validation Errors

**Error: "node_id X not found in peers list"**
- Solution: Ensure `node_id` matches one of the peer IDs in the configuration

**Error: "Duplicate node ID: X"**
- Solution: Each peer must have a unique ID

**Error: "replication_factor (5) cannot exceed peer count (3)"**
- Solution: Reduce replication_factor or add more peers

**Error: "min_insync_replicas (3) cannot exceed replication_factor (2)"**
- Solution: Reduce min_insync_replicas or increase replication_factor

### Network Issues

**Peers cannot connect:**
1. Verify firewall allows ports 9092 (Kafka) and 9093 (Raft)
2. Check DNS resolution for hostnames
3. Ensure advertised addresses are reachable from all nodes

### Startup Logs

Check for cluster configuration in logs:

```
INFO Loaded cluster configuration from environment variables
INFO   Node ID: 1
INFO   Replication Factor: 3
INFO   Min In-Sync Replicas: 2
INFO   Peers: 3
```

## Best Practices

1. **Use Quorum for min_insync_replicas**: Set to `(replication_factor / 2) + 1`
2. **Odd Number of Nodes**: Use 3, 5, or 7 nodes for Raft consensus
3. **Network Latency**: Keep nodes in same datacenter/region for best performance
4. **Monitoring**: Monitor cluster health via metrics endpoint
5. **Production**: Use TOML config files for reproducibility and version control

## Security Considerations

1. **Network Isolation**: Use private network for Raft communication
2. **TLS**: Enable TLS for both Kafka and Raft ports (future feature)
3. **Authentication**: Secure peer-to-peer communication (future feature)
4. **Firewall**: Restrict Raft port access to cluster nodes only

## Next Steps

After configuring static peer discovery:
- **Phase 4**: Implement Raft consensus (leader election, log replication)
- **Phase 5**: Add partition replication and ISR management
- **Phase 6**: Implement cluster membership changes (add/remove nodes)

## See Also

- [Raft Consensus Algorithm](https://raft.github.io/)
- [Chronik Architecture](../ARCHITECTURE.md)
- [Disaster Recovery Guide](DISASTER_RECOVERY.md)
