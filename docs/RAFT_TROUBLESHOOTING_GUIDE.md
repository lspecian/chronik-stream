# Chronik Raft Cluster Troubleshooting Guide

**Version**: 2.0.0
**Last Updated**: 2025-10-22

This guide helps diagnose and resolve common issues with Chronik Raft clusters.

---

## Table of Contents

1. [Quick Diagnostics](#quick-diagnostics)
2. [Leader Election Issues](#leader-election-issues)
3. [Network & Connectivity](#network--connectivity)
4. [Snapshot & Log Compaction](#snapshot--log-compaction)
5. [Performance Issues](#performance-issues)
6. [Data Consistency](#data-consistency)
7. [Recovery Procedures](#recovery-procedures)

---

## Quick Diagnostics

### Health Check Command

Run this first to identify issues:

```bash
#!/bin/bash
# quick-health.sh

NODES=("localhost:9092" "localhost:9093" "localhost:9094")

for node in "${NODES[@]}"; do
  echo "=== $node ==="
  
  # Check reachability
  if ! curl -s --connect-timeout 2 http://$node/metrics >/dev/null 2>&1; then
    echo "❌ UNREACHABLE"
    continue
  fi
  
  # Check if leader
  is_leader=$(curl -s http://$node/metrics 2>/dev/null | \
    grep 'raft_is_leader.*__meta' | awk '{print $2}')
  
  if [ "$is_leader" == "1" ]; then
    echo "✅ LEADER"
  else
    echo "✅ FOLLOWER"
  fi
  
  # Check commit index
  commit=$(curl -s http://$node/metrics 2>/dev/null | \
    grep 'raft_commit_index.*__meta' | awk '{print $2}')
  echo "Commit index: $commit"
  
  # Check term
  term=$(curl -s http://$node/metrics 2>/dev/null | \
    grep 'raft_current_term' | awk '{print $2}')
  echo "Term: $term"
  
  echo ""
done
```

### Log Analysis

```bash
# Check for errors
journalctl -u chronik --since "10 minutes ago" | grep -i "error\|fatal\|panic"

# Check leader elections
journalctl -u chronik --since "1 hour ago" | grep "became leader"

# Check snapshot activity
journalctl -u chronik --since "1 hour ago" | grep -i "snapshot"
```

---

## Leader Election Issues

### Symptom: No Leader Elected

**Logs show:**
```
WARN raft::raft - No leader elected after 30 seconds
INFO raft::raft - Starting election for term 15
```

**Root Causes:**
1. Quorum not reached (< 2/3 nodes alive)
2. Network partitions between nodes
3. Time sync issues (NTP drift)
4. Firewall blocking Raft ports

**Diagnosis:**

```bash
# 1. Check if all nodes are running
for i in 1 2 3; do
  ssh node$i "systemctl status chronik"
done

# 2. Check Raft port connectivity
for i in 1 2 3; do
  for j in 1 2 3; do
    echo "node$i -> node$j:"
    nc -zv node$j 5001
  done
done

# 3. Check time sync
for i in 1 2 3; do
  ssh node$i "date"
done
```

**Solution:**

```bash
# If < 2 nodes alive, start missing nodes
systemctl start chronik

# If network partition, check firewall
sudo iptables -L -n | grep 5001

# If time drift > 1 second, sync NTP
sudo ntpdate -s time.nist.gov
sudo systemctl restart chronik
```

### Symptom: Frequent Leader Re-elections

**Logs show:**
```
INFO raft::raft - Node 1 became leader for term 25
INFO raft::raft - Node 2 became leader for term 26
INFO raft::raft - Node 1 became leader for term 27
```

**Root Causes:**
1. Network latency spikes
2. CPU starvation (100% CPU usage)
3. Disk I/O bottleneck
4. Election timeout too low

**Diagnosis:**

```bash
# Check network latency
ping -c 10 node2.example.com

# Check CPU usage
top -bn1 | grep chronik-server

# Check disk I/O
iostat -x 1 5

# Check election timeout
grep election_timeout chronik-cluster.toml
```

**Solution:**

```bash
# Increase election timeout (if < 500ms)
# Edit chronik-cluster.toml:
[raft]
election_timeout_ms = 500  # Up from 300
heartbeat_interval_ms = 100  # Up from 30

# Restart nodes one at a time
systemctl restart chronik
```

---

## Network & Connectivity

### Symptom: "No address for peer X" Errors

**Logs show:**
```
ERROR chronik_raft::client - No address for peer 2
WARN chronik_raft::replica - Failed to send AppendEntries to peer 2
```

**Root Cause:** Peer addresses not registered before replica creation

**Solution:**

```bash
# Check peer configuration in chronik-cluster.toml
cat chronik-cluster.toml | grep -A 3 "\\[\\[peers\\]\\]"

# Verify DNS resolution
for i in 1 2 3; do
  nslookup node$i.example.com
done

# Restart with correct hostnames
CHRONIK_ADVERTISED_ADDR=node1.example.com systemctl restart chronik
```

### Symptom: gRPC Connection Failures

**Logs show:**
```
ERROR chronik_raft::rpc - gRPC connection failed: Connection refused
WARN chronik_raft::client - Peer 2 unreachable
```

**Diagnosis:**

```bash
# Check if Raft port is listening
sudo netstat -tulpn | grep :5001

# Check if firewall is blocking
sudo iptables -L -n | grep 5001

# Check if process is running
ps aux | grep chronik-server
```

**Solution:**

```bash
# Allow Raft port through firewall
sudo iptables -A INPUT -p tcp --dport 5001 -j ACCEPT

# If using ufw
sudo ufw allow 5001/tcp

# Restart chronik
systemctl restart chronik
```

---

## Snapshot & Log Compaction

### Symptom: Snapshots Not Being Created

**Logs show:**
```
WARN chronik_raft::snapshot - Snapshot creation disabled
```

**Or no snapshot logs at all**

**Diagnosis:**

```bash
# Check if snapshots are enabled
env | grep CHRONIK_SNAPSHOT

# Check log size threshold
curl -s localhost:9101/metrics | grep raft_log_entries
```

**Solution:**

```bash
# Enable snapshots
export CHRONIK_SNAPSHOT_ENABLED=true
export CHRONIK_SNAPSHOT_LOG_THRESHOLD=10000

# Restart
systemctl restart chronik

# Verify snapshots are being created
journalctl -u chronik -f | grep "Creating snapshot"
```

### Symptom: Snapshot Upload to S3 Fails

**Logs show:**
```
ERROR chronik_raft::snapshot - Failed to upload snapshot to S3: Access Denied
```

**Diagnosis:**

```bash
# Check S3 credentials
env | grep AWS_

# Test S3 access
aws s3 ls s3://chronik-prod/

# Check bucket permissions
aws s3api get-bucket-policy --bucket chronik-prod
```

**Solution:**

```bash
# Option 1: Use IAM role (recommended for EC2)
# Attach IAM role with S3 permissions to EC2 instance

# Option 2: Use access keys
export AWS_ACCESS_KEY_ID=AKIA...
export AWS_SECRET_ACCESS_KEY=...

# Restart
systemctl restart chronik
```

### Symptom: Disk Full Despite Snapshots

**Logs show:**
```
ERROR chronik_wal - Failed to write to WAL: No space left on device
```

**Diagnosis:**

```bash
# Check disk usage
df -h /var/lib/chronik/data

# Check if old segments are being deleted
ls -lht /var/lib/chronik/data/wal/ | head -20
```

**Solution:**

```bash
# Manual cleanup of old segments (if safe)
find /var/lib/chronik/data/wal -name "*.log" -mtime +7 -delete

# Increase snapshot frequency
export CHRONIK_SNAPSHOT_TIME_THRESHOLD_SECS=1800  # 30 min instead of 1 hour

# Add more disk space
# (AWS EBS example)
aws ec2 modify-volume --volume-id vol-xxx --size 200
```

---

## Performance Issues

### Symptom: Slow Produce Latency (> 500ms)

**Metrics show:**
```
chronik_produce_latency_seconds{quantile="0.99"} 2.5
```

**Root Causes:**
1. Disk I/O bottleneck (slow fsync)
2. Too many concurrent requests
3. WAL segment rotation during write
4. Network latency to replicas

**Diagnosis:**

```bash
# Check disk I/O latency
iostat -x 1 10 | grep nvme

# Check WAL flush profile
env | grep CHRONIK_WAL_PROFILE

# Check produce handler profile
env | grep CHRONIK_PRODUCE_PROFILE
```

**Solution:**

```bash
# Use high-throughput profile for bulk loads
export CHRONIK_PRODUCE_PROFILE=high-throughput
export CHRONIK_WAL_PROFILE=high

# Use NVMe SSD for data directory
# Mount with:
mount -o noatime,nodiratime /dev/nvme0n1 /var/lib/chronik/data

# Restart
systemctl restart chronik
```

### Symptom: High CPU Usage (> 90%)

**Diagnosis:**

```bash
# Profile CPU usage
top -H -p $(pgrep chronik-server)

# Check for tight loops
perf record -p $(pgrep chronik-server) -g -- sleep 10
perf report
```

**Solution:**

```bash
# Reduce heartbeat frequency (if too high)
# Edit chronik-cluster.toml:
[raft]
heartbeat_interval_ms = 100  # Up from 30 (70% traffic reduction)

# Reduce partition count (if > 1000)
# Kafka guideline: 100 partitions per node

# Add more CPU cores
# (AWS EC2 example: upgrade instance type)
```

---

## Data Consistency

### Symptom: Message Loss After Failover

**Symptom:** Producer sent 1000 messages, consumer only sees 950 after leader failure

**Root Cause:** Producer didn't wait for acks=all

**Diagnosis:**

```bash
# Check producer acks setting
# Python example
kafka-console-producer --bootstrap-server localhost:9092 \
  --topic test \
  --producer-property acks=all
```

**Solution:**

```bash
# Always use acks=all for durability
# Python example:
producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    acks='all',  # Wait for all replicas
    retries=10
)
```

### Symptom: Stale Reads from Follower

**Symptom:** Client reads from follower, sees old data

**Root Cause:** Follower lag (applied_index < commit_index)

**Diagnosis:**

```bash
# Check follower lag
curl localhost:9093/metrics | grep -E "(commit_index|applied_index)"
```

**Solution:**

```bash
# Read from leader only (default Kafka behavior)
# OR wait for read-your-writes consistency (not yet implemented in v2.0.0)

# Check follower is catching up
watch 'curl -s localhost:9093/metrics | grep applied_index'
```

---

## Recovery Procedures

### Complete Cluster Failure (All Nodes Lost)

**Scenario:** All 3 nodes failed, need to recover from S3 backups

**Steps:**

```bash
# 1. Provision new nodes with same S3 bucket
export OBJECT_STORE_BACKEND=s3
export S3_BUCKET=chronik-prod  # Same bucket as before
export S3_REGION=us-west-2

# 2. Start nodes (automatic recovery)
# On each node:
systemctl start chronik

# What happens automatically:
# - Downloads latest Raft snapshot from S3
# - Downloads metadata snapshot from S3
# - Replays WAL to restore state
# - Cluster resumes

# 3. Verify recovery
kafka-topics --list --bootstrap-server localhost:9092
kafka-console-consumer --topic orders --from-beginning --bootstrap-server localhost:9092
```

### Single Node Failure with Data Corruption

**Scenario:** Node 2 disk failed, WAL corrupted

**Steps:**

```bash
# 1. Stop corrupted node
ssh node2 "systemctl stop chronik"

# 2. Clear corrupted data
ssh node2 "rm -rf /var/lib/chronik/data/*"

# 3. Restart (will sync from leader)
ssh node2 "systemctl start chronik"

# 4. Verify sync
ssh node2 "journalctl -u chronik -f" | grep "Synced from leader"
```

### Split Brain Prevention

**Scenario:** Network partition creates 2 groups

**What Chronik Does (Automatically):**
- Quorum-based writes: Only group with 2/3 nodes accepts writes
- Minority group rejects writes with `NOT_LEADER_FOR_PARTITION`
- When partition heals, minority syncs from majority

**Verification:**

```bash
# During partition, check both groups
# Majority group (2 nodes):
curl node1:9092/metrics | grep raft_is_leader  # Should be 1

# Minority group (1 node):
curl node3:9092/metrics | grep raft_is_leader  # Should be 0
```

---

## Common Error Messages

### "Quorum lost: need 2/3 nodes"

**Meaning:** < 2 nodes are alive
**Action:** Start missing nodes

### "NotLeaderForPartition"

**Meaning:** Client sent write to follower
**Action:** Client should retry (will be routed to leader)

### "Raft log truncated: index X not found"

**Meaning:** Snapshot deleted old log entries
**Action:** Normal operation, follower will sync from snapshot

### "Failed to apply snapshot: checksum mismatch"

**Meaning:** Corrupted snapshot in S3
**Action:** Delete corrupted snapshot, re-create

---

## Getting Help

If this guide doesn't resolve your issue:

1. **Check logs:** `journalctl -u chronik --since "1 hour ago" > chronik.log`
2. **Collect metrics:** `curl localhost:9101/metrics > metrics.txt`
3. **File issue:** https://github.com/your-org/chronik-stream/issues
4. **Include:**
   - Chronik version (`chronik-server --version`)
   - Cluster size (3 nodes, 5 nodes, etc.)
   - Object storage backend (S3/GCS/Azure/Local)
   - Logs and metrics

---

**Related Guides:**
- [RAFT_DEPLOYMENT_GUIDE.md](RAFT_DEPLOYMENT_GUIDE.md)
- [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md)
- [RAFT_ARCHITECTURE.md](RAFT_ARCHITECTURE.md)
