# Raft Metrics Guide

Comprehensive guide to monitoring Chronik Raft cluster health and performance using Prometheus metrics.

## Overview

Chronik exposes detailed Raft consensus metrics for monitoring cluster health, replication lag, elections, commit performance, snapshots, and network operations. All metrics follow Prometheus naming conventions and are available at `/metrics` endpoint (default port: 9090).

## Metrics Categories

### 1. Cluster Health Metrics

Monitor overall cluster state and partition distribution.

#### `chronik_raft_leader_count`
- **Type**: Gauge
- **Labels**: `node_id`
- **Description**: Number of partitions where this node is the Raft leader
- **Usage**: Verify leadership distribution is balanced across nodes
- **Alert If**:
  - Any node has 0 leaders (may indicate network isolation)
  - One node has significantly more leaders than others (imbalance)

```promql
# View leader distribution
chronik_raft_leader_count{node_id="node1"}

# Alert: Node has no leaders (should have at least 1)
chronik_raft_leader_count < 1
```

#### `chronik_raft_follower_count`
- **Type**: Gauge
- **Labels**: `node_id`
- **Description**: Number of partitions where this node is a Raft follower
- **Usage**: Track follower responsibilities per node

```promql
# Total partitions per node (leader + follower)
chronik_raft_leader_count{node_id="node1"} + chronik_raft_follower_count{node_id="node1"}
```

#### `chronik_raft_isr_size`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Number of replicas in the In-Sync Replica set
- **Usage**: Critical for durability guarantees
- **Alert If**: ISR size < replication factor (replicas are falling behind)

```promql
# Alert: ISR size below quorum (for 3-node cluster)
chronik_raft_isr_size < 2

# ISR size by topic
avg(chronik_raft_isr_size) by (topic)
```

#### `chronik_raft_node_state`
- **Type**: Gauge
- **Labels**: `topic`, `partition`, `node_id`
- **Values**: `1=leader`, `2=candidate`, `3=follower`, `0=unknown`
- **Description**: Current Raft state for this partition/node
- **Alert If**:
  - State = 2 (candidate) for > 30s (election in progress)
  - State = 0 (unknown) (node disconnected)

```promql
# Alert: Stuck in candidate state
chronik_raft_node_state == 2

# Count leaders across cluster
sum(chronik_raft_node_state == 1)
```

#### `chronik_raft_current_term`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Current Raft term number
- **Usage**: Detect frequent elections (rapid term increases)

```promql
# Rate of term changes (indicates elections)
rate(chronik_raft_current_term[5m]) > 0.1
```

#### `chronik_raft_commit_index` / `chronik_raft_last_applied`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Committed vs applied log indices
- **Alert If**: `commit_index - last_applied` > 1000 (state machine lag)

```promql
# State machine lag
chronik_raft_commit_index - chronik_raft_last_applied > 1000
```

---

### 2. Replication Lag Metrics

Monitor how far followers are behind the leader.

#### `chronik_raft_follower_lag_entries`
- **Type**: Gauge
- **Labels**: `topic`, `partition`, `follower_id`
- **Description**: Number of log entries a follower is behind the leader
- **Usage**: Primary replication health indicator
- **Alert If**:
  - Lag > 100 entries (follower falling behind)
  - Lag > 1000 entries (critical, near ISR removal)

```promql
# Alert: High replication lag
chronik_raft_follower_lag_entries > 100

# Max lag across all followers
max(chronik_raft_follower_lag_entries) by (topic, partition)

# Average lag by topic
avg(chronik_raft_follower_lag_entries) by (topic)
```

#### `chronik_raft_follower_lag_ms`
- **Type**: Gauge
- **Labels**: `topic`, `partition`, `follower_id`
- **Description**: Time in milliseconds a follower is behind the leader
- **Usage**: Measure replication delay impact on consumers
- **Alert If**:
  - Lag > 500ms (noticeable delay)
  - Lag > 5000ms (critical delay)

```promql
# Alert: High replication latency
chronik_raft_follower_lag_ms > 500

# p99 replication lag across cluster
histogram_quantile(0.99, sum(rate(chronik_raft_follower_lag_ms[5m])) by (le))
```

#### `chronik_raft_max_follower_lag_entries`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Maximum replication lag across all followers
- **Usage**: Single metric for overall partition replication health

```promql
# Alert: Any follower lagging significantly
chronik_raft_max_follower_lag_entries > 1000
```

#### `chronik_raft_last_heartbeat_timestamp_ms`
- **Type**: Gauge
- **Labels**: `topic`, `partition`, `follower_id`
- **Description**: Timestamp of last successful heartbeat from follower
- **Usage**: Detect unresponsive followers
- **Alert If**: `now() - last_heartbeat > 10000ms` (10 seconds)

```promql
# Alert: Follower hasn't sent heartbeat in 10s
(time() * 1000) - chronik_raft_last_heartbeat_timestamp_ms > 10000
```

---

### 3. Election Metrics

Monitor leadership stability and election performance.

#### `chronik_raft_election_count`
- **Type**: Counter
- **Labels**: `topic`, `partition`, `result` (success/failure)
- **Description**: Total number of Raft leader elections
- **Usage**: Track leadership stability
- **Alert If**:
  - > 5 elections in 1 hour (unstable cluster)
  - Frequent failed elections (network issues)

```promql
# Elections in last hour
sum(increase(chronik_raft_election_count[1h]))

# Failed elections
sum(rate(chronik_raft_election_count{result="failure"}[5m]))

# Alert: Too many elections
sum(increase(chronik_raft_election_count[1h])) > 5
```

#### `chronik_raft_election_latency_ms`
- **Type**: Histogram
- **Labels**: `topic`, `partition`
- **Buckets**: 10, 25, 50, 100, 250, 500, 1000, 2000, 5000, 10000ms
- **Description**: Time taken for Raft leader election
- **Usage**: Measure cluster recovery speed
- **Alert If**: p99 > 2000ms (slow elections)

```promql
# p99 election latency
histogram_quantile(0.99, sum(rate(chronik_raft_election_latency_ms_bucket[5m])) by (le))

# p50 election latency
histogram_quantile(0.50, sum(rate(chronik_raft_election_latency_ms_bucket[5m])) by (le))

# Alert: Slow elections
histogram_quantile(0.99, sum(rate(chronik_raft_election_latency_ms_bucket[5m])) by (le)) > 2000
```

#### `chronik_raft_last_election_timestamp_ms`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Timestamp of last Raft election (Unix epoch ms)
- **Usage**: Detect when last leadership change occurred

```promql
# Time since last election (in hours)
((time() * 1000) - chronik_raft_last_election_timestamp_ms) / 1000 / 3600
```

#### `chronik_raft_votes_received`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Number of votes received in current/last election
- **Usage**: Debug election outcomes

---

### 4. Commit Latency Metrics

Measure write performance and quorum commit speed.

#### `chronik_raft_commit_latency_ms`
- **Type**: Histogram
- **Labels**: `topic`, `partition`
- **Buckets**: 1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000ms
- **Description**: Time from log entry proposal to commit (end-to-end write latency)
- **Usage**: **PRIMARY PERFORMANCE METRIC** for write path
- **Alert If**:
  - p99 > 100ms (high latency)
  - p99 > 500ms (critical latency)

```promql
# p99 commit latency (SLO metric)
histogram_quantile(0.99, sum(rate(chronik_raft_commit_latency_ms_bucket[5m])) by (le, topic))

# p50 commit latency
histogram_quantile(0.50, sum(rate(chronik_raft_commit_latency_ms_bucket[5m])) by (le, topic))

# Alert: High write latency
histogram_quantile(0.99, sum(rate(chronik_raft_commit_latency_ms_bucket[5m])) by (le)) > 100
```

#### `chronik_raft_commits_total`
- **Type**: Counter
- **Labels**: `topic`, `partition`
- **Description**: Total number of committed Raft log entries
- **Usage**: Track write throughput

```promql
# Commit rate (writes/sec)
rate(chronik_raft_commits_total[5m])

# Total commits per topic
sum(increase(chronik_raft_commits_total[1h])) by (topic)
```

#### `chronik_raft_batch_commits_total`
- **Type**: Counter
- **Labels**: `topic`, `partition`
- **Description**: Total number of batched commit operations
- **Usage**: Measure batch efficiency

```promql
# Average batch size
rate(chronik_raft_commits_total[5m]) / rate(chronik_raft_batch_commits_total[5m])
```

#### `chronik_raft_commit_batch_size`
- **Type**: Histogram
- **Labels**: `topic`, `partition`
- **Buckets**: 1, 5, 10, 25, 50, 100, 250, 500, 1000
- **Description**: Number of log entries committed in a single batch
- **Usage**: Optimize batching configuration

```promql
# p99 batch size
histogram_quantile(0.99, sum(rate(chronik_raft_commit_batch_size_bucket[5m])) by (le))
```

---

### 5. Snapshot Metrics

Monitor Raft log compaction and snapshot transfers.

#### `chronik_raft_snapshot_count`
- **Type**: Counter
- **Labels**: `topic`, `partition`, `trigger` (size/time/manual)
- **Description**: Total number of Raft snapshots created
- **Usage**: Track snapshot frequency

```promql
# Snapshots per hour
sum(increase(chronik_raft_snapshot_count[1h])) by (topic, trigger)
```

#### `chronik_raft_snapshot_size_bytes`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Size of the latest Raft snapshot in bytes
- **Usage**: Monitor state machine size growth
- **Alert If**: Size > 1GB (may need tuning)

```promql
# Snapshot size in MB
chronik_raft_snapshot_size_bytes / 1024 / 1024

# Alert: Large snapshots
chronik_raft_snapshot_size_bytes > 1073741824
```

#### `chronik_raft_snapshot_create_latency_ms` / `chronik_raft_snapshot_apply_latency_ms`
- **Type**: Histogram
- **Labels**: `topic`, `partition`
- **Buckets**: 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000ms
- **Description**: Time to create/apply a Raft snapshot
- **Usage**: Detect snapshot performance issues
- **Alert If**: p99 > 10000ms (10 seconds)

```promql
# p99 snapshot creation time
histogram_quantile(0.99, sum(rate(chronik_raft_snapshot_create_latency_ms_bucket[5m])) by (le))

# p99 snapshot apply time
histogram_quantile(0.99, sum(rate(chronik_raft_snapshot_apply_latency_ms_bucket[5m])) by (le))

# Alert: Slow snapshot creation
histogram_quantile(0.99, sum(rate(chronik_raft_snapshot_create_latency_ms_bucket[5m])) by (le)) > 10000
```

#### `chronik_raft_snapshot_last_index`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Index of last log entry included in the latest snapshot
- **Usage**: Track log compaction progress

#### `chronik_raft_snapshot_transfers_total`
- **Type**: Counter
- **Labels**: `topic`, `partition`, `direction` (send/receive), `result` (success/failure)
- **Description**: Total number of snapshot transfers
- **Usage**: Monitor snapshot network operations

```promql
# Failed snapshot transfers
rate(chronik_raft_snapshot_transfers_total{result="failure"}[5m])
```

---

### 6. Network & RPC Metrics

Monitor Raft network communication and RPC performance.

#### `chronik_raft_rpc_latency_ms`
- **Type**: Histogram
- **Labels**: `rpc_type` (AppendEntries, RequestVote, InstallSnapshot), `target_node`
- **Buckets**: 1, 2, 5, 10, 25, 50, 100, 250, 500, 1000ms
- **Description**: Raft RPC call latency
- **Usage**: Detect network issues between nodes
- **Alert If**: p99 > 100ms (network degradation)

```promql
# p99 RPC latency by type
histogram_quantile(0.99, sum(rate(chronik_raft_rpc_latency_ms_bucket[5m])) by (le, rpc_type))

# p99 latency to specific node
histogram_quantile(0.99, sum(rate(chronik_raft_rpc_latency_ms_bucket{target_node="node2"}[5m])) by (le))

# Alert: High RPC latency
histogram_quantile(0.99, sum(rate(chronik_raft_rpc_latency_ms_bucket[5m])) by (le)) > 100
```

#### `chronik_raft_rpc_calls_total`
- **Type**: Counter
- **Labels**: `rpc_type`, `target_node`, `result` (success/failure)
- **Description**: Total number of Raft RPC calls
- **Usage**: Track RPC volume and success rate

```promql
# RPC call rate
rate(chronik_raft_rpc_calls_total[5m])

# RPC success rate
sum(rate(chronik_raft_rpc_calls_total{result="success"}[5m])) / sum(rate(chronik_raft_rpc_calls_total[5m]))
```

#### `chronik_raft_rpc_errors_total`
- **Type**: Counter
- **Labels**: `rpc_type`, `target_node`, `error_type` (timeout, connection_refused, etc.)
- **Description**: Total number of Raft RPC errors
- **Usage**: Diagnose network or node failures
- **Alert If**: Error rate > 1% of total calls

```promql
# RPC error rate
rate(chronik_raft_rpc_errors_total[5m])

# Errors by type
sum(rate(chronik_raft_rpc_errors_total[5m])) by (error_type)

# Alert: High RPC error rate
sum(rate(chronik_raft_rpc_errors_total[5m])) / sum(rate(chronik_raft_rpc_calls_total[5m])) > 0.01
```

#### `chronik_raft_append_entries_sent` / `chronik_raft_append_entries_received`
- **Type**: Counter
- **Labels**: `topic`, `partition`, `target_node` / `from_node`
- **Description**: Total number of AppendEntries RPCs sent/received
- **Usage**: Track replication traffic

```promql
# AppendEntries rate per partition
rate(chronik_raft_append_entries_sent[5m])

# Bidirectional traffic (sent + received)
sum(rate(chronik_raft_append_entries_sent[5m])) + sum(rate(chronik_raft_append_entries_received[5m]))
```

#### `chronik_raft_append_entries_size_bytes`
- **Type**: Histogram
- **Labels**: `topic`, `partition`
- **Buckets**: 100, 1K, 10K, 100K, 1M, 10M bytes
- **Description**: Size of AppendEntries RPC payload
- **Usage**: Monitor replication bandwidth

```promql
# p99 AppendEntries size
histogram_quantile(0.99, sum(rate(chronik_raft_append_entries_size_bytes_bucket[5m])) by (le))

# Average payload size
sum(rate(chronik_raft_append_entries_size_bytes_sum[5m])) / sum(rate(chronik_raft_append_entries_size_bytes_count[5m]))
```

#### `chronik_raft_append_entries_rejected_total`
- **Type**: Counter
- **Labels**: `topic`, `partition`, `reason` (stale_term, log_inconsistency)
- **Description**: Total number of rejected AppendEntries RPCs
- **Usage**: Detect log inconsistencies or stale leaders
- **Alert If**: Rejection rate > 1%

```promql
# AppendEntries rejection rate
rate(chronik_raft_append_entries_rejected_total[5m])

# Rejections by reason
sum(rate(chronik_raft_append_entries_rejected_total[5m])) by (reason)
```

---

### 7. Log Metrics

Monitor Raft log size and compaction.

#### `chronik_raft_log_size_entries` / `chronik_raft_log_size_bytes`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Number of entries / bytes in the Raft log
- **Usage**: Monitor log growth and compaction effectiveness
- **Alert If**: Log size > 1M entries or > 10GB (needs snapshot)

```promql
# Log size in entries
chronik_raft_log_size_entries

# Log size in MB
chronik_raft_log_size_bytes / 1024 / 1024

# Alert: Log too large
chronik_raft_log_size_entries > 1000000
```

#### `chronik_raft_log_compactions_total`
- **Type**: Counter
- **Labels**: `topic`, `partition`
- **Description**: Total number of Raft log compactions
- **Usage**: Track compaction frequency

```promql
# Compactions per hour
sum(increase(chronik_raft_log_compactions_total[1h]))
```

---

### 8. Quorum Metrics

Monitor cluster configuration and quorum requirements.

#### `chronik_raft_cluster_size`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Number of nodes in the Raft cluster configuration
- **Usage**: Verify cluster membership

```promql
# Current cluster size
chronik_raft_cluster_size
```

#### `chronik_raft_quorum_size`
- **Type**: Gauge
- **Labels**: `topic`, `partition`
- **Description**: Required quorum size for this partition (majority)
- **Usage**: Understand availability requirements

```promql
# Quorum size (should be (cluster_size / 2) + 1)
chronik_raft_quorum_size
```

#### `chronik_raft_config_changes_total`
- **Type**: Counter
- **Labels**: `topic`, `partition`, `change_type` (add_node, remove_node)
- **Description**: Total number of Raft configuration changes
- **Usage**: Track cluster topology changes

```promql
# Config changes in last hour
sum(increase(chronik_raft_config_changes_total[1h])) by (change_type)
```

---

## Recommended Alerting Rules

### Critical Alerts (Page Immediately)

```yaml
# No leader for partition (data unavailable)
- alert: RaftPartitionNoLeader
  expr: sum(chronik_raft_node_state{state="1"}) by (topic, partition) == 0
  for: 30s
  severity: critical
  annotations:
    summary: "Partition {{ $labels.topic }}-{{ $labels.partition }} has no leader"

# ISR size below quorum (data loss risk)
- alert: RaftISRBelowQuorum
  expr: chronik_raft_isr_size < 2
  for: 1m
  severity: critical
  annotations:
    summary: "ISR size {{ $value }} below quorum for {{ $labels.topic }}-{{ $labels.partition }}"

# High replication lag (data inconsistency)
- alert: RaftHighReplicationLag
  expr: chronik_raft_follower_lag_entries > 1000
  for: 5m
  severity: critical
  annotations:
    summary: "Follower {{ $labels.follower_id }} lagging {{ $value }} entries"

# High commit latency (performance degradation)
- alert: RaftHighCommitLatency
  expr: histogram_quantile(0.99, sum(rate(chronik_raft_commit_latency_ms_bucket[5m])) by (le)) > 500
  for: 5m
  severity: critical
  annotations:
    summary: "p99 commit latency {{ $value }}ms > 500ms"
```

### Warning Alerts (Investigate Soon)

```yaml
# Frequent elections (cluster instability)
- alert: RaftFrequentElections
  expr: sum(increase(chronik_raft_election_count[1h])) > 5
  for: 10m
  severity: warning
  annotations:
    summary: "{{ $value }} elections in last hour (cluster unstable)"

# Moderate replication lag
- alert: RaftModerateReplicationLag
  expr: chronik_raft_follower_lag_entries > 100
  for: 5m
  severity: warning
  annotations:
    summary: "Follower {{ $labels.follower_id }} lagging {{ $value }} entries"

# High RPC error rate
- alert: RaftHighRPCErrorRate
  expr: sum(rate(chronik_raft_rpc_errors_total[5m])) / sum(rate(chronik_raft_rpc_calls_total[5m])) > 0.01
  for: 5m
  severity: warning
  annotations:
    summary: "RPC error rate {{ $value | humanizePercentage }} > 1%"

# Large snapshots (performance impact)
- alert: RaftLargeSnapshots
  expr: chronik_raft_snapshot_size_bytes > 1073741824
  for: 10m
  severity: warning
  annotations:
    summary: "Snapshot size {{ $value | humanize1024 }}B > 1GB"
```

---

## Grafana Dashboard

Import the pre-built Grafana dashboard from:
```
docs/grafana/chronik_raft_dashboard.json
```

**Dashboard Features:**
- Cluster health overview (leader/follower distribution, ISR size)
- Replication lag visualization (entries and milliseconds)
- Election history and latency (p50, p99)
- Commit performance (p50, p95, p99 latencies)
- Snapshot metrics (size, creation/apply latency)
- Network & RPC monitoring (latency by type, error rates)

**Dashboard Variables:**
- `$node` - Filter by node ID
- `$topic` - Filter by topic
- `$DS_PROMETHEUS` - Prometheus datasource

---

## Integration Example

```rust
use chronik_monitoring::RaftMetrics;

let metrics = RaftMetrics::new();

// Record cluster health
metrics.set_leader_count("node1", 3);
metrics.set_isr_size("orders", 0, 3);

// Record replication lag
metrics.set_follower_lag_entries("orders", 0, "node2", 15);
metrics.set_follower_lag_ms("orders", 0, "node2", 45);

// Record election
metrics.record_election("orders", 0, true, 1200.0);

// Record commit latency
metrics.record_commit_latency("orders", 0, 8.0, 10);

// Record snapshot creation
metrics.record_snapshot_create("orders", 0, "size", 67108864, 2500.0, 1000);

// Record RPC call
metrics.record_rpc_call("AppendEntries", "node2", true, 12.0);
```

---

## Troubleshooting Guide

### Issue: High Replication Lag

**Symptoms:**
- `chronik_raft_follower_lag_entries` > 100
- `chronik_raft_follower_lag_ms` > 500

**Possible Causes:**
1. Network latency between leader and follower
2. Follower overloaded (CPU, disk I/O)
3. Large AppendEntries payloads

**Investigation:**
```promql
# Check RPC latency to lagging follower
histogram_quantile(0.99, sum(rate(chronik_raft_rpc_latency_ms_bucket{target_node="node2"}[5m])) by (le))

# Check AppendEntries size
histogram_quantile(0.99, sum(rate(chronik_raft_append_entries_size_bytes_bucket[5m])) by (le))

# Check commit rate (high throughput can cause lag)
rate(chronik_raft_commits_total[5m])
```

**Solutions:**
- Increase network bandwidth between nodes
- Scale follower resources (CPU, disk)
- Tune batch size to reduce payload

---

### Issue: Frequent Elections

**Symptoms:**
- `chronik_raft_election_count` increasing rapidly
- `chronik_raft_current_term` jumping frequently

**Possible Causes:**
1. Network partitions / instability
2. Heartbeat timeout too low
3. Leader overloaded (missing heartbeat deadlines)

**Investigation:**
```promql
# Check election rate
sum(rate(chronik_raft_election_count[5m]))

# Check heartbeat freshness
(time() * 1000) - chronik_raft_last_heartbeat_timestamp_ms

# Check RPC errors
sum(rate(chronik_raft_rpc_errors_total{rpc_type="AppendEntries"}[5m])) by (error_type)
```

**Solutions:**
- Increase heartbeat interval / election timeout
- Fix network issues between nodes
- Scale leader resources

---

### Issue: High Commit Latency

**Symptoms:**
- `chronik_raft_commit_latency_ms` p99 > 100ms
- Write requests slow

**Possible Causes:**
1. Slow quorum acknowledgment (network or disk I/O)
2. Large batch sizes
3. Followers falling behind

**Investigation:**
```promql
# Check commit latency by partition
histogram_quantile(0.99, sum(rate(chronik_raft_commit_latency_ms_bucket[5m])) by (le, partition))

# Check follower lag (lagging followers delay commits)
max(chronik_raft_follower_lag_entries) by (partition)

# Check RPC latency (slow RPCs delay commits)
histogram_quantile(0.99, sum(rate(chronik_raft_rpc_latency_ms_bucket{rpc_type="AppendEntries"}[5m])) by (le))
```

**Solutions:**
- Reduce replication lag (see above)
- Optimize disk I/O on followers (fsync, IOPS)
- Tune batch commit settings

---

## Performance Benchmarks

Expected metrics under normal operation (3-node cluster, 10K msg/s):

| Metric | Expected Value | Alert Threshold |
|--------|---------------|-----------------|
| Commit Latency (p99) | 10-50ms | > 100ms |
| Replication Lag (entries) | 0-10 | > 100 |
| Replication Lag (ms) | 0-50ms | > 500ms |
| ISR Size | 3 | < 2 |
| Elections / hour | 0 | > 5 |
| RPC Latency (p99) | 5-20ms | > 100ms |
| RPC Error Rate | < 0.01% | > 1% |

---

## Additional Resources

- **Prometheus Best Practices**: https://prometheus.io/docs/practices/naming/
- **Grafana Alerting**: https://grafana.com/docs/grafana/latest/alerting/
- **Raft Paper**: https://raft.github.io/raft.pdf
- **Chronik Raft Implementation**: `crates/chronik-raft/`

---

## Conclusion

Comprehensive Raft metrics enable:
- **Proactive monitoring**: Catch issues before they impact users
- **Performance optimization**: Identify bottlenecks and tune configuration
- **Capacity planning**: Understand resource usage and scaling needs
- **Incident response**: Diagnose failures quickly with detailed metrics

**Key Metrics to Watch:**
1. `chronik_raft_commit_latency_ms` (write performance)
2. `chronik_raft_follower_lag_entries` (replication health)
3. `chronik_raft_isr_size` (durability)
4. `chronik_raft_election_count` (stability)
5. `chronik_raft_rpc_errors_total` (network health)

For production deployments, set up all critical alerts and review the Grafana dashboard regularly.
