# Chronik Raft Clustering Troubleshooting Guide

This document provides diagnostic procedures and solutions for common issues in Chronik Raft clustering.

## Table of Contents

- [Quick Diagnostic Checklist](#quick-diagnostic-checklist)
- [Common Issues](#common-issues)
- [Diagnostic Commands](#diagnostic-commands)
- [Metric Interpretation](#metric-interpretation)
- [Log Analysis](#log-analysis)
- [Advanced Debugging](#advanced-debugging)
- [Emergency Procedures](#emergency-procedures)

## Quick Diagnostic Checklist

When encountering cluster issues, run through this checklist:

```bash
# 1. Check cluster health
curl http://localhost:8080/health

# 2. Verify all nodes are reachable
curl http://node1:8080/health
curl http://node2:8080/health
curl http://node3:8080/health

# 3. Check cluster membership
curl http://node1:8080/admin/cluster/members

# 4. Verify leadership
curl http://node1:8080/admin/cluster/leader

# 5. Check Raft metrics
curl http://node1:8080/metrics | grep chronik_raft

# 6. Review recent logs
journalctl -u chronik -n 100 --no-pager

# 7. Check network connectivity
nc -zv node2 9093  # From node1
nc -zv node3 9093  # From node1

# 8. Verify disk space
df -h /var/lib/chronik

# 9. Check system resources
top -bn1 | head -20
```

If any check fails, see detailed troubleshooting below.

## Common Issues

### Issue 1: Node Cannot Join Cluster

**Symptom**:
```
[ERROR] Failed to join cluster: ConnectionRefused
[ERROR] Seed node 1@node1:9093 unreachable
```

**Possible Causes**:
1. Seed node not running
2. Network connectivity issues
3. Firewall blocking port 9093
4. Incorrect seed node address

**Diagnostic Steps**:

```bash
# Check if seed node is running
curl http://node1:8080/health
# Expected: {"status":"healthy",...}

# Test network connectivity
nc -zv node1 9093
# Expected: Connection to node1 9093 port [tcp/*] succeeded!

# Check firewall (on seed node)
sudo iptables -L -n | grep 9093
# Should NOT show DROP/REJECT for 9093

# Verify seed_nodes config
grep seed_nodes /etc/chronik/chronik.toml
# Expected: seed_nodes = ["1@node1.internal:9093", ...]
```

**Solutions**:

**Solution A**: Fix network connectivity
```bash
# Check DNS resolution
nslookup node1.internal
# If fails, add to /etc/hosts:
echo "10.0.1.10 node1.internal" >> /etc/hosts

# Test connectivity again
nc -zv node1.internal 9093
```

**Solution B**: Fix firewall
```bash
# On seed node, allow port 9093
sudo iptables -A INPUT -p tcp --dport 9093 -j ACCEPT
sudo iptables-save
```

**Solution C**: Fix configuration
```bash
# Verify seed node address format
# Correct: "1@node1.internal:9093"
# Incorrect: "node1.internal:9093" (missing node_id@)

vim /etc/chronik/chronik.toml
# Fix seed_nodes = ["1@node1.internal:9093"]

systemctl restart chronik
```

---

### Issue 2: Split-Brain (Multiple Leaders)

**Symptom**:
```
[ERROR] Raft election conflict: node 2 claims to be leader, but node 1 is already leader
[WARN] Cluster has multiple leaders: [1, 2]
```

**Possible Causes**:
1. Network partition
2. Clock skew between nodes
3. Corrupted Raft state

**Diagnostic Steps**:

```bash
# Check leadership on all nodes
curl http://node1:8080/admin/cluster/leader
curl http://node2:8080/admin/cluster/leader
curl http://node3:8080/admin/cluster/leader

# If output differs, split-brain detected

# Check network partitions
# From node1:
ping -c 3 node2
ping -c 3 node3

# Check clock skew
ssh node1 date +%s
ssh node2 date +%s
ssh node3 date +%s
# Should be within 1-2 seconds
```

**Solutions**:

**Solution A**: Fix network partition
```bash
# Identify partitioned node
# (Node that cannot reach others)

# On partitioned node, check routes
ip route show

# Fix network connectivity
# (Depends on infrastructure - contact network admin)
```

**Solution B**: Fix clock skew
```bash
# Sync clocks with NTP
sudo systemctl enable ntpd
sudo systemctl start ntpd
sudo ntpdate -u pool.ntp.org

# Verify sync
timedatectl status
```

**Solution C**: Force re-election (emergency only)
```bash
# CRITICAL: Only use if network is healthy

# Stop conflicting leader
systemctl stop chronik  # On node 2 (conflicting leader)

# Clear Raft state
rm -rf /var/lib/chronik/data/wal/__meta/raft_*

# Restart
systemctl start chronik

# Node 2 will rejoin as follower
```

---

### Issue 3: Log Replication Lag

**Symptom**:
```
[WARN] Follower node 2 is 10000 entries behind leader
[WARN] Replication lag exceeds threshold (60 seconds)
```

**Possible Causes**:
1. Slow network between nodes
2. Follower node overloaded (high CPU/disk)
3. Large AppendEntries batches

**Diagnostic Steps**:

```bash
# Check replication lag metric
curl http://node1:8080/metrics | grep chronik_raft_replication_lag_entries
# Expected: chronik_raft_replication_lag_entries{node="2"} 10000

# Check network latency
ping -c 100 node2 | tail -1
# Expected: rtt min/avg/max/mdev = 0.5/1.0/2.0/0.5 ms

# Check follower CPU/disk
ssh node2 top -bn1 | head -10
ssh node2 iostat -x 1 3
```

**Solutions**:

**Solution A**: Reduce batch size (if network is slow)
```bash
# On leader node
vim /etc/chronik/chronik.toml

[cluster.raft]
max_payload_entries = 500  # Reduce from 1000

systemctl restart chronik
```

**Solution B**: Scale up follower resources
```bash
# Increase CPU/memory on follower node
# (Depends on infrastructure - e.g., AWS EC2 instance type)

# Or reduce load on follower
# - Stop other processes
# - Reduce consumer count
```

**Solution C**: Trigger snapshot to catch up
```bash
# On leader, create snapshot
curl -X POST http://node1:8080/admin/snapshot

# Leader will send snapshot to lagging follower
# Monitor logs:
journalctl -u chronik -f | grep "InstallSnapshot"

# Expected:
# [INFO] Sending snapshot to follower 2 (size: 123MB)
# [INFO] Snapshot sent successfully to follower 2
```

---

### Issue 4: Snapshot Transfer Fails

**Symptom**:
```
[ERROR] InstallSnapshot failed: ConnectionTimeout
[ERROR] Failed to install snapshot from leader: ChunkMissing
```

**Possible Causes**:
1. Network timeout (snapshot too large)
2. Insufficient disk space on follower
3. gRPC message size limit exceeded

**Diagnostic Steps**:

```bash
# Check snapshot size
ls -lh /var/lib/chronik/data/snapshots/
# Example: snapshot_00000100.snap (500MB)

# Check network timeout settings
grep request_timeout_ms /etc/chronik/chronik.toml
# Default: 10000 (10 seconds) - too short for large snapshots

# Check disk space on follower
ssh node2 df -h /var/lib/chronik
# Ensure > 2x snapshot size available

# Check gRPC max message size
grep max_message_size /etc/chronik/chronik.toml
# Default: 4MB - too small for large snapshot chunks
```

**Solutions**:

**Solution A**: Increase timeouts
```bash
vim /etc/chronik/chronik.toml

[cluster.network]
request_timeout_ms = 300000  # 5 minutes (was 10 seconds)
install_snapshot_timeout_secs = 600  # 10 minutes

systemctl restart chronik
```

**Solution B**: Reduce snapshot size
```bash
vim /etc/chronik/chronik.toml

[cluster.raft]
snapshot_log_size_threshold = 50000  # More frequent, smaller snapshots

# Trigger compaction to reduce snapshot size
curl -X POST http://node1:8080/admin/compact
```

**Solution C**: Increase chunk size
```bash
vim /etc/chronik/chronik.toml

[cluster.raft]
max_snapshot_chunk_size = 2097152  # 2MB (was 1MB)

[cluster.network]
max_message_size = 8388608  # 8MB (was 4MB, must be > chunk size)

systemctl restart chronik
```

---

### Issue 5: High Write Latency

**Symptom**: Produce latency increased from 5ms (v1.x) to 50ms (v2.0)

**Possible Causes**:
1. Raft quorum overhead
2. Slow fsync on followers
3. Network latency between nodes
4. Disabled pipeline replication

**Diagnostic Steps**:

```bash
# Measure produce latency
kafka-producer-perf-test.sh \
  --topic test \
  --num-records 10000 \
  --record-size 1024 \
  --throughput -1 \
  --producer-props bootstrap.servers=node1:9092

# Check Raft append latency metric
curl http://node1:8080/metrics | grep chronik_raft_append_latency_ms
# Expected: p99 < 20ms

# Check network latency
ping -c 100 node2 | tail -1
ping -c 100 node3 | tail -1

# Check pipeline setting
grep enable_pipeline /etc/chronik/chronik.toml
```

**Solutions**:

**Solution A**: Enable pipeline replication
```bash
vim /etc/chronik/chronik.toml

[cluster.raft]
enable_pipeline = true
max_in_flight = 1000

systemctl restart chronik
```

**Solution B**: Tune WAL fsync
```bash
# Use group commit for better throughput
export CHRONIK_WAL_PROFILE=high
# Or edit chronik.toml

systemctl restart chronik
```

**Solution C**: Optimize network
```bash
# Enable TCP fast open
sudo sysctl -w net.ipv4.tcp_fastopen=3

# Increase TCP buffer sizes
sudo sysctl -w net.core.rmem_max=16777216
sudo sysctl -w net.core.wmem_max=16777216
```

---

### Issue 6: Leader Election Takes Too Long

**Symptom**: Leader election takes > 5 seconds after leader failure

**Possible Causes**:
1. `election_timeout_ms` set too high
2. Network latency
3. Slow disk (vote persistence)

**Diagnostic Steps**:

```bash
# Check election timeout
grep election_timeout_ms /etc/chronik/chronik.toml
# If > 500ms, may be too high for LAN

# Measure election time
# 1. Note current time
date +%s

# 2. Stop leader
systemctl stop chronik  # On current leader

# 3. Check when new leader elected
curl http://node2:8080/admin/cluster/leader

# 4. Calculate duration
date +%s  # Subtract from step 1
```

**Solutions**:

**Solution A**: Reduce election timeout
```bash
vim /etc/chronik/chronik.toml

[cluster.raft]
election_timeout_ms = 200  # Was 1000
heartbeat_interval_ms = 20  # Must be < election_timeout/10

systemctl restart chronik
```

**Solution B**: Use faster disks
```bash
# Move WAL to SSD (if currently on HDD)
systemctl stop chronik
mv /var/lib/chronik/data/wal /mnt/ssd/chronik/wal
ln -s /mnt/ssd/chronik/wal /var/lib/chronik/data/wal
systemctl start chronik
```

---

### Issue 7: Consumer Offset Commit Fails

**Symptom**:
```
[ERROR] OffsetCommit failed: NOT_COORDINATOR
[ERROR] Consumer group coordinator not found
```

**Possible Causes**:
1. Metadata partition leader changed
2. Consumer group coordinator reassignment in progress
3. Metadata Raft group not healthy

**Diagnostic Steps**:

```bash
# Check metadata partition health
curl http://node1:8080/admin/cluster/partitions | grep __meta
# Expected: {"topic":"__meta","partition":0,"leader":1,"replicas":[1,2,3],"isr":[1,2,3]}

# Check consumer group coordinator
kafka-consumer-groups.sh \
  --bootstrap-server node1:9092 \
  --describe \
  --group my-group

# Check Raft state for __meta partition
curl http://node1:8080/metrics | grep 'chronik_raft_term{partition="__meta"}'
```

**Solutions**:

**Solution A**: Wait for coordinator election
```bash
# Coordinator election takes 1-2 seconds
# Retry consumer offset commit
# Kafka clients auto-retry by default
```

**Solution B**: Manually trigger coordinator refresh
```bash
# On consumer side, refresh metadata
# (Most Kafka clients do this automatically)

# Or restart consumer application
# (Forces coordinator rediscovery)
```

---

## Diagnostic Commands

### Cluster Status

```bash
# Basic health check
curl http://localhost:8080/health

# Cluster membership
curl http://localhost:8080/admin/cluster/members | jq

# Current leader
curl http://localhost:8080/admin/cluster/leader | jq

# Partition assignments
curl http://localhost:8080/admin/cluster/partitions | jq

# Node information
curl http://localhost:8080/admin/node/info | jq
```

### Raft State

```bash
# Raft term (increases with each election)
curl http://localhost:8080/metrics | grep chronik_raft_term

# Raft log index (should increase monotonically)
curl http://localhost:8080/metrics | grep chronik_raft_log_index

# Raft state (leader/follower/candidate)
curl http://localhost:8080/metrics | grep chronik_raft_state

# Replication lag (leader only)
curl http://localhost:8080/metrics | grep chronik_raft_replication_lag

# Snapshot metadata
curl http://localhost:8080/admin/snapshot/info | jq
```

### WAL Inspection

```bash
# List WAL segments
ls -lh /var/lib/chronik/data/wal/__meta/

# Dump WAL contents
chronik-server debug wal-dump \
  --path /var/lib/chronik/data/wal/__meta/wal_0_00000001.log

# Verify WAL checksums
chronik-server debug wal-verify \
  --path /var/lib/chronik/data/wal/__meta/

# Show WAL statistics
chronik-server debug wal-stats \
  --path /var/lib/chronik/data/wal/__meta/
```

### Network Diagnostics

```bash
# Test gRPC connectivity
grpcurl -plaintext node2:9093 list

# Test Raft RPC
grpcurl -plaintext -d '{"node_id": 1}' \
  node2:9093 chronik.raft.RaftService/Heartbeat

# Measure network latency
ping -c 100 node2
ping -c 100 node3

# Check TCP connections
netstat -an | grep 9093
```

## Metric Interpretation

### Key Metrics to Monitor

| Metric | Description | Healthy Range | Alert If |
|--------|-------------|---------------|----------|
| `chronik_cluster_nodes` | Number of nodes in cluster | 3 or 5 | < expected count |
| `chronik_raft_state` | Node state (0=follower, 1=leader, 2=candidate) | 0 or 1 | 2 for > 1s |
| `chronik_raft_term` | Current Raft term | Increases slowly | Increases rapidly |
| `chronik_raft_log_index` | Last log index | Increases monotonically | Stops increasing |
| `chronik_raft_replication_lag_entries` | Follower lag (entries) | < 1000 | > 10000 |
| `chronik_raft_replication_lag_ms` | Follower lag (time) | < 1000ms | > 10000ms |
| `chronik_raft_append_latency_ms` | Log append latency (p99) | < 20ms | > 100ms |
| `chronik_raft_snapshot_size_bytes` | Snapshot size | Depends on data | Growing rapidly |

### Prometheus Queries

**Election frequency** (should be low):
```promql
rate(chronik_raft_term[5m])
# Alert if > 1/hour
```

**Replication lag**:
```promql
chronik_raft_replication_lag_entries
# Alert if > 10000
```

**Append latency**:
```promql
histogram_quantile(0.99, chronik_raft_append_latency_ms)
# Alert if > 100ms
```

## Log Analysis

### Important Log Patterns

**Normal operation**:
```
[INFO] Heartbeat sent to follower 2
[INFO] AppendEntries successful: node=2, entries=100
[INFO] Log committed: index=12345
```

**Leader election**:
```
[INFO] Election timeout reached
[INFO] Starting leader election, term=5
[INFO] RequestVote sent to node 2
[INFO] Vote granted by node 2
[INFO] Became leader for term 5
```

**Snapshot transfer**:
```
[INFO] Creating snapshot: index=100000
[INFO] Snapshot saved: size=123MB
[INFO] Sending snapshot to follower 2
[INFO] Snapshot transfer complete: node=2, size=123MB
```

**Error patterns to watch**:

```bash
# Frequent elections (unstable cluster)
journalctl -u chronik | grep "Starting leader election" | tail -20

# Replication failures
journalctl -u chronik | grep "AppendEntries failed" | tail -20

# Snapshot failures
journalctl -u chronik | grep "Snapshot.*failed" | tail -20
```

### Log Filtering

```bash
# Show only errors
journalctl -u chronik -p err --no-pager

# Show Raft-specific logs
journalctl -u chronik | grep -i raft

# Show cluster events
journalctl -u chronik | grep -E "(election|leader|follower|snapshot)"

# Show last 1 hour
journalctl -u chronik --since "1 hour ago" --no-pager
```

## Advanced Debugging

### Enable Trace Logging

```bash
# Enable trace logging for Raft
export RUST_LOG=chronik_raft=trace,chronik_cluster=trace,chronik_server=debug

systemctl restart chronik

# Watch logs
journalctl -u chronik -f
```

### Packet Capture

```bash
# Capture Raft gRPC traffic
sudo tcpdump -i eth0 -w raft-traffic.pcap port 9093

# Analyze with Wireshark
wireshark raft-traffic.pcap
```

### Performance Profiling

```bash
# Enable CPU profiling
export CHRONIK_CPU_PROFILE=true

systemctl restart chronik

# After 1 minute, stop and analyze
curl -X POST http://localhost:8080/admin/profile/stop

# Download flamegraph
curl http://localhost:8080/admin/profile/flamegraph > profile.svg
```

## Emergency Procedures

### Emergency Leader Transfer

**Use case**: Current leader is failing but still running

```bash
# Manually transfer leadership to node 2
curl -X POST http://node1:8080/admin/cluster/transfer-leader \
  -H "Content-Type: application/json" \
  -d '{"target_node_id": 2}'

# Verify new leader
curl http://node2:8080/admin/cluster/leader
```

### Emergency Cluster Rebuild

**Use case**: Cluster completely broken, data exists

```bash
# Step 1: Stop all nodes
systemctl stop chronik  # On all nodes

# Step 2: Backup data
tar -czf cluster-backup.tar.gz /var/lib/chronik/data/

# Step 3: Bootstrap new cluster from node 1
# Clear Raft state (keeps message data)
rm -rf /var/lib/chronik/data/wal/__meta/raft_*

# Edit config: seed_nodes = [] (bootstrap)
vim /etc/chronik/chronik.toml

# Start node 1
systemctl start chronik

# Step 4: Rejoin other nodes
# On node 2 and 3:
rm -rf /var/lib/chronik/data/wal/__meta/raft_*
vim /etc/chronik/chronik.toml  # seed_nodes = ["1@node1:9093"]
systemctl start chronik
```

### Force Quorum (Unsafe)

**Use case**: Quorum lost (e.g., 2 of 3 nodes failed), need to recover remaining node

**WARNING**: This can cause data loss and split-brain. Only use as last resort.

```bash
# On surviving node (node1)
systemctl stop chronik

# Edit Raft membership to remove failed nodes
chronik-server admin force-membership \
  --node-id 1 \
  --members '{"voters":[1],"learners":[]}'

systemctl start chronik

# Cluster now has quorum=1 (single node)
# Can accept writes again

# Later: Add new nodes to replace failed ones
curl -X POST http://node1:8080/admin/cluster/add-learner \
  -d '{"node_id":4,"addr":"node4:9093"}'
```

---

## Getting Help

- **GitHub Issues**: https://github.com/your-org/chronik-stream/issues
- **Slack**: #chronik-support
- **Documentation**: [docs/raft/](./README.md)

**When reporting issues, include**:
1. Output of diagnostic commands
2. Recent logs (`journalctl -u chronik -n 200`)
3. Cluster configuration (`/etc/chronik/chronik.toml`)
4. Metrics snapshot (`curl http://localhost:8080/metrics`)
5. Chronik version (`chronik-server version`)
