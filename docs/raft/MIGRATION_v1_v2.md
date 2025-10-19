# Chronik v1.x to v2.0 Migration Guide (Raft Clustering)

This document provides step-by-step instructions for upgrading from Chronik v1.x (standalone) to v2.0 (Raft clustering).

## Table of Contents

- [Overview](#overview)
- [Breaking Changes](#breaking-changes)
- [Pre-Migration Checklist](#pre-migration-checklist)
- [Migration Paths](#migration-paths)
- [Step-by-Step Migration](#step-by-step-migration)
- [WAL Format Migration](#wal-format-migration)
- [Rollback Strategy](#rollback-strategy)
- [Common Issues](#common-issues)
- [Post-Migration Validation](#post-migration-validation)

## Overview

Chronik v2.0 introduces **Raft-based clustering** for distributed consensus and high availability. This is a major architectural change from v1.x's standalone mode.

**Key Changes**:
- **Data Format**: WAL format upgraded from V2 to V3 (adds Raft metadata)
- **Metadata Storage**: ChronikMetaLog now uses Raft consensus
- **Configuration**: New `[cluster]` section required for clustering
- **Network**: New gRPC port 9093 for cluster communication
- **Compatibility**: V3 WAL is backward-compatible (can read V2 records)

**Migration Timeline**:
- **Preparation**: 1-2 hours (backup, config, planning)
- **Execution**: 30 minutes per node (rolling upgrade)
- **Validation**: 1 hour (verify data integrity, test clients)

## Breaking Changes

### 1. WAL Format (V2 → V3)

**V2 Format** (v1.x):
```
┌──────────────────────────────────────┐
│ WalRecord V2                         │
├──────────────────────────────────────┤
│ magic: 0x57414C32 ("WAL2")           │
│ version: 2                           │
│ checksum: CRC32                      │
│ record_type: u8                      │
│ timestamp: i64                       │
│ canonical_data: Vec<CanonicalRecord> │
└──────────────────────────────────────┘
```

**V3 Format** (v2.0):
```
┌──────────────────────────────────────┐
│ WalRecord V3                         │
├──────────────────────────────────────┤
│ magic: 0x57414C33 ("WAL3")           │
│ version: 3                           │
│ checksum: CRC32                      │
│ record_type: u8                      │
│ timestamp: i64                       │
│ raft_metadata: Option<RaftMetadata>  │ <- NEW
│   ├─ term: u64                       │
│   ├─ index: u64                      │
│   └─ leader_id: u64                  │
│ canonical_data: Vec<CanonicalRecord> │
└──────────────────────────────────────┘
```

**Compatibility**:
- V3 reader can read V2 records (treats `raft_metadata` as `None`)
- V2 reader **cannot** read V3 records (fails on unknown fields)
- Migration converts V2 → V3 on-the-fly during first read

### 2. Configuration Changes

**V1.x** (`chronik.toml`):
```toml
# No cluster section
kafka_port = 9092
data_dir = "./data"
```

**V2.0** (`chronik.toml`):
```toml
# Kafka settings unchanged
kafka_port = 9092
data_dir = "./data"

# NEW: Cluster section required
[cluster]
enabled = true
node_id = 1
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1.example.com:9093"
seed_nodes = ["2@node2:9093", "3@node3:9093"]
```

### 3. Network Ports

| Port | V1.x | V2.0 | Protocol |
|------|------|------|----------|
| 9092 | Kafka | Kafka | TCP (Kafka protocol) |
| 8080 | Metrics | Metrics | HTTP (Prometheus) |
| 9093 | N/A | **Cluster** | gRPC (Raft) |

**Firewall Rules**: Open port 9093 between cluster nodes

### 4. Metadata Store

**V1.x**: ChronikMetaLog (single-node WAL)
**V2.0**: ChronikMetaLog with Raft consensus (replicated WAL)

**Impact**: Metadata operations (CreateTopic, etc.) now go through Raft consensus
- Slightly higher latency (1-2ms for quorum)
- Higher durability (replicated to majority)

## Pre-Migration Checklist

### 1. Backup Existing Data

**Critical**: Create full backup before migration

```bash
# Stop Chronik v1.x
systemctl stop chronik

# Backup WAL directory
tar -czf chronik-backup-$(date +%Y%m%d).tar.gz \
    /var/lib/chronik/data/wal \
    /var/lib/chronik/data/segments \
    /var/lib/chronik/chronik.toml

# Verify backup
tar -tzf chronik-backup-*.tar.gz | head

# Copy to safe location
aws s3 cp chronik-backup-*.tar.gz s3://backups/chronik/
```

### 2. Verify Current State

```bash
# Check WAL version
hexdump -C /var/lib/chronik/data/wal/__meta/wal_0_00000001.log | head -1
# Should show: 00000000  57 41 4c 32  ...  (WAL2 magic)

# Check high watermarks
curl http://localhost:8080/metrics | grep chronik_high_watermark

# List topics
kafka-topics.sh --list --bootstrap-server localhost:9092
```

### 3. Plan Cluster Topology

**Decision points**:
- Number of nodes (3 or 5 recommended for HA)
- Node IDs (1-based, unique)
- Network topology (single DC, multi-DC, WAN)
- Replication factor (default: 3)

**Example 3-node cluster**:
```
Node 1: node-id=1, addr=node1.internal:9093
Node 2: node-id=2, addr=node2.internal:9093
Node 3: node-id=3, addr=node3.internal:9093
```

### 4. Update Configuration

Create v2.0 config for each node:

**node1.toml**:
```toml
kafka_port = 9092
data_dir = "/var/lib/chronik/data"

[cluster]
enabled = true
node_id = 1
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1.internal:9093"
seed_nodes = []  # Bootstrap node
```

**node2.toml**, **node3.toml**: Similar, with respective node_ids

### 5. Check System Requirements

| Resource | V1.x | V2.0 | Notes |
|----------|------|------|-------|
| CPU | 2 cores | 4 cores | +2 for Raft consensus |
| Memory | 4GB | 8GB | +4GB for Raft logs/snapshots |
| Disk | 100GB | 150GB | +50GB for Raft logs |
| Network | 1Gbps | 1Gbps | Same |
| Ports | 9092, 8080 | 9092, 8080, **9093** | +1 port |

## Migration Paths

### Path 1: In-Place Upgrade (Single Node → Cluster)

**Use case**: Upgrade existing standalone node to clustered setup

**Pros**: Preserves all data
**Cons**: Brief downtime during upgrade (5-10 minutes)

**Steps**: See [In-Place Migration](#in-place-migration)

### Path 2: Blue-Green Migration (Zero Downtime)

**Use case**: Production systems requiring zero downtime

**Pros**: No downtime, easy rollback
**Cons**: Requires double infrastructure temporarily

**Steps**: See [Blue-Green Migration](#blue-green-migration)

### Path 3: Fresh Cluster (Greenfield)

**Use case**: New deployments or data re-ingestion acceptable

**Pros**: Clean start, simplest process
**Cons**: Lose existing data (must re-ingest)

**Steps**: See [Fresh Cluster Setup](#fresh-cluster-setup)

## Step-by-Step Migration

### In-Place Migration

**Scenario**: Upgrade standalone v1.x node to 3-node v2.0 cluster

#### Phase 1: Upgrade First Node (Bootstrap)

**Step 1**: Stop v1.x on node1
```bash
# Node 1
systemctl stop chronik
```

**Step 2**: Install v2.0 binary
```bash
# Download v2.0 release
wget https://github.com/your-org/chronik-stream/releases/download/v2.0.0/chronik-server-v2.0.0-linux-amd64

# Replace binary
cp chronik-server-v2.0.0-linux-amd64 /usr/local/bin/chronik-server
chmod +x /usr/local/bin/chronik-server

# Verify version
chronik-server version
# Output: Chronik Stream v2.0.0 (features: raft, clustering)
```

**Step 3**: Update configuration
```bash
# Backup old config
cp /etc/chronik/chronik.toml /etc/chronik/chronik.toml.v1.bak

# Create v2.0 config
cat > /etc/chronik/chronik.toml <<EOF
kafka_port = 9092
data_dir = "/var/lib/chronik/data"

[cluster]
enabled = true
node_id = 1
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1.internal:9093"
seed_nodes = []  # Bootstrap node
EOF
```

**Step 4**: Migrate WAL format (automatic)
```bash
# Start v2.0 in migration mode
chronik-server cluster --migrate-wal

# What happens:
# 1. Scans data/wal/ for V2 records
# 2. Converts V2 → V3 on-the-fly during read
# 3. Writes new records in V3 format
# 4. Logs migration progress

# Expected output:
# [INFO] WAL migration started
# [INFO] Found 1234 V2 WAL segments
# [INFO] Converting segment 0/1234: data/wal/__meta/wal_0_00000001.log
# [INFO] Converted 10000 records (V2 → V3)
# [INFO] WAL migration complete (took 12.3s)
```

**Step 5**: Verify node1 started
```bash
# Check logs
journalctl -u chronik -f

# Verify cluster health
curl http://node1.internal:8080/health
# Output: {"status":"healthy","cluster_size":1,"is_leader":true}

# Verify Kafka still works
kafka-topics.sh --list --bootstrap-server node1.internal:9092
```

#### Phase 2: Add Node 2

**Step 1**: Provision node2 with v2.0
```bash
# Node 2 (new)
# Install v2.0 binary
wget https://github.com/your-org/chronik-stream/releases/download/v2.0.0/chronik-server-v2.0.0-linux-amd64
cp chronik-server-v2.0.0-linux-amd64 /usr/local/bin/chronik-server
chmod +x /usr/local/bin/chronik-server
```

**Step 2**: Configure node2
```bash
cat > /etc/chronik/chronik.toml <<EOF
kafka_port = 9092
data_dir = "/var/lib/chronik/data"

[cluster]
enabled = true
node_id = 2
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node2.internal:9093"
seed_nodes = ["1@node1.internal:9093"]
EOF
```

**Step 3**: Start node2
```bash
systemctl start chronik

# Check logs
journalctl -u chronik -f
# Expected output:
# [INFO] Joining cluster via seed node 1@node1.internal:9093
# [INFO] Received cluster membership from node 1
# [INFO] Starting Raft log replication
# [INFO] Snapshot received from node 1 (size: 123MB)
# [INFO] Snapshot installed successfully
# [INFO] Node 2 joined cluster
```

**Step 4**: Verify cluster health
```bash
# Check cluster size
curl http://node1.internal:8080/metrics | grep chronik_cluster_nodes
# Output: chronik_cluster_nodes 2

# Verify both nodes see each other
curl http://node1.internal:8080/admin/cluster/members
# Output: [{"node_id":1,"addr":"node1.internal:9093","state":"leader"},
#          {"node_id":2,"addr":"node2.internal:9093","state":"follower"}]
```

#### Phase 3: Add Node 3

**Repeat Phase 2 steps for node3**:
- node_id = 3
- seed_nodes = ["1@node1.internal:9093", "2@node2.internal:9093"]

**Final verification**:
```bash
curl http://node1.internal:8080/metrics | grep chronik_cluster_nodes
# Output: chronik_cluster_nodes 3
```

### Blue-Green Migration

**Scenario**: Zero-downtime migration with load balancer

#### Phase 1: Deploy Green Cluster (v2.0)

**Step 1**: Provision 3 new nodes (green-1, green-2, green-3)

**Step 2**: Bootstrap green cluster
```bash
# green-1 (bootstrap)
chronik-server cluster --node-id 1 --seed-nodes ""

# green-2
chronik-server cluster --node-id 2 --seed-nodes "1@green-1:9093"

# green-3
chronik-server cluster --node-id 3 --seed-nodes "1@green-1:9093,2@green-2:9093"
```

**Step 3**: Replicate data from blue to green
```bash
# Option A: MirrorMaker (Kafka replication)
kafka-mirror-maker.sh \
  --consumer.config blue-consumer.properties \
  --producer.config green-producer.properties \
  --whitelist '.*'

# Option B: Backup & Restore
# 1. Create WAL backup from blue cluster
chronik-server backup --output blue-backup.tar.gz

# 2. Restore to green cluster
chronik-server restore --input blue-backup.tar.gz
```

#### Phase 2: Switch Traffic

**Step 1**: Verify green cluster healthy
```bash
# Test produce/consume
kafka-console-producer.sh --bootstrap-server green-1:9092 --topic test
kafka-console-consumer.sh --bootstrap-server green-1:9092 --topic test --from-beginning
```

**Step 2**: Update load balancer DNS
```bash
# Route 10% traffic to green
aws route53 change-resource-record-sets --hosted-zone-id Z123 --change-batch '{
  "Changes": [{
    "Action": "UPSERT",
    "ResourceRecordSet": {
      "Name": "kafka.example.com",
      "Type": "CNAME",
      "SetIdentifier": "green",
      "Weight": 10,
      "ResourceRecords": [{"Value": "green-lb.example.com"}]
    }
  }]
}'

# Monitor for issues

# Route 100% traffic to green
# (Repeat with Weight: 100)
```

**Step 3**: Decommission blue cluster
```bash
# Wait 24-48 hours for verification
# Then shutdown blue cluster
```

### Fresh Cluster Setup

**Scenario**: New v2.0 deployment (no existing data)

**Step 1**: Provision 3 nodes

**Step 2**: Bootstrap cluster
```bash
# See [Scenario 1: Bootstrap New Cluster](./CONFIGURATION.md#scenario-1-bootstrap-new-cluster)
```

**Step 3**: Re-ingest data from source systems

## WAL Format Migration

### Automatic Migration

**Default behavior** (v2.0):
- On startup, v2.0 detects V2 WAL segments
- Converts V2 → V3 **on-the-fly** during first read
- New writes use V3 format
- No manual intervention required

**Performance impact**:
- First read of each V2 segment: +10-20ms (one-time conversion)
- Subsequent reads: No overhead (V3 cached)

### Manual Migration (Optional)

**Pre-convert all WAL segments** before starting cluster:

```bash
# Stop Chronik v1.x
systemctl stop chronik

# Run offline migration tool
chronik-server migrate-wal \
  --input /var/lib/chronik/data/wal \
  --output /var/lib/chronik/data/wal-v3 \
  --workers 4

# What it does:
# 1. Reads all V2 WAL segments
# 2. Converts V2 → V3 records
# 3. Writes V3 segments to output directory
# 4. Validates checksums

# Expected output:
# [INFO] Found 1234 V2 WAL segments
# [INFO] Converting with 4 workers
# [INFO] Progress: 100/1234 segments (8.1%)
# [INFO] Progress: 500/1234 segments (40.5%)
# [INFO] Progress: 1000/1234 segments (81.0%)
# [INFO] Migration complete: 1234 segments, 0 errors
# [INFO] Output: /var/lib/chronik/data/wal-v3

# Replace old WAL with new
mv /var/lib/chronik/data/wal /var/lib/chronik/data/wal-v2-backup
mv /var/lib/chronik/data/wal-v3 /var/lib/chronik/data/wal

# Start v2.0
systemctl start chronik
```

**Benefits**:
- Faster startup (no on-the-fly conversion)
- Predictable performance (no conversion spikes)

**Drawbacks**:
- Longer downtime (migration time)
- Extra disk space (V2 + V3 WAL)

### Verification

**Verify V3 format**:
```bash
# Check magic bytes
hexdump -C /var/lib/chronik/data/wal/__meta/wal_0_00000001.log | head -1
# Expected: 00000000  57 41 4c 33  ...  (WAL3 magic)

# Verify Raft metadata present
chronik-server debug wal-dump \
  --path /var/lib/chronik/data/wal/__meta/wal_0_00000001.log \
  | grep raft_metadata

# Expected output:
# raft_metadata: Some(RaftMetadata { term: 1, index: 123, leader_id: 1 })
```

## Rollback Strategy

### Scenario 1: Rollback During Migration (Before V3 WAL)

**If migration fails before WAL conversion**:

```bash
# Stop v2.0
systemctl stop chronik

# Restore v1.x binary
cp /usr/local/bin/chronik-server.v1.bak /usr/local/bin/chronik-server

# Restore v1.x config
cp /etc/chronik/chronik.toml.v1.bak /etc/chronik/chronik.toml

# Start v1.x
systemctl start chronik

# Verify
curl http://localhost:8080/health
```

**Result**: Full rollback to v1.x (no data loss)

### Scenario 2: Rollback After V3 WAL (CANNOT ROLLBACK)

**CRITICAL**: Once WAL format is V3, v1.x cannot read it

**Options**:

**Option A**: Restore from backup (data loss possible)
```bash
# Stop v2.0
systemctl stop chronik

# Restore backup (V2 WAL)
tar -xzf chronik-backup-20241015.tar.gz -C /var/lib/chronik/

# Downgrade to v1.x
cp /usr/local/bin/chronik-server.v1.bak /usr/local/bin/chronik-server
cp /etc/chronik/chronik.toml.v1.bak /etc/chronik/chronik.toml

# Start v1.x
systemctl start chronik
```

**Data loss**: Any messages produced after backup was taken

**Option B**: Fix forward (recommended)
```bash
# Do NOT rollback
# Instead, fix the issue in v2.0

# Example: Fix config error
vim /etc/chronik/chronik.toml
systemctl restart chronik
```

**Prevention**: Always test migration in staging environment first

## Common Issues

### Issue 1: WAL Migration Fails

**Symptom**:
```
[ERROR] WAL migration failed: InvalidChecksum at offset 12345
```

**Cause**: Corrupted V2 WAL segment

**Solution**:
```bash
# Identify corrupted segment
chronik-server debug wal-verify --path /var/lib/chronik/data/wal

# Option A: Skip corrupted segment (data loss)
chronik-server migrate-wal --skip-errors

# Option B: Restore from backup
tar -xzf chronik-backup-*.tar.gz
```

### Issue 2: Cluster Split-Brain

**Symptom**:
```
[ERROR] Node 2 failed to join cluster: ConflictingLeader
```

**Cause**: Node 2 thinks it's leader, but node 1 is already leader

**Solution**:
```bash
# Stop node 2
systemctl stop chronik

# Clear Raft state (forces re-join)
rm -rf /var/lib/chronik/data/wal/__meta/raft_*

# Restart node 2
systemctl start chronik
```

### Issue 3: High Watermark Mismatch

**Symptom**:
```
Consumer fetches from offset 1000, but leader has HWM=900
```

**Cause**: WAL migration didn't preserve high watermarks correctly

**Solution**:
```bash
# Verify high watermarks
curl http://node1:8080/metrics | grep chronik_high_watermark

# If incorrect, manually set via admin API
curl -X POST http://node1:8080/admin/set-hwm \
  -H "Content-Type: application/json" \
  -d '{"topic":"orders","partition":0,"offset":1000}'
```

### Issue 4: Performance Degradation

**Symptom**: Produce latency increased from 5ms to 50ms after migration

**Cause**: Raft consensus overhead

**Solution**:
```bash
# Tune Raft config
vim /etc/chronik/chronik.toml

[cluster.raft]
enable_pipeline = true  # Enable pipelined replication
max_in_flight = 1000    # Increase concurrency

# Restart
systemctl restart chronik
```

## Post-Migration Validation

### 1. Verify Cluster Health

```bash
# Check all nodes joined
curl http://node1:8080/metrics | grep chronik_cluster_nodes
# Expected: chronik_cluster_nodes 3

# Check leadership
curl http://node1:8080/admin/cluster/leader
# Expected: {"node_id":1,"addr":"node1.internal:9093"}

# Check Raft state
curl http://node1:8080/admin/cluster/members
# Expected: All nodes in "follower" or "leader" state
```

### 2. Verify Data Integrity

```bash
# List topics
kafka-topics.sh --list --bootstrap-server node1:9092

# Check high watermarks match v1.x
curl http://node1:8080/metrics | grep chronik_high_watermark

# Compare with pre-migration snapshot
diff <(curl http://node1-old:8080/metrics | grep high_watermark) \
     <(curl http://node1:8080/metrics | grep high_watermark)
```

### 3. Test Produce/Consume

```bash
# Produce messages
kafka-console-producer.sh --bootstrap-server node1:9092 --topic test <<EOF
message1
message2
message3
EOF

# Consume messages
kafka-console-consumer.sh \
  --bootstrap-server node1:9092 \
  --topic test \
  --from-beginning \
  --timeout-ms 5000

# Expected output:
# message1
# message2
# message3
```

### 4. Test Failover

```bash
# Stop leader node
systemctl stop chronik  # On node1 (current leader)

# Wait for election (< 1 second)
sleep 2

# Verify new leader elected
curl http://node2:8080/admin/cluster/leader
# Expected: {"node_id":2,"addr":"node2.internal:9093"} (or node 3)

# Verify produce still works
kafka-console-producer.sh --bootstrap-server node2:9092 --topic test <<EOF
message4
EOF

# Restart node1
systemctl start chronik  # On node1

# Verify rejoined as follower
curl http://node1:8080/admin/cluster/members
```

### 5. Performance Benchmarking

```bash
# Run kafka-producer-perf-test
kafka-producer-perf-test.sh \
  --topic test \
  --num-records 100000 \
  --record-size 1024 \
  --throughput -1 \
  --producer-props bootstrap.servers=node1:9092

# Expected output (v2.0 should be within 10% of v1.x):
# 100000 records sent, 20000 records/sec, 20.00 MB/sec
# Average latency: 5.00 ms
# Max latency: 50.00 ms
# p99 latency: 15.00 ms

# Compare with v1.x benchmark results
```

## Migration Success Criteria

Before considering migration complete, verify:

- [ ] All nodes joined cluster (expected node count)
- [ ] Leadership elected and stable
- [ ] All topics present (compare with v1.x `kafka-topics --list`)
- [ ] High watermarks match v1.x (no data loss)
- [ ] Produce/consume works on all nodes
- [ ] Failover works (leader election < 1s)
- [ ] Performance within 10% of v1.x baseline
- [ ] Logs show no errors for 1 hour
- [ ] Prometheus metrics healthy

## Need Help?

- **Slack**: #chronik-support
- **GitHub Issues**: https://github.com/your-org/chronik-stream/issues
- **Documentation**: [docs/raft/](./README.md)
- **Emergency Rollback**: See [Rollback Strategy](#rollback-strategy)
