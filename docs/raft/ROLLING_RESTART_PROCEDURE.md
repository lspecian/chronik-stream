# Rolling Restart Procedure for Chronik Raft Cluster

This document describes the procedure for performing zero-downtime rolling restarts of a Chronik Raft cluster using graceful shutdown with leadership transfer.

## Overview

Chronik's graceful shutdown feature enables rolling restarts with:
- **Zero downtime** - No partition loses its leader during restart
- **Zero data loss** - All WAL data is fsynced before shutdown
- **Automatic leadership transfer** - Each node transfers leadership before stopping

## Prerequisites

- Cluster must have at least 3 nodes for quorum
- All nodes must be healthy and reachable
- Cluster must have recent successful heartbeats

## Shutdown Flow (per node)

Each node goes through the following shutdown stages:

1. **DrainRequests** (10s timeout)
   - Stop accepting new produce/fetch requests
   - Return `SHUTTING_DOWN` error to new requests
   - Wait for in-flight requests to complete

2. **TransferLeadership** (30s timeout)
   - For each partition where this node is leader:
     - Select best follower (highest applied_index in ISR)
     - Send `TransferLeader` request to Raft
     - Wait for leadership transfer to complete
   - Verify no partitions have this node as leader

3. **SyncWAL** (5s timeout)
   - Flush all pending WAL writes
   - Fsync WAL files to disk
   - Close WAL file handles

4. **Shutdown**
   - Stop Raft tick loop
   - Close network connections
   - Terminate process

## Rolling Restart Procedure

### Step 1: Verify Cluster Health

Before starting, ensure the cluster is healthy:

```bash
# Check cluster health
curl http://node1:3000/health

# Expected response:
{
  "status": "healthy",
  "partitions": {
    "test-topic-0": {
      "leader": 1,
      "in_sync_replicas": [1, 2, 3],
      "status": "healthy"
    }
  }
}
```

### Step 2: Restart Non-Leader Nodes First

Restart follower nodes first to minimize leadership transfers:

```bash
# On node2 (follower)
# Send SIGTERM to trigger graceful shutdown
kill -TERM $(pgrep chronik-server)

# Wait for shutdown to complete (check logs)
tail -f /var/log/chronik-server.log

# Expected log output:
# INFO Starting graceful shutdown for node 2
# INFO Step 1/4: Draining in-flight requests
# INFO All in-flight requests drained in 150ms
# INFO Step 2/4: Transferring leadership
# INFO No partitions where we're the leader, skipping transfer
# INFO Step 3/4: Syncing WAL to disk
# INFO WAL sync complete
# INFO Step 4/4: Shutting down
# INFO Graceful shutdown complete in 1.2s (transfers: 0 successful, 0 failed)

# Start node2 again
chronik-server --node-id 2 --cluster-config /etc/chronik/cluster.toml

# Wait for node to rejoin cluster
curl http://node2:3000/health
```

### Step 3: Restart Leader Nodes

Once follower nodes are back up, restart leader nodes:

```bash
# On node1 (leader for some partitions)
kill -TERM $(pgrep chronik-server)

# Wait for shutdown to complete
tail -f /var/log/chronik-server.log

# Expected log output:
# INFO Starting graceful shutdown for node 1
# INFO Step 1/4: Draining in-flight requests
# INFO All in-flight requests drained in 200ms
# INFO Step 2/4: Transferring leadership
# INFO Found 5 partitions where we're the leader
# INFO Transferring leadership for test-topic-0 to node 2
# INFO Successfully transferred leadership for test-topic-0 in 500ms
# INFO Transferring leadership for test-topic-1 to node 3
# INFO Successfully transferred leadership for test-topic-1 in 450ms
# ... (more transfers)
# INFO Step 3/4: Syncing WAL to disk
# INFO WAL sync complete
# INFO Step 4/4: Shutting down
# INFO Graceful shutdown complete in 8.5s (transfers: 5 successful, 0 failed)

# Start node1 again
chronik-server --node-id 1 --cluster-config /etc/chronik/cluster.toml

# Wait for node to rejoin cluster
curl http://node1:3000/health
```

### Step 4: Verify Cluster Health After Each Node

After restarting each node, verify:

```bash
# Check all partitions have leaders
curl http://node1:3000/partitions | jq '.[] | select(.leader == 0)'

# Should return empty (no partitions without leader)

# Check in-sync replicas
curl http://node1:3000/partitions | jq '.[] | .in_sync_replicas | length'

# Should return 3 for all partitions (full replication)
```

## Automated Rolling Restart Script

```bash
#!/bin/bash
# rolling_restart.sh - Automated rolling restart for Chronik cluster

set -euo pipefail

NODES=("node1" "node2" "node3")
SHUTDOWN_TIMEOUT=60  # seconds
REJOIN_TIMEOUT=30    # seconds

# Function to check node health
check_health() {
    local node=$1
    curl -sf "http://${node}:3000/health" > /dev/null
}

# Function to wait for node to shutdown
wait_for_shutdown() {
    local node=$1
    local start=$(date +%s)

    while ssh "$node" "pgrep chronik-server" > /dev/null 2>&1; do
        local now=$(date +%s)
        local elapsed=$((now - start))

        if [ $elapsed -gt $SHUTDOWN_TIMEOUT ]; then
            echo "ERROR: Node $node failed to shutdown in ${SHUTDOWN_TIMEOUT}s"
            return 1
        fi

        sleep 1
    done

    echo "Node $node shutdown complete"
}

# Function to wait for node to rejoin
wait_for_rejoin() {
    local node=$1
    local start=$(date +%s)

    while ! check_health "$node"; do
        local now=$(date +%s)
        local elapsed=$((now - start))

        if [ $elapsed -gt $REJOIN_TIMEOUT ]; then
            echo "ERROR: Node $node failed to rejoin in ${REJOIN_TIMEOUT}s"
            return 1
        fi

        sleep 1
    done

    echo "Node $node rejoined cluster"
}

# Function to restart a node
restart_node() {
    local node=$1

    echo "=== Restarting $node ==="

    # Send SIGTERM for graceful shutdown
    echo "Sending SIGTERM to $node..."
    ssh "$node" "sudo systemctl stop chronik-server"

    # Wait for shutdown
    wait_for_shutdown "$node"

    # Start node
    echo "Starting $node..."
    ssh "$node" "sudo systemctl start chronik-server"

    # Wait for rejoin
    wait_for_rejoin "$node"

    # Verify health
    if check_health "$node"; then
        echo "✓ $node successfully restarted"
    else
        echo "ERROR: $node health check failed after restart"
        return 1
    fi

    # Wait for replication to catch up
    sleep 5
}

# Main procedure
echo "Starting rolling restart of Chronik cluster"
echo "Nodes: ${NODES[*]}"

# Verify all nodes are healthy before starting
echo "Verifying initial cluster health..."
for node in "${NODES[@]}"; do
    if ! check_health "$node"; then
        echo "ERROR: Node $node is not healthy, aborting"
        exit 1
    fi
done
echo "✓ All nodes healthy"

# Restart each node sequentially
for node in "${NODES[@]}"; do
    restart_node "$node"
    echo ""
done

echo "✓ Rolling restart complete"
echo "Verifying final cluster health..."

# Final health check
for node in "${NODES[@]}"; do
    if ! check_health "$node"; then
        echo "WARNING: Node $node health check failed"
    else
        echo "✓ $node healthy"
    fi
done
```

## Monitoring During Rolling Restart

Monitor these metrics during the rolling restart:

1. **Leadership Transfers**
   ```
   chronik_leadership_transfers_total{result="success"}
   chronik_leadership_transfers_total{result="failed"}
   ```

2. **Shutdown Duration**
   ```
   chronik_shutdown_duration_seconds
   ```

3. **Partition Health**
   ```
   chronik_partition_leader_count{node="1"}
   chronik_partition_isr_count{partition="test-topic-0"}
   ```

4. **Request Latency** (should not spike during transfer)
   ```
   chronik_produce_latency_seconds{quantile="0.99"}
   chronik_fetch_latency_seconds{quantile="0.99"}
   ```

## Troubleshooting

### Leadership Transfer Timeout

If leadership transfer times out (> 30s):

**Symptoms:**
```
WARN Leadership transfer timeout for test-topic-0 after 30s
```

**Causes:**
- Target follower is not in ISR
- Network issues between leader and follower
- Target follower is overloaded

**Resolution:**
1. Check follower applied_index: `curl http://follower:3000/partitions`
2. Check network connectivity: `ping follower`
3. Increase transfer timeout: `CHRONIK_TRANSFER_TIMEOUT=60 chronik-server`

### Node Fails to Rejoin Cluster

If node fails to rejoin after restart:

**Symptoms:**
```
ERROR Failed to join cluster: no leader elected within 30s
```

**Causes:**
- Quorum lost (too many nodes down)
- Network partition
- Clock skew

**Resolution:**
1. Check quorum: Need (N/2)+1 nodes alive
2. Check network: `telnet other-node 5001`
3. Check clocks: `ntpdate -q pool.ntp.org`

### Drain Timeout with In-Flight Requests

If request draining times out:

**Symptoms:**
```
WARN Drain timeout reached with 3 in-flight requests remaining
```

**Causes:**
- Long-running fetch requests (max.poll.interval.ms)
- Stuck client connections
- Network issues

**Resolution:**
1. Increase drain timeout: `CHRONIK_DRAIN_TIMEOUT=30 chronik-server`
2. Check client timeouts: `request.timeout.ms`
3. Force shutdown if necessary: `kill -KILL $(pgrep chronik-server)`

## Best Practices

1. **Always restart followers first** - Minimizes leadership transfers
2. **Wait for full ISR before next node** - Ensures quorum maintained
3. **Monitor metrics during restart** - Catch issues early
4. **Test rolling restart in staging** - Verify procedure works
5. **Have rollback plan** - Be prepared to revert if issues arise
6. **Schedule during low traffic** - Reduces impact of brief latency spikes

## Advanced Configuration

### Custom Shutdown Timeouts

```bash
# Increase timeouts for large clusters
export CHRONIK_TRANSFER_TIMEOUT=60    # 60s for leadership transfer
export CHRONIK_DRAIN_TIMEOUT=20       # 20s for request drain
export CHRONIK_WAL_SYNC_TIMEOUT=10    # 10s for WAL sync

chronik-server --node-id 1 --cluster-config /etc/chronik/cluster.toml
```

### Systemd Integration

```ini
# /etc/systemd/system/chronik-server.service

[Unit]
Description=Chronik Stream Server
After=network.target

[Service]
Type=simple
User=chronik
ExecStart=/usr/local/bin/chronik-server --node-id 1 --cluster-config /etc/chronik/cluster.toml
ExecStop=/bin/kill -TERM $MAINPID
TimeoutStopSec=90
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Key points:
- `ExecStop` sends SIGTERM for graceful shutdown
- `TimeoutStopSec=90` allows time for leadership transfer (default 30s + buffer)
- `Restart=on-failure` auto-restarts on crashes (not on graceful shutdown)

## Verification Checklist

After completing rolling restart:

- [ ] All nodes are healthy (`/health` returns 200)
- [ ] All partitions have leaders (no `leader=0`)
- [ ] All partitions have full ISR (`in_sync_replicas.length == 3`)
- [ ] No leadership transfer failures in logs
- [ ] Request latency returned to normal (< 100ms p99)
- [ ] No error rate spikes in metrics
- [ ] Consumer lag is normal (< 1000 messages)

## Example: 3-Node Cluster Rolling Restart

```bash
# Initial state
curl http://node1:3000/partitions
# test-topic-0: leader=1, isr=[1,2,3]
# test-topic-1: leader=2, isr=[1,2,3]
# test-topic-2: leader=3, isr=[1,2,3]

# Step 1: Restart node2 (follower for test-topic-0, leader for test-topic-1)
kill -TERM $(pgrep chronik-server)  # on node2
# Leadership transfers: test-topic-1 -> node3
# Result: test-topic-1: leader=3, isr=[1,3]

systemctl start chronik-server  # on node2
# Result: test-topic-1: leader=3, isr=[1,2,3]

# Step 2: Restart node3 (follower for test-topic-0, leader for test-topic-1,2)
kill -TERM $(pgrep chronik-server)  # on node3
# Leadership transfers: test-topic-1 -> node1, test-topic-2 -> node1
# Result: test-topic-1: leader=1, isr=[1,2]
#         test-topic-2: leader=1, isr=[1,2]

systemctl start chronik-server  # on node3
# Result: test-topic-1: leader=1, isr=[1,2,3]
#         test-topic-2: leader=1, isr=[1,2,3]

# Step 3: Restart node1 (leader for test-topic-0,1,2)
kill -TERM $(pgrep chronik-server)  # on node1
# Leadership transfers: test-topic-0 -> node2, test-topic-1 -> node2, test-topic-2 -> node3
# Result: test-topic-0: leader=2, isr=[2,3]
#         test-topic-1: leader=2, isr=[2,3]
#         test-topic-2: leader=3, isr=[2,3]

systemctl start chronik-server  # on node1
# Result: test-topic-0: leader=2, isr=[1,2,3]
#         test-topic-1: leader=2, isr=[1,2,3]
#         test-topic-2: leader=3, isr=[1,2,3]

# Final state: All partitions have full ISR, cluster healthy
```

## Summary

- **Total downtime**: 0 (all partitions remain available)
- **Average leadership transfer time**: 500ms per partition
- **Average node restart time**: 10s (shutdown 8s + startup 2s)
- **Total rolling restart time**: ~30s for 3-node cluster
- **Client impact**: Brief 50-100ms latency spike during leadership transfers
