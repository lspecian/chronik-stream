# Phase 4 & 5 Completion - Quick Start Guide

**Created**: 2025-10-19
**Purpose**: Get you from 60-70% complete to v2.0.0 GA in 3 weeks

---

## TL;DR - What You Need to Do

1. **Run Phase 4 E2E test** (today) - Verify production features work
2. **Fix bugs** discovered (1-2 days) - ISR, shutdown, snapshots, metrics
3. **Integrate ReadIndex** into FetchHandler (1 day) - Enable follower reads
4. **Run Phase 5 E2E test** (1 day) - Verify advanced features
5. **Write docs** (2-3 days) - Deployment guide, config reference, troubleshooting
6. **Release v2.0.0-rc.1** (1 week from now)
7. **Beta test & polish** (2 weeks) → **v2.0.0 GA**

---

## Step 1: Run Phase 4 E2E Test (RIGHT NOW)

### Prerequisites
```bash
# 1. Build chronik-server
cd /Users/lspecian/Development/chronik-stream
cargo build --release --bin chronik-server

# 2. Install Python dependencies
pip3 install kafka-python requests

# 3. Clean up any old data
rm -rf /tmp/chronik-raft-node*
pkill -9 chronik-server
```

### Run the Test
```bash
cd /Users/lspecian/Development/chronik-stream/.conductor/lahore
./test_raft_phase4_production_verified.py
```

### What to Expect

**If Tests PASS** (unlikely but possible):
```
✅ ALL TESTS PASSED (4/4)
🎉 Phase 4 Production Features: VERIFIED!
```
→ Skip to Step 3 (Integrate ReadIndex)

**If Tests FAIL** (expected - first run):
```
⚠️  SOME TESTS FAILED (2/4 passed)
⚠️  Phase 4 needs fixes before production ready
```
→ Go to Step 2 (Fix Bugs)

### Specific Test Failures

**Test 1: ISR Tracking**
- **Failure**: ISR size doesn't change when follower lags
- **Likely Cause**: ISR tracking not enabled or thresholds wrong
- **Fix Location**: `crates/chronik-raft/src/isr_tracker.rs`
- **Check**: Is ISR tracking integrated into replica tick loop?

**Test 2: Graceful Shutdown**
- **Failure**: Leadership not transferred before shutdown
- **Likely Cause**: SIGTERM handler not implemented or broken
- **Fix Location**: `crates/chronik-server/src/main.rs` (signal handler)
- **Check**: Does `shutdown()` trigger leadership transfer?

**Test 3: Snapshot Bootstrap**
- **Failure**: 4th node can't join or read data
- **Likely Cause**: InstallSnapshot RPC broken or snapshot not created
- **Fix Location**: `crates/chronik-raft/src/snapshot.rs`, `src/rpc.rs`
- **Check**: Are snapshots created? Is InstallSnapshot RPC working?

**Test 4: Metrics Exposure**
- **Failure**: `/metrics` endpoint returns 404 or missing Raft metrics
- **Likely Cause**: Metrics endpoint not exposed or metrics not registered
- **Fix Location**: `crates/chronik-monitoring/src/raft_metrics.rs`, `crates/chronik-server/src/main.rs`
- **Check**: Is `/metrics` endpoint exposed? Are Raft metrics registered?

### Debug Commands
```bash
# Check logs from each node
tail -f /tmp/raft-node1.log
tail -f /tmp/raft-node2.log
tail -f /tmp/raft-node3.log

# Check metrics endpoint
curl http://localhost:8091/metrics | grep chronik_raft
curl http://localhost:8092/metrics | grep chronik_raft

# Check processes
ps aux | grep chronik-server

# Clean up and retry
pkill -9 chronik-server
rm -rf /tmp/chronik-raft-node*
./test_raft_phase4_production_verified.py
```

---

## Step 2: Fix Phase 4 Bugs (Day 2)

### Common Fixes

#### Fix 1: ISR Tracking Not Working
**Symptom**: ISR size stays at 3 even when follower killed

**Root Cause**: ISR tracker not integrated into Raft tick loop

**Fix**:
```rust
// File: crates/chronik-raft/src/replica.rs

impl PartitionReplica {
    pub fn tick(&mut self) {
        self.raft_node.tick();

        // ADD THIS: Update ISR based on follower lag
        if let Some(isr_tracker) = &mut self.isr_tracker {
            let followers = self.raft_node.raft.prs();  // Get follower progress
            for (follower_id, progress) in followers {
                let lag = self.raft_node.raft.raft_log.committed - progress.matched;
                if lag > self.config.isr_lag_threshold {
                    isr_tracker.remove_from_isr(*follower_id);
                } else if lag < self.config.isr_catchup_threshold {
                    isr_tracker.add_to_isr(*follower_id);
                }
            }
        }
    }
}
```

**Test**: Re-run Test 1, verify ISR shrinks when follower killed

---

#### Fix 2: Graceful Shutdown Not Transferring Leadership
**Symptom**: SIGTERM kills leader immediately without transferring leadership

**Root Cause**: Signal handler not implemented or not triggering leadership transfer

**Fix**:
```rust
// File: crates/chronik-server/src/main.rs

use tokio::signal;

#[tokio::main]
async fn main() -> Result<()> {
    // ... existing setup ...

    // ADD THIS: Handle SIGTERM gracefully
    let shutdown_signal = async {
        signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        println!("🛑 Received SIGTERM, initiating graceful shutdown...");
    };

    tokio::select! {
        _ = server.run() => {
            println!("✅ Server exited normally");
        }
        _ = shutdown_signal => {
            println!("🔄 Transferring leadership...");
            // Trigger leadership transfer for all partitions
            for replica in server.replicas.values() {
                replica.transfer_leadership().await?;
            }
            println!("✅ Leadership transferred, shutting down...");
        }
    }

    Ok(())
}
```

**Test**: Re-run Test 2, verify leadership transferred before exit

---

#### Fix 3: Snapshot Bootstrap Not Working
**Symptom**: 4th node fails to join or can't read data after joining

**Root Cause 1**: Snapshots not being created

**Fix**:
```rust
// File: crates/chronik-raft/src/replica.rs

impl PartitionReplica {
    pub fn check_snapshot_threshold(&mut self) -> Result<()> {
        let log_size = self.raft_node.raft.raft_log.persisted_entries_size();

        // ADD THIS: Create snapshot if log exceeds threshold
        if log_size > self.config.snapshot_threshold_bytes {
            println!("📸 Creating snapshot (log size: {} bytes)", log_size);
            let snapshot = self.create_snapshot()?;
            self.raft_node.raft.restore(snapshot)?;
            println!("✅ Snapshot created, log compacted");
        }

        Ok(())
    }
}
```

**Root Cause 2**: InstallSnapshot RPC not implemented

**Fix**:
```rust
// File: crates/chronik-raft/src/rpc.rs

impl RaftService for RaftServiceImpl {
    async fn install_snapshot(
        &self,
        request: Request<InstallSnapshotRequest>,
    ) -> Result<Response<InstallSnapshotResponse>, Status> {
        let req = request.into_inner();

        // ADD THIS: Handle snapshot installation
        let replica = self.get_replica(&req.topic, req.partition)?;
        let snapshot_data = req.snapshot_data;

        // Install snapshot
        replica.install_snapshot(snapshot_data).await?;

        Ok(Response::new(InstallSnapshotResponse { success: true }))
    }
}
```

**Test**: Re-run Test 3, verify 4th node downloads snapshot and catches up

---

#### Fix 4: Metrics Not Exposed
**Symptom**: `/metrics` returns 404 or Raft metrics missing

**Root Cause**: Metrics endpoint not exposed or metrics not registered

**Fix**:
```rust
// File: crates/chronik-server/src/main.rs

use chronik_monitoring::raft_metrics::RaftMetricsCollector;

#[tokio::main]
async fn main() -> Result<()> {
    // ... existing setup ...

    // ADD THIS: Register Raft metrics
    let raft_metrics = RaftMetricsCollector::new();
    prometheus::default_registry().register(Box::new(raft_metrics.clone()))?;

    // ADD THIS: Expose /metrics endpoint
    let metrics_addr = format!("0.0.0.0:{}", config.metrics_port);
    tokio::spawn(async move {
        let app = axum::Router::new()
            .route("/metrics", axum::routing::get(metrics_handler));
        axum::Server::bind(&metrics_addr.parse().unwrap())
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    // ... rest of main ...
}

async fn metrics_handler() -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
```

**Test**: Re-run Test 4, verify metrics exposed and accurate

---

## Step 3: Integrate ReadIndex into FetchHandler (Day 3)

### Current State
- ReadIndex implementation complete: `crates/chronik-raft/src/read_index.rs`
- 707 lines of code, 10 unit tests, all passing
- **NOT integrated** into FetchHandler

### What to Do

**File to Edit**: `crates/chronik-server/src/fetch_handler.rs`

**Changes**:

1. **Add ReadIndexManager field**:
```rust
pub struct FetchHandler {
    // ... existing fields
    read_index_manager: Option<Arc<ReadIndexManager>>,
    follower_read_policy: FollowerReadPolicy,
}

#[derive(Clone, Copy)]
pub enum FollowerReadPolicy {
    None,      // Always forward to leader
    Unsafe,    // Serve stale reads from follower
    Safe,      // Use ReadIndex for linearizable reads
}
```

2. **Add constructor method**:
```rust
impl FetchHandler {
    pub fn with_read_index_manager(mut self, manager: Arc<ReadIndexManager>) -> Self {
        self.read_index_manager = Some(manager);
        self.follower_read_policy = FollowerReadPolicy::Safe;
        self
    }
}
```

3. **Use ReadIndex for follower reads**:
```rust
impl FetchHandler {
    async fn handle_fetch(&self, request: FetchRequest) -> Result<FetchResponse> {
        let is_leader = self.is_leader_for_partition(&request.topic, request.partition)?;

        if is_leader {
            // Leader: serve immediately (fast path)
            self.serve_from_local_storage(request).await
        } else {
            // Follower: use read policy
            match self.follower_read_policy {
                FollowerReadPolicy::None => {
                    // Forward to leader
                    self.forward_to_leader(request).await
                }
                FollowerReadPolicy::Unsafe => {
                    // Serve stale reads (fast but unsafe)
                    self.serve_from_local_storage(request).await
                }
                FollowerReadPolicy::Safe => {
                    // Use ReadIndex for linearizability
                    self.serve_with_read_index(request).await
                }
            }
        }
    }

    async fn serve_with_read_index(&self, request: FetchRequest) -> Result<FetchResponse> {
        let manager = self.read_index_manager.as_ref()
            .ok_or_else(|| Error::msg("ReadIndex not configured"))?;

        // Request read index from leader
        let read_index = manager.request_read_index().await?;

        // Wait until safe to read (local commit >= read_index)
        manager.wait_for_safe_read(read_index).await?;

        // Now safe to serve from local storage
        self.serve_from_local_storage(request).await
    }
}
```

4. **Add configuration support**:
```rust
// File: crates/chronik-server/src/main.rs

let follower_read_policy = match env::var("CHRONIK_FOLLOWER_READ_POLICY") {
    Ok(policy) => match policy.as_str() {
        "none" => FollowerReadPolicy::None,
        "unsafe" => FollowerReadPolicy::Unsafe,
        "safe" => FollowerReadPolicy::Safe,
        _ => FollowerReadPolicy::Safe,  // Default
    },
    Err(_) => FollowerReadPolicy::Safe,
};

let fetch_handler = FetchHandler::new(...)
    .with_read_index_manager(read_index_manager)
    .with_follower_read_policy(follower_read_policy);
```

### Test Integration
```bash
# Test follower reads with ReadIndex
CHRONIK_FOLLOWER_READ_POLICY=safe ./test_raft_cluster_basic.py

# Test unsafe follower reads (stale)
CHRONIK_FOLLOWER_READ_POLICY=unsafe ./test_raft_cluster_basic.py

# Test forwarding to leader
CHRONIK_FOLLOWER_READ_POLICY=none ./test_raft_cluster_basic.py
```

---

## Step 4: Run Phase 5 E2E Test (Day 4)

```bash
cd /Users/lspecian/Development/chronik-stream/.conductor/lahore
./test_raft_phase5_advanced_verified.py
```

### Expected Results

**Test 1: DNS Discovery**
- ⚠️  SIMULATED (needs Kubernetes or dnsmasq)
- Should show "DNS discovery test (simulated) complete"
- Real testing requires infrastructure

**Test 2: Dynamic Rebalancing**
- ✅ Should PASS if rebalancing code works
- Verifies partitions redistribute when 4th node added
- Verifies zero downtime during rebalance

**Test 3: Rolling Upgrade**
- ⚠️  SIMULATED (needs two binary versions)
- Should show "Rolling upgrade test (simulated) complete"
- Real testing requires building v2.0 and v2.1 binaries

**Test 4: Multi-DC Replication**
- ⚠️  SIMULATED (needs network namespaces or tc)
- Should show "Multi-DC replication test complete (latency simulation skipped)"
- Real testing requires network latency injection

---

## Step 5: Write Documentation (Day 5-7)

### Priority 1: Deployment Guide
**File**: `docs/RAFT_DEPLOYMENT_GUIDE.md`

**Sections**:
1. Prerequisites (Rust, Kafka tools)
2. Single-node setup (development)
3. 3-node cluster setup (production)
4. Configuration examples (chronik.toml)
5. Environment variables
6. Health checks (/metrics endpoint)
7. Troubleshooting

**Template**:
```markdown
# Chronik Raft Cluster Deployment Guide

## Quick Start (3-Node Cluster)

### Prerequisites
- Rust 1.75+ (for building from source)
- 3 servers with network connectivity
- Kafka client tools (kafka-topics, kafka-console-producer)

### Configuration (chronik.toml)

```toml
[cluster]
enabled = true
node_id = 1  # Change for each node (1, 2, 3)
replication_factor = 3
min_insync_replicas = 2

[raft]
election_timeout_ms = 1000
heartbeat_interval_ms = 100
snapshot_max_log_size_mb = 64
```

### Start Cluster

**Node 1** (bootstrap):
```bash
chronik-server \
  --kafka-port 9092 \
  --node-id 1 \
  raft-cluster \
  --raft-addr 0.0.0.0:5001 \
  --peers 2@node2:5002,3@node3:5003 \
  --bootstrap
```

**Node 2**:
```bash
chronik-server \
  --kafka-port 9092 \
  --node-id 2 \
  raft-cluster \
  --raft-addr 0.0.0.0:5002 \
  --peers 1@node1:5001,3@node3:5003
```

**Node 3**:
```bash
chronik-server \
  --kafka-port 9092 \
  --node-id 3 \
  raft-cluster \
  --raft-addr 0.0.0.0:5003 \
  --peers 1@node1:5001,2@node2:5002
```

### Verify Cluster

```bash
# Check metrics
curl http://node1:8091/metrics | grep chronik_raft_leader_count

# Create topic
kafka-topics --create --topic test --partitions 3 --replication-factor 3 --bootstrap-server node1:9092

# Produce/consume
kafka-console-producer --topic test --bootstrap-server node1:9092
kafka-console-consumer --topic test --from-beginning --bootstrap-server node2:9092
```
```

---

### Priority 2: Configuration Reference
**File**: `docs/RAFT_CONFIGURATION_REFERENCE.md`

Document ALL Raft tunables with defaults and recommendations.

---

### Priority 3: Troubleshooting Guide
**File**: `docs/RAFT_TROUBLESHOOTING.md`

Common issues and solutions.

---

## Timeline Summary

| Day | Task | Deliverable |
|-----|------|-------------|
| **1** | Run Phase 4 E2E test | Test results, issue list |
| **2** | Fix Phase 4 bugs | All 4 tests passing |
| **3** | Integrate ReadIndex | Follower reads working |
| **4** | Run Phase 5 E2E test | Test results |
| **5-7** | Write documentation | Deployment guide, config ref, troubleshooting |
| **8** | Release prep | v2.0.0-rc.1 |
| **9-21** | Beta testing | v2.0.0 GA |

---

## Next Action: START NOW

```bash
cd /Users/lspecian/Development/chronik-stream/.conductor/lahore
./test_raft_phase4_production_verified.py
```

Then come back to this guide based on results:
- **Tests pass**: Skip to Step 3 (Integrate ReadIndex)
- **Tests fail**: Go to Step 2 (Fix Bugs)

Good luck! 🚀
