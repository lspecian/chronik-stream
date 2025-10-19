# Raft Troubleshooting Guide

Comprehensive guide to diagnosing and fixing common Raft clustering issues in Chronik.

## Table of Contents

- [Common Issues](#common-issues)
  - [Leader Election Stuck or Slow](#leader-election-stuck-or-slow)
  - [Follower Lag High](#follower-lag-high)
  - [Split-Brain Detection](#split-brain-detection)
  - [Snapshot Size Too Large](#snapshot-size-too-large)
  - [Metadata Replication Slow](#metadata-replication-slow)
  - [Node Won't Rejoin Cluster](#node-wont-rejoin-cluster)
- [Diagnostic Commands](#diagnostic-commands)
- [Log Analysis](#log-analysis)
- [Metrics Interpretation](#metrics-interpretation)
- [Recovery Procedures](#recovery-procedures)

---

## Common Issues

### Leader Election Stuck or Slow

**Symptoms:**
- Cluster takes > 5 seconds to elect leader after failure
- Logs show repeated "Starting election for term N" messages
- No leader elected after 30+ seconds

**Causes:**
1. Network partitions between nodes
2. `election_timeout_ms` set too high
3. Quorum not achieved (< 2/3 nodes alive)
4. Clock skew between nodes

**Diagnosis:**

```bash
# Check cluster connectivity
for node in node1 node2 node3; do
  echo "Checking $node..."
  curl -s http://$node:9092/metrics | grep raft_is_leader
done

# Check Raft term (should be stable once leader elected)
curl http://node1:9092/metrics | grep 'raft_term{topic="__meta"'

# Check logs for election messages
journalctl -u chronik | grep -i "election"
```

**Fix:**

1. **Verify network connectivity:**
```bash
# From node1, ping node2 and node3
ping -c 3 node2
ping -c 3 node3

# Check Raft port connectivity
nc -zv node2 9093
nc -zv node3 9093
```

2. **Lower election timeout (if network is stable):**
```toml
[raft]
election_timeout_ms = 200  # Down from 300
```

3. **Check quorum:**
```bash
# Need at least 2/3 nodes for quorum
curl http://node1:9092/metrics | grep cluster_quorum_size
```

4. **Sync system clocks (NTP):**
```bash
# Install NTP
sudo apt-get install ntp

# Check time sync
ntpq -p

# Force sync
sudo ntpdate pool.ntp.org
```

**Prevention:**
- Use NTP on all nodes
- Set `election_timeout_ms` appropriate for your network latency
- Monitor network latency with Prometheus

---

### Follower Lag High

**Symptoms:**
- Follower's `applied_index` falls behind leader's `commit_index` by > 10,000 entries
- ISR shrinks (followers marked out-of-sync)
- Produce requests fail with `NOT_ENOUGH_REPLICAS_AFTER_APPEND`

**Causes:**
1. Slow disk I/O on follower
2. Network congestion or packet loss
3. Follower CPU saturated
4. Too many partitions per node

**Diagnosis:**

```bash
# Check ISR status
curl http://node1:9092/metrics | grep partition_isr_size

# Check lag for each node
for node in node1 node2 node3; do
  echo "$node:"
  curl -s http://$node:9092/metrics | grep raft_applied_index
done

# Check disk I/O stats
iostat -x 1 10

# Check CPU usage
top -b -n 1 | grep chronik
```

**Fix:**

1. **Increase ISR lag thresholds (temporary):**
```toml
[isr]
max_lag_ms = 20000       # Increase from 10000
max_lag_entries = 20000  # Increase from 10000
```

2. **Upgrade follower hardware:**
- Switch to NVMe SSD for faster WAL writes
- Add more CPU cores
- Increase RAM for better caching

3. **Reduce load per node:**
```bash
# Rebalance partitions
curl -X POST http://node1:9092/admin/rebalance
```

4. **Increase Raft batch size (if network is good):**
```toml
[raft]
max_entries_per_batch = 500  # Increase from 100
```

**Prevention:**
- Monitor `raft_follower_lag_entries` metric
- Alert when lag > 5000 entries
- Provision adequate disk I/O (> 10K IOPS)

---

### Split-Brain Detection

**Symptoms:**
- Multiple nodes claim to be leader
- Logs show "WARNING: Detected split-brain scenario"
- Produce requests succeed on some nodes but not others
- Data divergence between nodes

**Causes:**
1. Network partition splitting cluster
2. Firewall rules blocking Raft ports
3. Bug in leader election logic

**Diagnosis:**

```bash
# Check leader status on all nodes
for node in node1 node2 node3; do
  echo "$node:"
  curl -s http://$node:9092/metrics | grep 'raft_is_leader{topic="__meta"'
done

# Should see only ONE node with value "1"
# If multiple nodes show "1", split-brain exists

# Check Raft term (should be identical on all nodes)
for node in node1 node2 node3; do
  echo "$node:"
  curl -s http://$node:9092/metrics | grep 'raft_term{topic="__meta"'
done
```

**Fix:**

**CRITICAL: Split-brain can cause data loss. Immediate action required.**

1. **Stop all nodes immediately:**
```bash
# On all nodes
systemctl stop chronik
```

2. **Identify the correct leader:**
```bash
# Check which node has highest commit_index
grep "commit_index" /data/chronik/wal/__meta/0/wal_*.log | sort -t: -k2 -n | tail -1
```

3. **Restore quorum:**
```bash
# Start nodes in this order:
# 1. Node with highest commit_index (becomes leader)
systemctl start chronik

# 2. Wait 30s, then start second node
sleep 30
systemctl start chronik

# 3. Wait 30s, then start third node
sleep 30
systemctl start chronik
```

4. **Verify single leader:**
```bash
curl http://node1:9092/metrics | grep raft_is_leader
curl http://node2:9092/metrics | grep raft_is_leader
curl http://node3:9092/metrics | grep raft_is_leader
# Only ONE should show "1"
```

**Prevention:**
- Enable proper firewall rules (allow Raft port 9093)
- Use dedicated network for Raft communication
- Monitor `raft_term` metric for unexpected increases

---

### Snapshot Size Too Large

**Symptoms:**
- Snapshot creation takes > 5 minutes
- Snapshot files > 1GB
- High CPU usage during snapshot creation
- Slow recovery from snapshots

**Causes:**
1. `snapshot_threshold` set too high
2. Large metadata state (many topics/partitions)
3. Snapshot compression disabled

**Diagnosis:**

```bash
# Check snapshot size
aws s3 ls s3://chronik-prod/snapshots/__meta/0/ --human-readable

# Check snapshot creation frequency
journalctl -u chronik | grep "Created snapshot"

# Check current log size
du -h /data/chronik/wal/__meta/0/
```

**Fix:**

1. **Lower snapshot threshold:**
```toml
[raft]
snapshot_threshold = 5000  # Down from 10000
```

2. **Enable compression:**
```toml
[raft.snapshot]
compression = "zstd"  # Faster than gzip
```

3. **Cleanup old data:**
```bash
# Delete old metadata WAL segments (after snapshot created)
# Chronik does this automatically, but can trigger manually
curl -X POST http://node1:9092/admin/compact-wal
```

**Prevention:**
- Monitor snapshot size metrics
- Set appropriate `snapshot_threshold` for your workload
- Use `zstd` compression (3-4x reduction)

---

### Metadata Replication Slow

**Symptoms:**
- Topic creation takes > 5 seconds
- Consumer offset commits slow (> 1 second)
- Metadata operations timeout

**Causes:**
1. Metadata partition (`__meta`) leader overloaded
2. Network latency between nodes
3. Disk I/O bottleneck on metadata WAL

**Diagnosis:**

```bash
# Check metadata partition leader
curl http://node1:9092/metrics | grep 'raft_is_leader{topic="__meta"'

# Check metadata operation latency
curl http://node1:9092/metrics | grep metadata_operation_duration

# Check WAL write latency
iostat -x 1 5
```

**Fix:**

1. **Ensure metadata WAL on fast disk:**
```bash
# Symlink metadata WAL to NVMe SSD
mv /data/chronik/wal/__meta /nvme/chronik/__meta
ln -s /nvme/chronik/__meta /data/chronik/wal/__meta
```

2. **Increase Raft batch size for metadata:**
```toml
[raft]
max_entries_per_batch = 200
```

3. **Transfer metadata leadership to less loaded node:**
```bash
curl -X POST http://node1:9092/admin/transfer-leader?topic=__meta&partition=0&target=2
```

**Prevention:**
- Use dedicated SSD for metadata WAL
- Monitor `metadata_operation_duration_seconds` metric
- Distribute partition leadership evenly

---

### Node Won't Rejoin Cluster

**Symptoms:**
- Node starts but never becomes follower
- Logs show "Failed to connect to peers"
- Node commit_index stuck at old value

**Causes:**
1. Firewall blocking Raft port
2. Stale Raft state on disk
3. Cluster configuration mismatch
4. Node ID conflict

**Diagnosis:**

```bash
# Check peer connectivity from failing node
nc -zv node2 9093
nc -zv node3 9093

# Check cluster config matches
diff /etc/chronik/cluster.toml /tmp/cluster-from-working-node.toml

# Check Raft logs
journalctl -u chronik | grep "Failed to connect"

# Check node ID in metrics
curl http://node1:9092/metrics | grep node_id
```

**Fix:**

1. **Verify firewall rules:**
```bash
# Allow Raft port on all nodes
sudo ufw allow 9093/tcp
```

2. **Clear stale Raft state (DESTRUCTIVE - node will re-sync):**
```bash
# Stop node
systemctl stop chronik

# Backup Raft state
cp -r /data/chronik/wal/__meta /backup/

# Delete Raft state (forces rejoin from leader)
rm -rf /data/chronik/wal/__meta

# Restart node
systemctl start chronik
```

3. **Fix cluster config:**
```bash
# Ensure all nodes have identical peer list
scp node1:/etc/chronik/cluster.toml /etc/chronik/cluster.toml
systemctl restart chronik
```

4. **Check for node ID conflicts:**
```bash
# Each node must have unique node_id
grep node_id /etc/chronik/cluster.toml
```

**Prevention:**
- Use configuration management (Ansible, Terraform) to ensure consistent configs
- Test firewall rules before deploying
- Backup Raft state before major changes

---

## Diagnostic Commands

### Cluster Status

```bash
# Check which node is leader
curl -s http://node1:9092/metrics | grep 'raft_is_leader{topic="__meta"}'

# Check Raft term (all nodes should have same value)
curl -s http://node1:9092/metrics | grep 'raft_term{topic="__meta"}'

# Check commit index (leader should have highest)
curl -s http://node1:9092/metrics | grep 'raft_commit_index{topic="__meta"}'
```

### ISR Status

```bash
# Check ISR size for all partitions
curl -s http://node1:9092/metrics | grep partition_isr_size

# Check which replicas are in ISR
curl -s http://node1:9092/metrics | grep partition_isr_replicas
```

### Network Health

```bash
# Check Raft RPC latency
curl -s http://node1:9092/metrics | grep raft_rpc_duration_seconds

# Check network errors
curl -s http://node1:9092/metrics | grep raft_network_errors_total
```

### Disk Health

```bash
# Check WAL write latency
curl -s http://node1:9092/metrics | grep wal_write_duration_seconds

# Check disk I/O stats
iostat -x 1 5
```

---

## Log Analysis

### Important Log Patterns

**Leader election:**
```
INFO chronik_raft::replica - Starting election for term 5
INFO chronik_raft::replica - Became leader for term 5
```

**ISR changes:**
```
WARN chronik_raft::isr - ISR SHRINK: orders-0 removed replica 3 (lag: 15000 entries, 12000 ms)
WARN chronik_raft::isr - ISR EXPAND: orders-0 added replica 3 (lag: 500 entries, 100 ms)
```

**Snapshot creation:**
```
INFO chronik_raft::snapshot - Creating snapshot for __meta-0 at index=12345 term=5
INFO chronik_raft::snapshot - Created snapshot abc-123 for __meta-0 in 2.5s (size: 45MB)
```

**Errors to watch for:**
```
ERROR chronik_raft::replica - Failed to append entries: network timeout
ERROR chronik_raft::replica - Raft log corruption detected
WARN chronik_raft::cluster - Detected split-brain scenario
```

### Log Query Examples

```bash
# Find leader elections in last hour
journalctl -u chronik --since "1 hour ago" | grep "Became leader"

# Find ISR shrink events
journalctl -u chronik | grep "ISR SHRINK"

# Find network errors
journalctl -u chronik | grep -i "network\|timeout\|connection refused"

# Trace a specific partition
journalctl -u chronik | grep "orders-0"
```

---

## Metrics Interpretation

### Normal vs. Abnormal Values

| Metric | Normal Range | Abnormal | Action |
|--------|-------------|----------|--------|
| `raft_term` | Stable, increases rarely | Increases frequently (> 1/min) | Investigate network or election timeout |
| `raft_commit_index` | Monotonic increasing | Stuck/not increasing | Check leader, quorum |
| `raft_follower_lag_entries` | < 1000 | > 10000 | Check follower disk/CPU/network |
| `partition_isr_size` | = replication_factor | < replication_factor | Check ISR shrink logs |
| `raft_snapshot_creation_duration` | < 10s | > 60s | Lower snapshot threshold or enable compression |
| `raft_rpc_duration_seconds` | < 0.05s (p99) | > 0.5s | Check network latency |

### Setting Up Alerts

**Prometheus AlertManager rules:**

```yaml
groups:
  - name: chronik-raft
    interval: 30s
    rules:
      # Multiple leaders detected (split-brain)
      - alert: RaftSplitBrain
        expr: count(raft_is_leader{topic="__meta",partition="0"} == 1) > 1
        for: 1m
        annotations:
          summary: "Split-brain detected: multiple leaders"

      # No leader elected
      - alert: RaftNoLeader
        expr: sum(raft_is_leader{topic="__meta",partition="0"}) == 0
        for: 30s
        annotations:
          summary: "No Raft leader elected for __meta partition"

      # Follower lag high
      - alert: RaftFollowerLagHigh
        expr: raft_follower_lag_entries > 10000
        for: 5m
        annotations:
          summary: "Follower lag > 10K entries"

      # ISR below min_insync_replicas
      - alert: PartitionUnderReplicated
        expr: partition_isr_size < 2
        for: 2m
        annotations:
          summary: "Partition ISR below min_insync_replicas"
```

---

## Recovery Procedures

### Quorum Lost Recovery

**Scenario:** 2 out of 3 nodes failed, quorum lost

**Steps:**

1. **Verify quorum status:**
```bash
curl http://node1:9092/metrics | grep cluster_quorum_size
# Shows 1/3
```

2. **Bring up failed nodes:**
```bash
# On node2 and node3
systemctl start chronik
```

3. **Wait for quorum:**
```bash
# Should see quorum achieved within 30s
journalctl -u chronik -f | grep "Quorum achieved"
```

4. **Verify leader elected:**
```bash
curl http://node1:9092/metrics | grep raft_is_leader
```

### Data Corruption Recovery

**Scenario:** Raft log corruption detected

**Steps:**

1. **Stop affected node:**
```bash
systemctl stop chronik
```

2. **Backup corrupted data:**
```bash
cp -r /data/chronik/wal /backup/corrupted-wal-$(date +%Y%m%d)
```

3. **Download snapshot from S3:**
```bash
aws s3 cp s3://chronik-prod/snapshots/__meta/0/latest.snap /tmp/
```

4. **Apply snapshot:**
```bash
# Clear corrupted WAL
rm -rf /data/chronik/wal/__meta

# Chronik will auto-recover from snapshot on restart
systemctl start chronik
```

5. **Verify recovery:**
```bash
journalctl -u chronik | grep "Applied snapshot"
curl http://node1:9092/metrics | grep raft_applied_index
```

### Metadata Restore from Backup

See [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) for complete DR procedures.

---

## Next Steps

- [RAFT_DEPLOYMENT_GUIDE.md](RAFT_DEPLOYMENT_GUIDE.md) - Cluster setup
- [RAFT_CONFIGURATION_REFERENCE.md](RAFT_CONFIGURATION_REFERENCE.md) - Config tuning
- [RAFT_ARCHITECTURE.md](RAFT_ARCHITECTURE.md) - How Raft works
- [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) - S3 backup and recovery
