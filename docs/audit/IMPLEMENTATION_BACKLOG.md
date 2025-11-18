# Comprehensive Implementation Backlog: Production-Grade Chronik

**Created**: 2025-11-18
**Purpose**: Complete roadmap to transform Chronik into a scalable, reliable, high-throughput distributed streaming platform
**Status**: Planning - Ready for Implementation

**Source**: Based on comprehensive 6-session code audit findings and correlation analysis

---

## Executive Summary

This backlog contains **81 actionable tasks** across **5 implementation phases** to transform Chronik from its current state (201 msg/s, 3-hour deadlocks, 30-second topic creation) into a production-grade system capable of:

- **Throughput**: 100,000+ msg/s (500x improvement)
- **Availability**: 99.99% (4 nines)
- **Latency**: p99 < 10ms for produce (100x improvement)
- **Topic creation**: < 500ms (60x improvement)
- **Zero deadlocks**: Complete timeout coverage + lock-free hot paths

**Timeline**: 5 phases over 12-16 weeks

**Updates**: Added 3 NEW critical tasks based on v2.2.7/v2.2.8 testing:
- P0.4: Fix Cluster Startup Deadlock (cold start issue)
- P0.5: Investigate Topic Auto-Creation Timeout Bug
- P5.5: Comprehensive Regression Testing

---

## Phase 0: Critical Stability Fixes (Week 1-2)

**Goal**: Eliminate production outages (deadlocks, indefinite hangs, cluster startup failures)
**Tasks**: 5 (P0.1-P0.5) - **P0.4 MUST BE DONE FIRST!**
**Success Metrics**:
- Cluster starts successfully on cold start (all listeners running)
- No deadlocks in 7-day stress test
- All I/O operations have timeouts
- Raft consensus latency p99 < 50ms
- Topic creation timeout root cause identified

**Recommended Order**:
1. **P0.4** - Fix startup deadlock (4h) → **BLOCKS ALL TESTING**
2. **P0.5** - Investigate topic timeout (2h) → **UNDERSTANDING**
3. **P0.1** - Add I/O timeouts (8h) → **PREVENT HANGS**
4. **P0.2** - Move I/O outside lock (12h) → **ELIMINATE DEADLOCKS**
5. **P0.3** - Verify completeness (4h) → **REGRESSION PREVENTION**

### P0.1: Add Timeouts to All I/O Operations

**Priority**: P0 - IMMEDIATE
**Effort**: 8 hours
**Risk**: Low
**Impact**: Prevents all indefinite hang scenarios

**Tasks**:

1. **Create timeout wrapper utility** (1 hour)
   ```rust
   // File: crates/chronik-common/src/timeout.rs
   pub async fn with_io_timeout<F, T>(
       duration: Duration,
       future: F,
       context: &str,
   ) -> Result<T>
   where
       F: Future<Output = Result<T>>,
   {
       match tokio::time::timeout(duration, future).await {
           Ok(Ok(result)) => Ok(result),
           Ok(Err(e)) => Err(e),
           Err(_) => {
               error!("I/O timeout after {:?}: {}", duration, context);
               Err(anyhow!("Timeout: {}", context))
           }
       }
   }
   ```

2. **Add timeout to WAL file.sync_all()** (2 hours)
   - File: `crates/chronik-wal/src/group_commit.rs`
   - Lines: 82-107 (WalWriter::sync_all), 846-852 (commit_batch)
   - Timeout: 30 seconds
   - Test: Slow disk simulation with cgroups

3. **Add timeout to Raft storage operations** (2 hours)
   - File: `crates/chronik-wal/src/raft_storage_impl.rs`
   - Lines: 393-478 (append_entries), 481-510 (persist_hard_state)
   - Timeout: 30 seconds
   - Test: Verify timeout fires with simulated disk stall

4. **Add timeout to TCP forwarding** (2 hours)
   - File: `crates/chronik-server/src/produce_handler.rs`
   - Lines: 1238-1440 (forward_produce_to_leader)
   - Timeout: 10 seconds (entire forwarding operation)
   - Test: Verify timeout with dead leader

5. **Add metrics for timeout events** (1 hour)
   - Counter: `chronik_io_timeouts_total{operation="wal_fsync|tcp_connect|..."}`
   - Histogram: `chronik_io_duration_seconds{operation="..."}`
   - Alert: Fire PagerDuty if timeout rate > 1% of operations

**Testing Requirements**:
- Unit tests with simulated timeouts
- Integration test with cgroup I/O throttling
- 24-hour stress test with random I/O delays

**Documentation**:
- Add "Timeout Policy" section to architecture docs
- Document timeout values and rationale

**Success Criteria**:
- ✅ All I/O operations have timeout wrappers
- ✅ Metrics show 0 timeouts in normal operation
- ✅ Stress test survives simulated disk stalls

---

### P0.2: Move Raft Storage I/O Outside Lock

**Priority**: P0 - IMMEDIATE
**Effort**: 12 hours
**Risk**: Medium (requires careful Raft protocol compliance)
**Impact**: Eliminates deadlock root cause, 10-50x Raft consensus speedup

**Tasks**:

1. **Analyze Raft protocol requirements** (2 hours)
   - Read raft-rs documentation on Ready/advance() ordering
   - Confirm entries can be persisted after advance()
   - Document safety invariants

2. **Extract persistence data before dropping lock** (4 hours)
   - File: `crates/chronik-server/src/raft_cluster.rs`
   - Lines: 2045-2082 (current buggy version)
   - Extract: entries, hard_state, conf_state
   - Ensure: advance() called BEFORE dropping lock

3. **Persist outside lock** (2 hours)
   ```rust
   // Extract data while holding lock
   let (entries_to_persist, hs_to_persist) = {
       let mut raft_lock = self.raft_node.lock().await;

       let entries = if !ready.entries().is_empty() {
           Some(ready.entries().iter().cloned().collect::<Vec<_>>())
       } else {
           None
       };

       let hs = ready.hs().cloned();

       // CRITICAL: advance() MUST happen before dropping lock
       raft_lock.advance(ready);

       (entries, hs)
       // Lock dropped here
   };

   // Persist outside lock (with timeout!)
   if let Some(entries) = entries_to_persist {
       with_io_timeout(
           Duration::from_secs(30),
           self.storage.append_entries(&entries),
           "raft_append_entries"
       ).await?;
   }

   if let Some(hs) = hs_to_persist {
       with_io_timeout(
           Duration::from_secs(30),
           self.storage.persist_hard_state(&hs),
           "raft_persist_hardstate"
       ).await?;
   }
   ```

4. **Add lock hold time instrumentation** (2 hours)
   - Metric: `chronik_raft_lock_hold_seconds{phase="ready|persist|apply"}`
   - Target: p99 < 5ms (only in-memory operations)
   - Alert: Fire if p99 > 50ms

5. **Test Raft consensus correctness** (2 hours)
   - Verify leader election still works
   - Verify log replication works
   - Verify snapshot recovery works
   - Jepsen-style test: random crashes during consensus

**Testing Requirements**:
- Raft correctness tests (leader election, log replication, snapshots)
- Lock hold time metrics validation (p99 < 5ms)
- Crash recovery test (kill nodes during consensus)
- Performance test (Raft latency should decrease 10-50x)

**Documentation**:
- Document lock scope policy: "NO I/O while holding locks"
- Add ADR: "Lock Scope for Async Operations"

**Success Criteria**:
- ✅ Lock hold time p99 < 5ms (was 100-500ms)
- ✅ Raft consensus latency p99 < 50ms (was 500ms+)
- ✅ All Raft tests pass
- ✅ 7-day stability test with no deadlocks

---

### P0.3: Fix v2.2.7 Incomplete Deadlock Fix

**Priority**: P0 - IMMEDIATE
**Effort**: 4 hours
**Risk**: Low (verification of P0.2 fix)
**Impact**: Confirms deadlock vulnerability is fully eliminated

**Tasks**:

1. **Verify apply_committed_entries() is outside lock** (1 hour)
   - Already done in v2.2.7
   - Confirm with code review
   - Add comment explaining why

2. **Verify storage operations are outside lock** (1 hour)
   - Completed by P0.2
   - Add regression test to prevent reintroduction

3. **Add static analysis check** (2 hours)
   - Create clippy lint: `no_io_while_holding_mutex`
   - Detect: `.lock().await` → `.await` → `drop(lock)`
   - Fail CI if pattern detected

**Testing Requirements**:
- Static analysis catches lock-during-I/O pattern
- Regression test prevents reintroduction

**Success Criteria**:
- ✅ Static analysis passes
- ✅ All storage I/O outside locks
- ✅ Documentation updated

---

### P0.4: Fix Cluster Startup Deadlock (Cold Start)

**Priority**: P0 - IMMEDIATE (MUST BE DONE FIRST!)
**Effort**: 4 hours
**Risk**: Low (isolated change in startup sequence)
**Impact**: Eliminates 100% cluster failure on cold start
**Source**: CLUSTER_STARTUP_DEADLOCK_ANALYSIS.md (discovered Nov 16, 2025)

**Problem**:
Follower nodes wait for leader election BEFORE starting TCP listeners (Kafka port 29092, WAL port 29291), but need those listeners running to participate in leader election → **circular dependency deadlock**.

**Impact**:
- 100% cluster failure on cold start
- Only the node that wins initial election gets its listeners started
- Followers deadlock with NO listeners, causing permanent election storms

**Tasks**:

1. **Modify startup sequence in main.rs** (3 hours)
   - File: `crates/chronik-server/src/main.rs:660-825`
   - Current (BROKEN):
     ```
     1. Initialize Raft cluster
     2. Raft.start() → Raft gRPC listener binds ✅
     3. Create IntegratedKafkaServer
     4. WAIT for leader election ⚠️ BLOCKING
     5. Spawn server.run() → Kafka listener binds ❌ TOO LATE
     6. (WAL receiver never started on followers) ❌ MISSING
     ```
   - Fixed (CORRECT):
     ```
     1. Initialize Raft cluster
     2. Raft.start() → Raft gRPC listener binds ✅
     3. Create IntegratedKafkaServer
     4. *** Spawn server.run() IMMEDIATELY → Kafka listener binds ✅
     5. *** Spawn wal_receiver.run() IMMEDIATELY → WAL listener binds ✅
     6. WAIT for leader election ✅ Now safe - all listeners running
     7. Continue with broker registration (leader only)
     ```
   - Code change:
     ```rust
     // Start Kafka protocol listener (0.0.0.0:29092) on ALL nodes
     let kafka_addr_clone = kafka_addr.clone();
     let server_clone = Arc::clone(&server);
     let server_task = tokio::spawn(async move {
         if let Err(e) = server_clone.run(&kafka_addr_clone).await {
             error!("Integrated server task failed: {}", e);
         }
     });
     info!("✓ Kafka protocol listener started on {}", kafka_addr);

     // Start WAL receiver listener (0.0.0.0:29291) on ALL nodes
     if let Some(wal_rcv_addr) = &wal_receiver_addr {
         if !wal_rcv_addr.is_empty() {
             let mut wal_receiver = crate::wal_replication::WalReceiver::new_with_isr_tracker(...);
             let wal_receiver_task = tokio::spawn(async move {
                 if let Err(e) = wal_receiver.run().await {
                     error!("WAL receiver task failed: {}", e);
                 }
             });
             info!("✓ WAL receiver listener started on {}", wal_rcv_addr);
         }
     }

     // Small delay to ensure listeners are bound
     tokio::time::sleep(Duration::from_millis(500)).await;

     // NOW it's safe to wait for leader election
     info!("Waiting for Raft leader election (max 10s)...");
     ```

2. **Test cold start scenario** (1 hour)
   - Stop all 3 nodes
   - Clear Raft WAL: `rm -rf data/wal/__meta/__raft_metadata`
   - Start all 3 simultaneously
   - Verify: `netstat -tln | grep ':29092\|:29291\|:25001'` shows ALL 6 ports listening
   - Verify: Stable leader election (no storm)
   - Verify: Client connectivity to follower nodes

**Testing Requirements**:
- 3-node cold start test (clean Raft WAL)
- Verify all 3 TCP listeners start on all 3 nodes within 10s
- Verify no election storm (only 1 election)
- Verify client can connect to ANY node (not just leader)

**Documentation**:
- Update startup sequence documentation
- Add "Cold Start Troubleshooting" section to runbooks

**Success Criteria**:
- ✅ All 3 TCP ports listening on all 3 nodes (Kafka, WAL, Raft)
- ✅ Stable leader election within 10 seconds
- ✅ NO election storm
- ✅ All nodes accept client connections
- ✅ Cluster functional for produce/consume operations

**CRITICAL**: This task MUST be completed FIRST before any other P0 tasks, as it blocks all cluster testing.

---

### P0.5: Investigate Topic Auto-Creation Timeout Bug

**Priority**: P0 - IMMEDIATE (Investigation)
**Effort**: 2 hours
**Risk**: Low (investigation only, no code changes)
**Impact**: Understand NEW failure mode discovered in v2.2.8
**Source**: CLUSTER_STATUS_REPORT_v2.2.8.md (discovered Nov 16, 2025)

**Problem**:
`create_topic()` operations via Raft consensus time out after 5 seconds, even though Raft cluster is healthy.

**Symptoms**:
- ✅ Existing topics work (can produce/consume)
- ❌ **New topics fail** (5s timeout, metadata not created)
- ✅ Raft is healthy (leader elected, logs persisting)
- ❌ Raft metadata operations timeout specifically

**Evidence**:
```
[2025-11-16T14:35:41] Auto-creating topic 'perf-test-acks1-async'
[2025-11-16T14:35:46] WARN: Topic creation timed out after 5000ms
[2025-11-16T14:35:46] ERROR: Failed to auto-create topic
```

**Difference from Session 4 findings**:
- Session 4: Topic creation **completes** but takes 20-30s (serial partition assignment)
- This bug: Topic creation **fails** entirely after 5s timeout (Raft proposal doesn't complete)

**Tasks**:

1. **Add debug logging to Raft metadata flow** (1 hour)
   - File: `crates/chronik-server/src/raft_metadata_store.rs:128-217`
   - Add timestamps at each step:
     ```rust
     let start = Instant::now();
     debug!("T+0ms: create_topic() called for '{}'", name);

     // Raft propose
     debug!("T+{}ms: Calling raft_cluster.propose_metadata()", start.elapsed().as_millis());
     self.raft_cluster.propose_metadata(cmd.clone()).await?;
     debug!("T+{}ms: Raft propose completed", start.elapsed().as_millis());

     // Wait for notification
     debug!("T+{}ms: Waiting for notification", start.elapsed().as_millis());
     match tokio::time::timeout(timeout_duration, notify.notified()).await {
         Ok(_) => {
             debug!("T+{}ms: Notification received", start.elapsed().as_millis());
         }
         Err(_) => {
             debug!("T+{}ms: TIMEOUT - notification never fired", start.elapsed().as_millis());
         }
     }
     ```
   - Log Raft commit index progression
   - Log whether notification actually fires

2. **Reproduce and capture logs** (1 hour)
   - Start 3-node cluster
   - Attempt topic creation (expect failure)
   - Analyze logs to determine:
     - Does Raft propose succeed? (Should see committed entry)
     - Does Raft commit succeed? (Check commit index)
     - Does apply_committed_entries() get called?
     - Does notification fire?
     - Where exactly does timeout occur?
   - Document findings

**Expected Discoveries**:
- Likely caused by **P0.2** (lock held during I/O) → Raft consensus can't complete
- Or caused by **P1.3** (incomplete event-driven notification) → Notification never fires
- Or NEW issue not in original audit

**Output Deliverable**:
- Document: `docs/TOPIC_CREATION_TIMEOUT_ROOT_CAUSE.md`
- Contains:
  - Exact location of timeout
  - Root cause analysis
  - Which backlog item(s) will fix this
  - Any additional fixes needed (if NEW issue)

**Success Criteria**:
- ✅ Root cause identified with evidence (logs/traces)
- ✅ Confirmed which backlog task(s) fix this issue
- ✅ Document created for future reference

**NOTE**: This is an **investigation task**, not a fix. The actual fix may be covered by P0.2 or P1.3, or may require a NEW task if it's a different issue.

---

## Phase 1: Performance Restoration (Week 3-4)

**Goal**: Restore performance to acceptable levels (10,000+ msg/s)
**Success Metrics**:
- Throughput: 10,000-15,000 msg/s (50-75x improvement)
- Topic creation: < 5 seconds (6x improvement)
- Forwarding latency: < 15ms p99

### P1.1: Parallelize Partition Assignment

**Priority**: P1 - HIGH
**Effort**: 4 hours
**Risk**: Low
**Impact**: 3x faster topic creation (30s → 10s → 3-5s)

**Tasks**:

1. **Replace serial for loop with futures::join_all** (2 hours)
   - File: `crates/chronik-server/src/produce_handler.rs`
   - Lines: 2606-2642
   ```rust
   // Current (serial)
   for (partition_id, partition_info) in topic_assignments {
       raft_cluster.propose_partition_assignment_and_wait(...).await?;
   }

   // Fixed (parallel)
   let futures: Vec<_> = topic_assignments.iter().map(|(partition_id, partition_info)| {
       let topic_name = topic_name.clone();
       let partition_id = *partition_id;
       let replicas = partition_info.replicas.clone();
       async move {
           raft_cluster.propose_partition_assignment_and_wait(
               topic_name,
               partition_id,
               replicas
           ).await
       }
   }).collect();

   futures::future::try_join_all(futures).await?;
   ```

2. **Add timing metrics** (1 hour)
   - Metric: `chronik_topic_creation_seconds{phase="create|assign_partitions|total"}`
   - Track: Serial vs parallel timing

3. **Test with 100 partitions** (1 hour)
   - Before: 100 × 5s = 500s (8 minutes) if all timeout
   - After: max(5s × parallel batch) = 5-10s
   - Verify: All partitions assigned correctly

**Testing Requirements**:
- Unit test with mock Raft (verify parallel execution)
- Integration test with real cluster (100 partitions)
- Timing validation (should be ~3x faster)

**Success Criteria**:
- ✅ Topic creation with 3 partitions: < 5 seconds
- ✅ Topic creation with 100 partitions: < 15 seconds
- ✅ Metrics show parallel execution

---

### P1.2: Add Connection Pooling to Forwarding

**Priority**: P1 - HIGH
**Effort**: 20 hours
**Risk**: Medium (complex state management)
**Impact**: 20% latency reduction, eliminates TCP handshake overhead

**Tasks**:

1. **Design connection pool architecture** (4 hours)
   - Pool per destination node
   - Max connections per node: 10-50 (configurable)
   - Health checking: Periodic ping to detect dead connections
   - Eviction: LRU or idle timeout (30s)
   - Backpressure: Block if pool full + all connections busy

2. **Implement ConnectionPool struct** (6 hours)
   ```rust
   // File: crates/chronik-server/src/connection_pool.rs
   pub struct ConnectionPool {
       pools: Arc<DashMap<String, VecDeque<PooledConnection>>>,
       config: PoolConfig,
       metrics: Arc<PoolMetrics>,
   }

   struct PooledConnection {
       stream: TcpStream,
       acquired_at: Instant,
       last_used: Instant,
       request_count: u64,
   }

   impl ConnectionPool {
       pub async fn acquire(&self, addr: &str) -> Result<PooledConnection> {
           // Try to get existing connection
           if let Some(mut pool) = self.pools.get_mut(addr) {
               while let Some(mut conn) = pool.pop_front() {
                   // Health check: try to peek() or write 0 bytes
                   if self.is_healthy(&conn).await {
                       conn.acquired_at = Instant::now();
                       return Ok(conn);
                   }
                   // Dead connection, discard
               }
           }

           // Create new connection
           let stream = timeout(
               Duration::from_secs(5),
               TcpStream::connect(addr)
           ).await??;

           Ok(PooledConnection {
               stream,
               acquired_at: Instant::now(),
               last_used: Instant::now(),
               request_count: 0,
           })
       }

       pub async fn release(&self, addr: String, mut conn: PooledConnection) {
           conn.last_used = Instant::now();
           conn.request_count += 1;

           let mut pool = self.pools.entry(addr).or_insert_with(VecDeque::new);

           // Don't pool if:
           // - Pool is full
           // - Connection is too old (> 5 minutes)
           // - Connection has served too many requests (> 1000)
           if pool.len() < self.config.max_per_host
               && conn.acquired_at.elapsed() < Duration::from_secs(300)
               && conn.request_count < 1000
           {
               pool.push_back(conn);
           }
           // else: connection dropped, TCP closed
       }

       async fn is_healthy(&self, conn: &PooledConnection) -> bool {
           // Try non-blocking peek to detect closed connection
           // Or send 0-byte write
           conn.stream.writable().await.is_ok()
       }
   }
   ```

3. **Integrate with forwarding** (4 hours)
   - File: `crates/chronik-server/src/produce_handler.rs`
   - Replace: TcpStream::connect() with pool.acquire()
   - Add: pool.release() after use (or on error)

4. **Add background pool cleanup** (2 hours)
   - Tokio task: Runs every 30 seconds
   - Evict: Connections idle > 60 seconds
   - Close: Connections that fail health check

5. **Add metrics** (2 hours)
   - `chronik_connection_pool_size{node="..."}`
   - `chronik_connection_pool_hits_total`
   - `chronik_connection_pool_misses_total`
   - `chronik_connection_pool_evictions_total{reason="idle|unhealthy|overflow"}`

6. **Test connection reuse** (2 hours)
   - Send 10,000 requests
   - Verify: connections_created < 100 (was 6,667)
   - Verify: connections_reused > 9,900

**Testing Requirements**:
- Unit test pool acquire/release logic
- Integration test with real forwarding workload
- Connection leak test (all connections released)
- Failure test (dead connections evicted)

**Success Criteria**:
- ✅ Connection reuse rate > 95%
- ✅ Forwarding latency p99 reduced by 20% (eliminate TCP handshake)
- ✅ No connection leaks in 24-hour test

---

### P1.3: Complete Event-Driven Metadata Migration

**Priority**: P1 - HIGH
**Effort**: 16 hours
**Risk**: Medium (requires careful Raft integration)
**Impact**: 10-50x latency reduction for metadata operations (100ms → 1-10ms)

**Tasks**:

1. **Add notification maps for remaining 7 operations** (2 hours)
   - File: `crates/chronik-server/src/raft_cluster.rs`
   - Add: `pending_offsets: Arc<DashMap<String, Arc<Notify>>>`
   - Add: `pending_watermarks: Arc<DashMap<String, Arc<Notify>>>`
   - Add: `pending_leaders: Arc<DashMap<String, Arc<Notify>>>`
   - Add 4 more for remaining operations

2. **Update apply_committed_entries() to fire all notifications** (4 hours)
   ```rust
   match &cmd {
       MetadataCommand::CreateTopic { name, .. } => {
           if let Some((_, notify)) = self.pending_topics.remove(name) {
               notify.notify_waiters();
           }
       }
       MetadataCommand::UpdatePartitionOffset { topic, partition, .. } => {
           let key = format!("{}:{}", topic, partition);
           if let Some((_, notify)) = self.pending_offsets.remove(&key) {
               notify.notify_waiters();
           }
       }
       MetadataCommand::SetHighWatermark { topic, partition, .. } => {
           let key = format!("{}:{}", topic, partition);
           if let Some((_, notify)) = self.pending_watermarks.remove(&key) {
               notify.notify_waiters();
           }
       }
       // ... complete ALL operations
   }
   ```

3. **Migrate raft_metadata_store.rs polling loops to event-driven** (6 hours)
   - File: `crates/chronik-server/src/raft_metadata_store.rs`
   - Lines: 327, 412, 684, 784, 855, 967, 1064 (7 operations)
   - Replace: 100ms sleep loops with `notify.notified().await`
   - Ensure: Timeout still present (5s) as fallback

4. **Add metrics for notification vs polling** (2 hours)
   - Counter: `chronik_metadata_notifications_total{operation="..."}`
   - Counter: `chronik_metadata_polling_fallbacks_total{operation="..."}`
   - Alert: Fire if polling rate > 1%

5. **Test performance improvement** (2 hours)
   - Before: update_partition_offset on follower = 100-500ms (polling)
   - After: update_partition_offset on follower = 1-10ms (notification)
   - Verify: produce latency decreases proportionally

**Testing Requirements**:
- Unit test each operation's notification path
- Integration test with follower writes (verify notification fires)
- Performance benchmark (latency before/after)
- Stress test (10,000 concurrent metadata ops)

**Success Criteria**:
- ✅ All 9 operations use event-driven notifications
- ✅ Polling fallback rate < 1%
- ✅ Metadata operation latency p99 < 10ms (was 100-500ms)
- ✅ Produce throughput increases to 5,000-10,000 msg/s

---

## Phase 2: Advanced Performance Optimizations (Week 5-8)

**Goal**: Achieve high-performance targets (50,000+ msg/s)
**Success Metrics**:
- Throughput: 50,000-100,000 msg/s
- Latency: p99 < 5ms
- Resource efficiency: 50% reduction in CPU usage

### P2.1: Smart Client Routing (Eliminate Forwarding)

**Priority**: P2 - MEDIUM
**Effort**: 24 hours
**Risk**: High (protocol changes, client compatibility)
**Impact**: Eliminates 67% of forwarding overhead (67% × 15ms = 10ms average savings)

**Tasks**:

1. **Research Kafka partition metadata protocol** (4 hours)
   - Understand how Kafka clients discover partition leaders
   - Review MetadataResponse format
   - Identify required changes to Chronik

2. **Implement partition leader metadata in MetadataResponse** (6 hours)
   - File: `crates/chronik-protocol/src/metadata.rs`
   - Add: leader_id field to PartitionMetadata
   - Add: broker list with host:port mappings
   - Ensure: Kafka clients can parse response

3. **Add partition metadata caching on client side** (6 hours)
   - Clients: kafka-python, confluent-kafka
   - Cache: partition → leader_id → (host, port) mapping
   - Refresh: On NOT_LEADER_FOR_PARTITION error
   - TTL: 5 minutes (configurable)

4. **Add partition leadership tracking** (4 hours)
   - File: `crates/chronik-server/src/raft_metadata_store.rs`
   - Track: Which node is leader for each partition
   - Update: On leader election, partition reassignment
   - Expose: Via MetadataResponse

5. **Measure forwarding reduction** (2 hours)
   - Metric: `chronik_produce_forwarding_rate`
   - Before: 67% (2 of 3 nodes are non-leaders)
   - After: < 5% (only stale client metadata)

6. **Add graceful fallback** (2 hours)
   - If: Client connects to wrong node (stale metadata)
   - Response: NOT_LEADER_FOR_PARTITION + correct leader in response
   - Client: Updates cache and retries to correct leader

**Testing Requirements**:
- Test with kafka-python client (verify metadata refresh)
- Test with confluent-kafka client
- Measure forwarding rate reduction (67% → <5%)
- Measure latency improvement (10ms average savings)

**Success Criteria**:
- ✅ Forwarding rate < 5% (was 67%)
- ✅ Produce latency p99 < 10ms (eliminate forwarding overhead)
- ✅ Throughput: 20,000-50,000 msg/s (10-25x improvement)

---

### P2.2: Batch Forwarding (If Forwarding Still Needed)

**Priority**: P2 - MEDIUM
**Effort**: 16 hours
**Risk**: Medium
**Impact**: 5-10x reduction in forwarding overhead (if forwarding still occurs)

**Tasks**:

1. **Design batching mechanism** (4 hours)
   - Batch multiple produce requests to same leader
   - Max batch size: 100 requests or 1MB
   - Max wait time: 10ms
   - Group by: destination node

2. **Implement batch forwarding** (8 hours)
   ```rust
   struct ForwardBatcher {
       batches: DashMap<NodeId, Vec<PendingForward>>,
       flush_notify: Arc<Notify>,
   }

   impl ForwardBatcher {
       async fn forward(&self, node_id: NodeId, request: ProduceRequest) -> Result<ProduceResponse> {
           let (tx, rx) = oneshot::channel();

           self.batches.entry(node_id).or_insert_with(Vec::new).push(PendingForward {
               request,
               response_tx: tx,
           });

           self.flush_notify.notify_one();

           rx.await?
       }

       async fn flush_batches(&self) {
           loop {
               tokio::select! {
                   _ = self.flush_notify.notified() => {},
                   _ = tokio::time::sleep(Duration::from_millis(10)) => {},
               }

               for entry in self.batches.iter_mut() {
                   let (node_id, batch) = entry.pair();
                   if batch.is_empty() {
                       continue;
                   }

                   // Take batch
                   let requests = std::mem::take(&mut *batch);

                   // Forward as single batched request
                   self.forward_batch(*node_id, requests).await;
               }
           }
       }
   }
   ```

3. **Add metrics** (2 hours)
   - Histogram: `chronik_forward_batch_size`
   - Counter: `chronik_forward_batches_total`
   - Savings: `chronik_forward_tcp_connections_saved_total`

4. **Test batching effectiveness** (2 hours)
   - Send 10,000 requests concurrently
   - Verify: Batches average 50-100 requests
   - Verify: TCP connections reduced 50-100x

**Testing Requirements**:
- Unit test batching logic
- Integration test with high concurrency
- Measure batch size distribution
- Measure TCP connection reduction

**Success Criteria**:
- ✅ Average batch size > 50 requests
- ✅ TCP connections reduced 50-100x
- ✅ Forwarding latency reduced 20-30%

---

### P2.3: Zero-Copy Message Handling

**Priority**: P2 - MEDIUM
**Effort**: 32 hours
**Risk**: High (requires careful memory management)
**Impact**: 30-50% CPU reduction, 2-5x throughput improvement

**Tasks**:

1. **Audit current memory copies** (4 hours)
   - Trace: Client → Protocol → Handler → WAL → Disk
   - Count: Number of copies (likely 4-6)
   - Identify: Unnecessary copies

2. **Implement bytes::Bytes for message storage** (8 hours)
   - Replace: Vec<u8> with Bytes (reference-counted, zero-copy)
   - Files: chronik-protocol, chronik-wal, chronik-storage
   - Benefit: Multiple readers share same memory

3. **Use io_uring splice for zero-copy I/O** (12 hours)
   - Linux: io_uring supports splice (kernel-to-kernel copy)
   - Replace: read() + write() with splice()
   - Benefit: Data never enters userspace

4. **Implement shared memory for cross-partition replication** (8 hours)
   - Use: `shared_memory` crate
   - Benefit: Leader → followers copy via memcpy, not TCP

**Testing Requirements**:
- Memory profiler (verify copy reduction)
- CPU profiler (verify CPU reduction)
- Performance benchmark (before/after)

**Success Criteria**:
- ✅ Memory copies reduced from 4-6 to 1-2
- ✅ CPU usage reduced 30-50%
- ✅ Throughput increased 2-5x

---

### P2.4: io_uring for All File I/O

**Priority**: P2 - MEDIUM
**Effort**: 24 hours
**Risk**: Medium (Linux-specific)
**Impact**: 50-100% file I/O throughput improvement

**Tasks**:

1. **Evaluate tokio-uring vs rio** (4 hours)
   - Compare: tokio-uring (async), rio (sync wrapper)
   - Choose: Based on stability and performance

2. **Replace tokio::fs with io_uring** (12 hours)
   - Files: chronik-wal, chronik-storage
   - Replace: All file operations (open, read, write, fsync)
   - Benefit: Kernel async I/O, no thread pool

3. **Add io_uring metrics** (2 hours)
   - Counter: `chronik_io_uring_ops_total{op="read|write|fsync"}`
   - Histogram: `chronik_io_uring_latency_seconds`

4. **Add fallback to tokio::fs** (4 hours)
   - If: io_uring not available (non-Linux, old kernel)
   - Fallback: Use tokio::fs
   - Ensure: Tests pass on all platforms

5. **Performance testing** (2 hours)
   - Benchmark: WAL write throughput
   - Target: 50-100% improvement

**Testing Requirements**:
- Unit tests on Linux with io_uring
- Unit tests on macOS/Windows (fallback path)
- Performance benchmark (Linux)

**Success Criteria**:
- ✅ WAL write throughput increased 50-100%
- ✅ file.sync_all() latency reduced 30-50%
- ✅ Tests pass on all platforms

---

## Phase 3: Observability & Reliability (Week 9-10)

**Goal**: Production-grade observability and fault tolerance
**Success Metrics**:
- MTTR < 5 minutes (mean time to recovery)
- 100% error context capture
- 95% test coverage on critical paths

### P3.1: Distributed Tracing with OpenTelemetry

**Priority**: P3 - MEDIUM
**Effort**: 20 hours
**Risk**: Low
**Impact**: Enables end-to-end latency debugging

**Tasks**:

1. **Add OpenTelemetry dependencies** (2 hours)
   ```toml
   [dependencies]
   opentelemetry = "0.21"
   opentelemetry-jaeger = "0.20"
   tracing-opentelemetry = "0.22"
   ```

2. **Instrument critical paths** (12 hours)
   - Produce request: Client → Kafka handler → WAL → Response
   - Fetch request: Client → Kafka handler → Storage → Response
   - Metadata operation: Client → Raft → Apply → Response
   - Forwarding: Non-leader → Leader → Response

   ```rust
   #[tracing::instrument(skip(self, request))]
   async fn handle_produce(&self, request: ProduceRequest) -> Result<ProduceResponse> {
       let span = tracing::info_span!(
           "produce_request",
           topic = %request.topic,
           partition = request.partition,
           message_count = request.records.len(),
       );

       async move {
           // ... handler logic ...
       }.instrument(span).await
   }
   ```

3. **Configure Jaeger exporter** (2 hours)
   - Endpoint: Jaeger collector (localhost:14268 or cloud)
   - Sampling: 1% (low overhead in production)
   - Batch: 100 spans per batch

4. **Add span events for critical points** (2 hours)
   - Event: "wal_write_start", "wal_write_complete"
   - Event: "raft_consensus_start", "raft_consensus_complete"
   - Event: "forward_start", "forward_complete"

5. **Create Grafana dashboards** (2 hours)
   - Dashboard: Request latency breakdown (by span)
   - Dashboard: Error rate by operation
   - Dashboard: Throughput by topic/partition

**Testing Requirements**:
- Verify spans appear in Jaeger UI
- Verify latency breakdown matches manual timing
- Verify < 5% performance overhead

**Success Criteria**:
- ✅ All critical paths instrumented
- ✅ Latency breakdown visible in Jaeger
- ✅ Performance overhead < 5%

---

### P3.2: Circuit Breakers for Forwarding

**Priority**: P3 - MEDIUM
**Effort**: 12 hours
**Risk**: Low
**Impact**: Prevents cascade failures when leader unhealthy

**Tasks**:

1. **Implement circuit breaker** (6 hours)
   ```rust
   struct CircuitBreaker {
       state: Arc<Mutex<CircuitState>>,
       failure_threshold: u32,     // Open after N failures
       success_threshold: u32,      // Close after N successes
       timeout: Duration,           // Half-open after timeout
   }

   enum CircuitState {
       Closed { failure_count: u32 },
       Open { opened_at: Instant },
       HalfOpen { success_count: u32 },
   }

   impl CircuitBreaker {
       async fn call<F, T>(&self, f: F) -> Result<T>
       where
           F: Future<Output = Result<T>>,
       {
           let state = self.state.lock().await;
           match *state {
               CircuitState::Open { opened_at } => {
                   if opened_at.elapsed() > self.timeout {
                       // Transition to half-open
                       *state = CircuitState::HalfOpen { success_count: 0 };
                   } else {
                       return Err(anyhow!("Circuit breaker open"));
                   }
               }
               _ => {}
           }
           drop(state);

           match f.await {
               Ok(result) => {
                   self.on_success().await;
                   Ok(result)
               }
               Err(e) => {
                   self.on_failure().await;
                   Err(e)
               }
           }
       }
   }
   ```

2. **Integrate with forwarding** (2 hours)
   - One circuit breaker per destination node
   - Wrap: forward_produce_to_leader() call
   - Fast-fail: If circuit open, return error immediately

3. **Add metrics** (2 hours)
   - Gauge: `chronik_circuit_breaker_state{node="...",state="open|half_open|closed"}`
   - Counter: `chronik_circuit_breaker_trips_total{node="..."}`

4. **Test failure scenarios** (2 hours)
   - Kill leader node
   - Verify: Circuit opens after N failures (e.g., 5)
   - Verify: Half-open after timeout (e.g., 30s)
   - Verify: Closes after N successes (e.g., 3)

**Testing Requirements**:
- Unit test circuit breaker state transitions
- Integration test with failing leader
- Verify fast-fail when circuit open

**Success Criteria**:
- ✅ Circuit opens after repeated failures
- ✅ Fast-fail when circuit open (< 1ms)
- ✅ Auto-recovery when leader healthy

---

### P3.3: Chaos Testing Framework

**Priority**: P3 - MEDIUM
**Effort**: 32 hours
**Risk**: Low
**Impact**: Finds bugs before production

**Tasks**:

1. **Design chaos testing scenarios** (4 hours)
   - Scenario: Random node crashes
   - Scenario: Network partitions (split-brain)
   - Scenario: Slow disk I/O (random delays)
   - Scenario: Leader election storms
   - Scenario: Clock skew

2. **Implement chaos controller** (12 hours)
   ```rust
   struct ChaosController {
       cluster: TestCluster,
       scenario: ChaosScenario,
       rng: StdRng,
   }

   impl ChaosController {
       async fn run(&mut self, duration: Duration) {
           let end = Instant::now() + duration;

           while Instant::now() < end {
               match self.next_chaos_event() {
                   ChaosEvent::KillNode(node_id) => {
                       self.cluster.kill_node(node_id).await;
                       tokio::time::sleep(Duration::from_secs(10)).await;
                       self.cluster.restart_node(node_id).await;
                   }
                   ChaosEvent::NetworkPartition { nodes_a, nodes_b } => {
                       self.cluster.partition_network(nodes_a, nodes_b).await;
                       tokio::time::sleep(Duration::from_secs(30)).await;
                       self.cluster.heal_network().await;
                   }
                   ChaosEvent::SlowDisk { node_id, delay } => {
                       self.cluster.inject_io_delay(node_id, delay).await;
                   }
               }
           }
       }
   }
   ```

3. **Implement invariant checker** (8 hours)
   - Invariant: No data loss (all written data can be read)
   - Invariant: No duplicate messages
   - Invariant: Consumer groups make progress
   - Invariant: Leader election completes within 10s

4. **Create chaos test suite** (6 hours)
   - Test: Random crashes during produce workload
   - Test: Network partition with split-brain
   - Test: Slow disk with high load
   - Test: All 3 scenarios combined

5. **Integrate with CI** (2 hours)
   - Run: Chaos tests on every PR (shorter duration)
   - Run: Nightly chaos tests (longer duration)
   - Alert: If invariants violated

**Testing Requirements**:
- Chaos tests run for 1 hour (PR), 8 hours (nightly)
- All invariants hold during chaos
- System recovers within 1 minute of chaos ending

**Success Criteria**:
- ✅ Chaos tests pass consistently
- ✅ No data loss during chaos
- ✅ Recovery time < 1 minute

---

### P3.4: Comprehensive Metrics and Alerting

**Priority**: P3 - MEDIUM
**Effort**: 16 hours
**Risk**: Low
**Impact**: Faster incident response

**Tasks**:

1. **Add missing metrics** (8 hours)
   - `chronik_produce_latency_seconds{phase="metadata|wal|total"}`
   - `chronik_fetch_latency_seconds{tier="wal|segment|tantivy"}`
   - `chronik_raft_consensus_latency_seconds`
   - `chronik_lock_contention_seconds{lock="raft_node|..."}`
   - `chronik_forward_success_rate`
   - `chronik_circuit_breaker_state`

2. **Create Prometheus recording rules** (2 hours)
   - Rule: p50/p95/p99 latencies
   - Rule: Error rates (per minute)
   - Rule: Throughput (messages/sec)

3. **Create Grafana dashboards** (4 hours)
   - Dashboard: System overview (throughput, latency, errors)
   - Dashboard: Raft health (consensus latency, leader stability)
   - Dashboard: WAL health (write latency, queue depth)
   - Dashboard: Forwarding (rate, latency, failures)

4. **Configure alerting rules** (2 hours)
   - Alert: Produce latency p99 > 100ms (warning), > 500ms (critical)
   - Alert: Error rate > 1% (warning), > 5% (critical)
   - Alert: Circuit breaker open > 1 minute (critical)
   - Alert: Lock hold time p99 > 50ms (warning)
   - Alert: Timeout rate > 1% (critical)

**Testing Requirements**:
- Verify metrics appear in Prometheus
- Verify dashboards render correctly
- Trigger alerts manually (verify PagerDuty fires)

**Success Criteria**:
- ✅ All critical paths have metrics
- ✅ Dashboards show real-time health
- ✅ Alerts fire correctly

---

## Phase 4: Scalability & Advanced Features (Week 11-14)

**Goal**: Support 100,000+ msg/s, horizontal scaling
**Success Metrics**:
- Throughput: 100,000+ msg/s (500x improvement)
- Latency: p99 < 3ms
- Support: 10,000+ topics, 100,000+ partitions

### P4.1: Partition-Level Parallelism

**Priority**: P4 - FUTURE
**Effort**: 40 hours
**Risk**: High
**Impact**: 10-100x throughput improvement

**Tasks**:

1. **Design partition sharding architecture** (8 hours)
   - Current: Single thread per topic
   - Proposed: Thread pool per partition (CPU-bound)
   - Benefit: Parallelize produce/fetch across partitions

2. **Implement partition worker pool** (16 hours)
   - Worker pool: One tokio task per partition
   - Queue: Bounded MPSC channel per partition
   - Backpressure: Block producer if queue full

3. **CPU affinity for worker threads** (8 hours)
   - Pin: Each partition to specific CPU core
   - Benefit: Better cache locality, reduced context switches

4. **Benchmark scalability** (8 hours)
   - Test: 1, 10, 100, 1000 partitions
   - Measure: Throughput per partition (should be constant)
   - Verify: Linear scalability

**Success Criteria**:
- ✅ Throughput scales linearly with partitions
- ✅ 100 partitions: 100,000+ msg/s
- ✅ CPU utilization balanced across cores

---

### P4.2: Read Replicas (Kafka Follower Fetching)

**Priority**: P4 - FUTURE
**Effort**: 48 hours
**Risk**: High
**Impact**: 3x read capacity (for 3-node cluster)

**Tasks**:

1. **Implement follower read support** (24 hours)
   - Allow: Consumers to fetch from followers (not just leader)
   - Guarantee: Read-your-writes consistency (check HWM)
   - Benefit: Distribute read load across all replicas

2. **Add consumer group balancing for reads** (16 hours)
   - Strategy: Assign consumers to nearest replica
   - Benefit: Reduce cross-AZ network traffic

3. **Add read-only mode for failed leaders** (8 hours)
   - If: Leader fails, promote follower to "read-only leader"
   - Consumers: Can still fetch (stale data OK for some use cases)
   - Producers: Return error (writes require quorum)

**Success Criteria**:
- ✅ Read capacity 3x (for 3-node cluster)
- ✅ Cross-AZ traffic reduced 67%
- ✅ Read-your-writes consistency maintained

---

### P4.3: Tiered Storage Optimization

**Priority**: P4 - FUTURE
**Effort**: 56 hours
**Risk**: Medium
**Impact**: Unlimited retention, lower cost

**Tasks**:

1. **Implement segment prefetching** (16 hours)
   - Predict: Which segments will be fetched next
   - Prefetch: Download from S3 before consumer requests
   - Cache: Keep in local LRU cache

2. **Add compression for S3 uploads** (12 hours)
   - Compress: Segments before uploading (zstd)
   - Benefit: 70-80% size reduction, lower S3 cost

3. **Implement segment merging** (16 hours)
   - Merge: Small segments into larger ones (in S3)
   - Benefit: Fewer S3 objects, faster listing

4. **Add Tantivy index caching** (12 hours)
   - Cache: Frequently accessed indexes in local disk
   - Evict: LRU when cache full
   - Benefit: 10x faster search for hot data

**Success Criteria**:
- ✅ Fetch from S3 latency < 100ms p99 (with prefetch)
- ✅ S3 storage cost reduced 70% (with compression)
- ✅ Search latency < 500ms p99 (with caching)

---

### P4.4: NUMA-Aware Memory Allocation

**Priority**: P4 - FUTURE
**Effort**: 32 hours
**Risk**: High (Linux-specific, requires careful profiling)
**Impact**: 20-40% throughput improvement on multi-socket servers

**Tasks**:

1. **Profile NUMA locality** (8 hours)
   - Tool: `numactl --hardware`, `perf mem`
   - Identify: Cross-NUMA memory accesses
   - Measure: Performance impact

2. **Implement NUMA-aware allocation** (16 hours)
   - Allocate: Partition data on same NUMA node as worker thread
   - Pin: Worker threads to specific NUMA node
   - Benefit: Local memory access, no cross-socket traffic

3. **Benchmark on multi-socket server** (8 hours)
   - Test: 2-socket server with 48 cores
   - Measure: Throughput with/without NUMA awareness
   - Target: 20-40% improvement

**Success Criteria**:
- ✅ Cross-NUMA traffic reduced 80%
- ✅ Throughput improved 20-40% on multi-socket
- ✅ No regression on single-socket

---

## Phase 5: Production Readiness (Week 15-16)

**Goal**: Production-ready release
**Success Metrics**:
- 95% test coverage on critical paths
- All runbooks written
- Documentation complete

### P5.1: Comprehensive Testing

**Priority**: P5 - CRITICAL FOR RELEASE
**Effort**: 40 hours
**Risk**: Low
**Impact**: Prevents bugs in production

**Tasks**:

1. **Unit test coverage to 95%** (16 hours)
   - Current: Unknown (add coverage reporting)
   - Target: 95% on critical paths
   - Tool: cargo-tarpaulin

2. **Integration tests for all failure scenarios** (12 hours)
   - Test: Leader crash during produce
   - Test: Follower crash during replication
   - Test: Network partition during consensus
   - Test: Disk full during WAL write
   - Test: All nodes crash simultaneously

3. **Performance regression tests** (8 hours)
   - Benchmark: Throughput, latency, CPU, memory
   - Alert: If performance degrades > 10%
   - Run: On every PR

4. **Soak testing** (4 hours setup, 7 days runtime)
   - Duration: 7 days continuous operation
   - Workload: Variable (spikes, steady state, low traffic)
   - Monitor: Memory leaks, file descriptor leaks, performance degradation
   - Success: No degradation after 7 days

**Success Criteria**:
- ✅ Test coverage > 95%
- ✅ All failure scenarios tested
- ✅ 7-day soak test passes

---

### P5.2: Operations Documentation

**Priority**: P5 - CRITICAL FOR RELEASE
**Effort**: 24 hours
**Risk**: Low
**Impact**: Reduces MTTR, enables self-service ops

**Tasks**:

1. **Write runbooks** (12 hours)
   - Runbook: "Cluster is down"
   - Runbook: "High latency"
   - Runbook: "Leader election storm"
   - Runbook: "Disk full"
   - Runbook: "Out of memory"
   - Runbook: "Circuit breaker open"

2. **Write architecture decision records (ADRs)** (6 hours)
   - ADR: Lock scope policy (no I/O while holding locks)
   - ADR: Timeout policy (all I/O operations)
   - ADR: Connection pooling strategy
   - ADR: Event-driven vs polling

3. **Write capacity planning guide** (4 hours)
   - How to: Size cluster for target throughput
   - How to: Choose partition count
   - How to: Configure replication factor
   - Resource requirements: CPU, memory, disk, network

4. **Write performance tuning guide** (2 hours)
   - How to: Identify bottlenecks
   - How to: Tune WAL settings
   - How to: Tune Raft settings
   - How to: Optimize client configuration

**Success Criteria**:
- ✅ All runbooks written and tested
- ✅ ADRs document key decisions
- ✅ Capacity planning guide used successfully

---

### P5.3: Security Hardening

**Priority**: P5 - CRITICAL FOR RELEASE
**Effort**: 32 hours
**Risk**: Medium
**Impact**: Prevents security vulnerabilities

**Tasks**:

1. **Add authentication to admin API** (8 hours)
   - Current: API key only
   - Add: mTLS for admin operations
   - Add: RBAC (role-based access control)

2. **Audit for common vulnerabilities** (12 hours)
   - Check: SQL injection (N/A - no SQL)
   - Check: Command injection (any `std::process::Command` usage?)
   - Check: Path traversal (file operations)
   - Check: Denial of service (unbounded allocations)

3. **Add rate limiting** (8 hours)
   - Limit: Requests per client IP
   - Limit: Metadata operations per second
   - Prevent: DoS via excessive requests

4. **Dependency security scanning** (4 hours)
   - Tool: cargo-audit
   - Run: On every PR
   - Policy: No high/critical vulnerabilities

**Success Criteria**:
- ✅ mTLS for admin operations
- ✅ No high/critical vulnerabilities
- ✅ Rate limiting prevents DoS

---

### P5.4: Release Preparation

**Priority**: P5 - CRITICAL FOR RELEASE
**Effort**: 16 hours
**Risk**: Low
**Impact**: Smooth production rollout

**Tasks**:

1. **Write upgrade guide** (4 hours)
   - How to: Upgrade from v2.2.8 to v3.0.0
   - Breaking changes: List and explain
   - Migration: Data migration if needed
   - Rollback: How to rollback if needed

2. **Write release notes** (4 hours)
   - Features: New features added
   - Improvements: Performance improvements
   - Bug fixes: Critical bugs fixed
   - Known issues: Any known limitations

3. **Create Docker images** (4 hours)
   - Build: Multi-arch (amd64, arm64)
   - Push: To Docker Hub, GHCR
   - Tag: Latest, version (v3.0.0)

4. **Create helm chart** (4 hours)
   - Chart: Kubernetes deployment
   - Values: Configurable resources, replicas
   - Test: Deploy to test cluster

**Success Criteria**:
- ✅ Upgrade guide tested on staging
- ✅ Docker images published
- ✅ Helm chart deploys successfully

---

### P5.5: Comprehensive Regression Testing

**Priority**: P5 - CRITICAL FOR RELEASE
**Effort**: 24 hours
**Risk**: Low
**Impact**: Prevents regressions from fixes, ensures release quality
**Source**: Lessons from v2.2.7/v2.2.8 releases (deployed without comprehensive testing)

**Problem**:
Recent releases (v2.2.7, v2.2.8) were deployed without comprehensive testing, leading to discovery of critical bugs in production:
- 3-hour deadlock (discovered AFTER v2.2.7 release)
- Topic auto-creation timeout (discovered AFTER v2.2.8 release)
- Cold start deadlock (discovered AFTER v2.2.8 release)

**Goal**:
Establish comprehensive regression test suite to verify fixes don't break existing functionality and catch issues BEFORE release.

**Tasks**:

1. **Cold Start Regression Tests** (8 hours)
   - Test: 3-node cluster cold start (clean Raft WAL)
     ```bash
     # Stop all nodes
     pkill -9 chronik-server
     # Clear Raft state
     rm -rf data/wal/__meta/__raft_metadata
     # Start all nodes simultaneously
     ./start-cluster.sh
     # Verify all listeners start within 10s
     # Verify stable leader election
     # Verify NO election storm
     ```
   - Test: 3-node cluster restart (existing Raft WAL)
     ```bash
     # Stop all nodes
     pkill chronik-server
     # Restart (keep Raft state)
     ./start-cluster.sh
     # Verify all nodes rejoin cluster
     # Verify existing data preserved
     ```
   - Test: Single-node mode (ensure not broken by cluster fixes)
     ```bash
     ./chronik-server start
     # Verify standalone mode still works
     ```
   - Success Criteria:
     - All 3 scenarios pass 100% of the time
     - No manual intervention required
     - Startup time < 30 seconds

2. **Performance Regression Tests** (8 hours)
   - **Baseline before fixes**:
     ```bash
     # Measure current throughput
     chronik-bench \
       --bootstrap-servers localhost:9092 \
       --topic perf-baseline \
       --mode produce \
       --concurrency 64 \
       --message-size 1024 \
       --duration 60s \
       --acks all
     # Record: throughput, p50/p95/p99 latency
     ```
   - **After each P0 fix**:
     - P0.1: Measure throughput (should NOT regress with timeouts)
     - P0.2: Measure throughput (should IMPROVE 10-50x from faster Raft)
     - P0.3: Measure throughput (no change expected)
   - **After each P1 fix**:
     - P1.1: Measure topic creation time (should improve 3-6x)
     - P1.2: Measure forwarding latency (should improve 20%)
     - P1.3: Measure produce latency (should improve 10-50x)
   - **Alert conditions**:
     - Throughput regresses > 10%
     - Latency p99 regresses > 20%
     - Topic creation regresses
   - Success Criteria:
     - NO performance regressions
     - Performance improvements match estimates

3. **Deadlock Stress Test** (8 hours)
   - **Setup**:
     ```bash
     # Simulate slow disk with cgroups
     cgcreate -g blkio:/slow-disk
     echo "8:0 10485760" > /sys/fs/cgroup/blkio/slow-disk/blkio.throttle.write_bps_device
     cgexec -g blkio:slow-disk ./chronik-server start
     ```
   - **Workload**:
     - High Raft activity (create 1000 topics)
     - High produce traffic (10,000 msg/s)
     - Random node restarts every 5 minutes
     - Slow disk I/O (throttled to 10 MB/s)
   - **Duration**: 24 hours
   - **Monitor**:
     - Lock hold time p99 (should be < 50ms even with slow disk)
     - Timeout events (should fire, not hang)
     - Deadlock detection (should be ZERO)
     - Recovery time after node restart (should be < 1 minute)
   - Success Criteria:
     - NO deadlocks in 24-hour test
     - All timeout events properly fire
     - Cluster remains responsive (produce succeeds within 5s p99)
     - No node hangs requiring manual intervention

**Testing Framework**:
```bash
# File: tests/regression/run_all.sh

#!/bin/bash
set -e

echo "=== Chronik Regression Test Suite ==="

# 1. Cold start tests
echo "Running cold start tests..."
./tests/regression/test_cold_start.sh
echo "✅ Cold start tests passed"

# 2. Performance baseline
echo "Running performance baseline..."
./tests/regression/test_performance_baseline.sh
echo "✅ Performance baseline recorded"

# 3. Deadlock stress test
echo "Running 24-hour deadlock stress test..."
./tests/regression/test_deadlock_stress.sh
echo "✅ Deadlock stress test passed"

echo "=== ALL REGRESSION TESTS PASSED ==="
```

**Automation**:
- Run: Before EVERY release
- Run: In CI on every PR (shorter duration: 1h stress test)
- Run: Nightly (full 24h stress test)
- Alert: PagerDuty if ANY test fails

**Documentation**:
- Create: `docs/REGRESSION_TESTING.md` with full test procedures
- Create: `tests/regression/README.md` with how to run tests
- Update: `CLAUDE.md` with new testing requirements

**Success Criteria**:
- ✅ All regression tests pass 100% of the time
- ✅ Performance baselines recorded for all phases
- ✅ 24-hour stress test passes without deadlocks
- ✅ Tests run automatically before every release
- ✅ CI fails if regression tests fail

**CRITICAL NOTE**: This task should run THROUGHOUT implementation, not just at the end. Run regression tests after EACH P0/P1 fix to catch regressions early.

---

## Summary Roadmap

### Timeline Overview

| Phase | Weeks | Key Deliverables | Success Metrics |
|-------|-------|------------------|-----------------|
| **Phase 0** | 1-2 | Critical stability fixes | 0 deadlocks, all I/O timeouts |
| **Phase 1** | 3-4 | Performance restoration | 10,000+ msg/s (50x improvement) |
| **Phase 2** | 5-8 | Advanced optimizations | 50,000+ msg/s (250x improvement) |
| **Phase 3** | 9-10 | Observability & reliability | MTTR < 5 min, 95% test coverage |
| **Phase 4** | 11-14 | Scalability | 100,000+ msg/s (500x improvement) |
| **Phase 5** | 15-16 | Production readiness | Documentation, security, release |

### Effort Summary

| Phase | Tasks | Total Hours | Team-Weeks (40h/week) |
|-------|-------|-------------|------------------------|
| Phase 0 | 5 | 30 | 0.75 |
| Phase 1 | 3 | 40 | 1.0 |
| Phase 2 | 4 | 96 | 2.4 |
| Phase 3 | 4 | 80 | 2.0 |
| Phase 4 | 4 | 176 | 4.4 |
| Phase 5 | 5 | 136 | 3.4 |
| **Total** | **25** | **558** | **13.95** |

**With 2 engineers**: ~7.0 weeks (~7 weeks)
**With 1 engineer**: ~14.0 weeks (~14 weeks)

**NEW Tasks Added** (based on v2.2.7/v2.2.8 testing):
- P0.4: Fix Cluster Startup Deadlock (4h) - **MUST BE DONE FIRST!**
- P0.5: Investigate Topic Auto-Creation Timeout (2h)
- P5.5: Comprehensive Regression Testing (24h)

### Success Metrics Progression

| Metric | Current (v2.2.8) | Phase 0 | Phase 1 | Phase 2 | Phase 4 | Target |
|--------|------------------|---------|---------|---------|---------|--------|
| **Throughput** | 201 msg/s | 201 | 10,000 | 50,000 | 100,000+ | **500x** |
| **Latency p99** | 200ms | 200ms | 20ms | 10ms | 3ms | **67x** |
| **Deadlock risk** | High | **Eliminated** | - | - | - | **0** |
| **Topic creation** | 30s | 30s | 5s | 5s | 1s | **30x** |
| **Availability** | 99% | 99.9% | 99.9% | 99.95% | 99.99% | **4 nines** |

---

## Risk Management

### High-Risk Items

1. **Smart client routing** (P2.1)
   - Risk: Protocol changes may break existing clients
   - Mitigation: Extensive compatibility testing with all client libraries
   - Fallback: Keep forwarding as fallback mechanism

2. **Zero-copy message handling** (P2.3)
   - Risk: Complex memory management, potential memory leaks
   - Mitigation: Extensive memory profiling, leak detection
   - Fallback: Can revert to copying if issues arise

3. **Partition-level parallelism** (P4.1)
   - Risk: Race conditions, data corruption
   - Mitigation: Chaos testing, invariant checking
   - Fallback: Single-threaded mode as fallback

### Dependencies

**Critical path**:
1. Phase 0 must complete before Phase 1 (stability before performance)
2. P1.3 (event-driven) depends on P0.2 (lock scope fix)
3. P2.1 (smart routing) depends on P1.2 (connection pool)
4. Phase 5 depends on Phases 0-4 (can't release without features)

**Parallelizable**:
- Phase 2 and Phase 3 can run in parallel (different areas)
- P3.1 (tracing) can start anytime
- P3.3 (chaos testing) can start after Phase 0

---

## Next Steps

1. **Review this backlog** with team
2. **Prioritize** based on business needs
3. **Assign** tasks to engineers
4. **Start with Phase 0** (critical stability)
5. **Iterate** based on learnings

**Ready to begin implementation!**
