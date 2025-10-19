# Chronik Cluster CLI Examples

This document provides examples of all cluster management commands available in the Chronik CLI.

## Overview

The Chronik CLI provides comprehensive cluster management capabilities:
- Cluster status and monitoring
- Node management (add/remove/list)
- Partition management and information
- Rebalancing operations
- ISR (In-Sync Replicas) status
- Metadata operations
- Health checks and diagnostics

## Command Structure

```bash
chronik-server cluster [OPTIONS] <COMMAND>

Global Options:
  --addr <ADDR>         Server address [default: http://localhost:5001]
  -o, --output <OUTPUT> Output format: table, json, yaml [default: table]
```

## Examples

### 1. Cluster Status

**Basic Status:**
```bash
$ chronik-server cluster status
Cluster Status
==============
Nodes: 3
Healthy: 3
Unhealthy: 0
Metadata Leader: Node 1

┌──────┬──────────────────────┬───────────┬─────────┬────────────┐
│ Node │ Address              │ Raft Port │ Status  │ Partitions │
├──────┼──────────────────────┼───────────┼─────────┼────────────┤
│ 1    │ 192.168.1.10:9092    │ 5001      │ Healthy │ 12         │
│ 2    │ 192.168.1.11:9092    │ 5001      │ Healthy │ 12         │
│ 3    │ 192.168.1.12:9092    │ 5001      │ Healthy │ 12         │
└──────┴──────────────────────┴───────────┴─────────┴────────────┘
```

**Detailed Status:**
```bash
$ chronik-server cluster status --detailed
# Shows additional metrics like CPU, memory, disk usage per node
```

**JSON Output:**
```bash
$ chronik-server cluster status --output json
{
  "total_nodes": 3,
  "healthy_nodes": 3,
  "unhealthy_nodes": 0,
  "metadata_leader": 1,
  "nodes": [
    {
      "id": 1,
      "address": "192.168.1.10:9092",
      "raft_port": 5001,
      "status": "Healthy",
      "partition_count": 12
    },
    ...
  ]
}
```

### 2. Node Management

**Add Node:**
```bash
$ chronik-server cluster add-node --id 4 --addr 192.168.1.40:9092 --raft-port 5001
Node added successfully:
  ID: 4
  Address: 192.168.1.40:9092
  Raft Port: 5001
```

**Remove Node:**
```bash
$ chronik-server cluster remove-node --id 2
Node 2 removed successfully
```

**Force Remove (even if healthy):**
```bash
$ chronik-server cluster remove-node --id 2 --force
Node 2 removed successfully
```

**List All Nodes:**
```bash
$ chronik-server cluster list-nodes
┌──────┬──────────────────────┬───────────┬─────────┬────────────┐
│ Node │ Address              │ Raft Port │ Status  │ Partitions │
├──────┼──────────────────────┼───────────┼─────────┼────────────┤
│ 1    │ 192.168.1.10:9092    │ 5001      │ Healthy │ 12         │
│ 2    │ 192.168.1.11:9092    │ 5001      │ Healthy │ 12         │
│ 3    │ 192.168.1.12:9092    │ 5001      │ Healthy │ 12         │
└──────┴──────────────────────┴───────────┴─────────┴────────────┘
```

**List Only Healthy Nodes:**
```bash
$ chronik-server cluster list-nodes --healthy-only
```

**List Only Unhealthy Nodes:**
```bash
$ chronik-server cluster list-nodes --unhealthy-only
```

### 3. Partition Management

**List All Partitions:**
```bash
$ chronik-server cluster list-partitions
┌───────────────┬───────────┬────────┬───────────────┬───────────────┬───────────────┬─────┐
│ Topic         │ Partition │ Leader │ Replicas      │ ISR           │ Applied Index │ Lag │
├───────────────┼───────────┼────────┼───────────────┼───────────────┼───────────────┼─────┤
│ my-topic      │ 0         │ 1      │ [1, 2, 3]     │ [1, 2, 3]     │ 15023         │ 0   │
│ my-topic      │ 1         │ 2      │ [2, 3, 1]     │ [2, 3, 1]     │ 14891         │ 0   │
│ other-topic   │ 0         │ 3      │ [3, 1, 2]     │ [3, 1, 2]     │ 9234          │ 0   │
└───────────────┴───────────┴────────┴───────────────┴───────────────┴───────────────┴─────┘
```

**List Partitions for Specific Topic:**
```bash
$ chronik-server cluster list-partitions --topic my-topic
┌───────────┬───────────┬────────┬───────────────┬───────────────┬───────────────┬─────┐
│ Topic     │ Partition │ Leader │ Replicas      │ ISR           │ Applied Index │ Lag │
├───────────┼───────────┼────────┼───────────────┼───────────────┼───────────────┼─────┤
│ my-topic  │ 0         │ 1      │ [1, 2, 3]     │ [1, 2, 3]     │ 15023         │ 0   │
│ my-topic  │ 1         │ 2      │ [2, 3, 1]     │ [2, 3, 1]     │ 14891         │ 0   │
└───────────┴───────────┴────────┴───────────────┴───────────────┴───────────────┴─────┘
```

**List Partitions on Specific Node:**
```bash
$ chronik-server cluster list-partitions --node 1
# Shows only partitions where node 1 is a replica
```

**Get Partition Details:**
```bash
$ chronik-server cluster partition-info --topic my-topic --partition 0
Partition: my-topic/0
Leader: Node 1
Replicas: [1, 2, 3]
ISR: [1, 2, 3]
Applied Index: 15023
Committed Index: 15023
Lag: 0 entries
```

### 4. Rebalancing

**Dry Run (Plan Only):**
```bash
$ chronik-server cluster rebalance --dry-run
Rebalance Plan (Dry Run)
========================
Current Imbalance: 18.0%
Partitions to Move: 3

┌───────────────┬───────────┬───────────┬─────────┐
│ Topic         │ Partition │ From Node │ To Node │
├───────────────┼───────────┼───────────┼─────────┤
│ my-topic      │ 2         │ 1         │ 3       │
│ other-topic   │ 5         │ 1         │ 2       │
│ test-topic    │ 0         │ 2         │ 3       │
└───────────────┴───────────┴───────────┴─────────┘

Estimated Duration: 2m 30s
```

**Execute Rebalance:**
```bash
$ chronik-server cluster rebalance
Rebalance Executed
==================
Current Imbalance: 18.0%
Partitions to Move: 3
...
```

**Rebalance with Max Moves Limit:**
```bash
$ chronik-server cluster rebalance --max-moves 5
# Limits rebalancing to at most 5 partition moves
```

**Rebalance Single Topic:**
```bash
$ chronik-server cluster rebalance --topic my-topic
# Only rebalances partitions for my-topic
```

### 5. ISR Status

**Show All ISR Status:**
```bash
$ chronik-server cluster isr-status
┌───────────────┬───────────┬────────┬───────────────┬──────────────────┐
│ Topic         │ Partition │ Leader │ ISR           │ Under-Replicated │
├───────────────┼───────────┼────────┼───────────────┼──────────────────┤
│ my-topic      │ 0         │ 1      │ [1, 2, 3]     │ NO               │
│ test-topic    │ 1         │ 2      │ [2]           │ YES              │
└───────────────┴───────────┴────────┴───────────────┴──────────────────┘
```

**Show ISR for Specific Topic:**
```bash
$ chronik-server cluster isr-status --topic my-topic
```

**Show Only Under-Replicated Partitions:**
```bash
$ chronik-server cluster isr-status --under-replicated-only
┌───────────────┬───────────┬────────┬───────────────┬──────────────────┐
│ Topic         │ Partition │ Leader │ ISR           │ Under-Replicated │
├───────────────┼───────────┼────────┼───────────────┼──────────────────┤
│ test-topic    │ 1         │ 2      │ [2]           │ YES              │
└───────────────┴───────────┴────────┴───────────────┴──────────────────┘
```

### 6. Metadata Status

**Basic Metadata Status:**
```bash
$ chronik-server cluster metadata-status
Metadata Status
===============
Leader Node: Node 1
Total Entries: 1523
Applied Index: 1523
Committed Index: 1523
Last Updated: 2025-10-16T14:32:10Z
```

**Detailed Metadata Status:**
```bash
$ chronik-server cluster metadata-status --detailed
# Shows additional details like log size, snapshot status
```

**Replicate Metadata:**
```bash
$ chronik-server cluster metadata-replicate \
  --key topics \
  --value '{"name":"test-topic","partitions":3}'
Metadata replicated successfully:
  Key: topics
  Value: {"name":"test-topic","partitions":3}
```

### 7. Health Checks

**Cluster Health Check:**
```bash
$ chronik-server cluster health
Cluster Health Check
====================
Overall Status: HEALTHY

┌──────┬──────────────────────┬────────┬───────────────┐
│ Node │ Address              │ Status │ Response Time │
├──────┼──────────────────────┼────────┼───────────────┤
│ 1    │ 192.168.1.10:9092    │ Alive  │ 5 ms          │
│ 2    │ 192.168.1.11:9092    │ Alive  │ 7 ms          │
│ 3    │ 192.168.1.12:9092    │ Alive  │ 6 ms          │
└──────┴──────────────────────┴────────┴───────────────┘
```

**Health Check with Custom Timeout:**
```bash
$ chronik-server cluster health --timeout 10
# Uses 10 second timeout for health checks
```

**Ping Specific Node:**
```bash
$ chronik-server cluster ping-node --id 1
Ping Results for Node 1
=======================
Address: 192.168.1.10:9092
Successful Pings: 1
Failed Pings: 0
Average Response Time: 5 ms
```

**Multiple Pings:**
```bash
$ chronik-server cluster ping-node --id 1 --count 5
Ping Results for Node 1
=======================
Address: 192.168.1.10:9092
Successful Pings: 5
Failed Pings: 0
Average Response Time: 5 ms
```

### 8. Remote Server Management

**Connect to Remote Server:**
```bash
$ chronik-server cluster --addr http://remote-server:5001 status
# Manages cluster on remote-server instead of localhost
```

**Set via Environment Variable:**
```bash
$ export CHRONIK_SERVER_ADDR=http://remote-server:5001
$ chronik-server cluster status
# Automatically uses remote-server
```

## Output Formats

### Table Format (Default)
Human-readable tables with borders and alignment.

### JSON Format
```bash
$ chronik-server cluster status --output json
{
  "total_nodes": 3,
  "healthy_nodes": 3,
  ...
}
```

### YAML Format
```bash
$ chronik-server cluster status --output yaml
total_nodes: 3
healthy_nodes: 3
...
```

## Integration Examples

### Scripting
```bash
#!/bin/bash
# Check cluster health before deployment

STATUS=$(chronik-server cluster health --output json)
HEALTHY=$(echo $STATUS | jq -r '.cluster_healthy')

if [ "$HEALTHY" != "true" ]; then
  echo "Cluster is unhealthy, aborting deployment"
  exit 1
fi

echo "Cluster is healthy, proceeding with deployment"
```

### Monitoring
```bash
# Monitor under-replicated partitions
while true; do
  chronik-server cluster isr-status --under-replicated-only --output json > /tmp/isr_status.json
  COUNT=$(jq 'length' /tmp/isr_status.json)

  if [ "$COUNT" -gt 0 ]; then
    echo "WARNING: $COUNT under-replicated partitions detected"
  fi

  sleep 60
done
```

### Automated Rebalancing
```bash
# Run rebalancing during maintenance window
if [ "$(date +%H)" -eq 2 ]; then  # 2 AM
  echo "Running scheduled rebalancing..."
  chronik-server cluster rebalance --max-moves 10
fi
```

## Notes

1. **Phase 4 Implementation**: This CLI provides the command structure and parsing. Phase 5 will implement the actual RPC client to communicate with the cluster.

2. **Mock Responses**: Current implementation returns mock data. Phase 5 will replace with actual gRPC calls to the Raft cluster.

3. **Authentication**: Future versions will support TLS and authentication for secure cluster management.

4. **Bash Completion**: Generate completion scripts with:
   ```bash
   chronik-server cluster --help | grep completion
   ```

## Command Reference

| Command                 | Description                          | Options                                    |
|------------------------|--------------------------------------|-------------------------------------------|
| `status`               | Display cluster status               | `--detailed`                              |
| `add-node`             | Add new node                         | `--id`, `--addr`, `--raft-port`           |
| `remove-node`          | Remove node                          | `--id`, `--force`                         |
| `list-nodes`           | List all nodes                       | `--healthy-only`, `--unhealthy-only`      |
| `list-partitions`      | List partitions                      | `--topic`, `--node`                       |
| `partition-info`       | Get partition details                | `--topic`, `--partition`                  |
| `rebalance`            | Rebalance partitions                 | `--dry-run`, `--max-moves`, `--topic`     |
| `isr-status`           | Show ISR status                      | `--topic`, `--under-replicated-only`      |
| `metadata-status`      | Show metadata status                 | `--detailed`                              |
| `metadata-replicate`   | Replicate metadata entry             | `--key`, `--value`                        |
| `health`               | Check cluster health                 | `--timeout`                               |
| `ping-node`            | Ping specific node                   | `--id`, `--count`                         |

## See Also

- [RAFT_IMPLEMENTATION_PLAN.md](RAFT_IMPLEMENTATION_PLAN.md) - Raft consensus implementation details
- [PHASE1_COMPLETE.md](PHASE1_COMPLETE.md) - Core data structures
- Phase 5 will implement the RPC client backend
