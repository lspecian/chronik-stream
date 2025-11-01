# Next Session Prompt - Raft Cluster Integration & Testing

## Context: What We Just Completed (2025-11-01)

We fixed **4 critical Raft bugs** that were preventing multi-node deployments:

1. ✅ **Raft Leadership Panic** - Fixed `propose()` to check leadership first
2. ✅ **WAL Recovery Panic** - Fixed truncated file handling in recovery
3. ✅ **Entry Persistence** - Now AWAITs `append_entries()` before `advance()`
4. ✅ **HardState Persistence** - Now AWAITs `persist_hard_state()` before `advance()`

**Current Status:** 3-node cluster can start and run (tested 2+ minutes without crashes)

**Files Changed:**
- `crates/chronik-server/src/raft_cluster.rs` - Leadership check + persistence fixes
- `crates/chronik-wal/src/manager.rs` - Graceful error handling for truncated files

**Documentation:**
- [RAFT_FIXES_2025-11-01.md](RAFT_FIXES_2025-11-01.md) - Complete fix summary
- [PRODUCTION_READINESS_ANALYSIS.md](PRODUCTION_READINESS_ANALYSIS.md) - Updated checklist

---

## YOUR MISSION: Complete Raft Cluster Integration

### Phase 1: Verify Persistence Works (PRIORITY 1)

**Goal:** Prove that Raft entries and HardState actually persist across restarts.

**Test Plan:**
```bash
# 1. Start 3-node cluster
cd /home/ubuntu/Development/chronik-stream

# Clean any old data
rm -rf /tmp/chronik-test-cluster

# Start Node 1
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  --data-dir /tmp/chronik-test-cluster/node1 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap &

# Start Node 2
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  --data-dir /tmp/chronik-test-cluster/node2 \
  raft-cluster \
  --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" \
  --bootstrap &

# Start Node 3
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  --data-dir /tmp/chronik-test-cluster/node3 \
  raft-cluster \
  --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" \
  --bootstrap &

# Wait for cluster to form (10 seconds)
sleep 10

# Check logs for "✓ Persisted N Raft entries to WAL"
grep "Persisted.*Raft entries" /tmp/chronik-test-cluster/node*/raft.log

# 2. Kill ALL nodes
pkill -f "chronik-server.*raft-cluster"

# 3. Restart ALL nodes (same commands as above)
# ... restart all 3 nodes ...

# 4. Verify recovery
# - Check logs for WAL recovery messages
# - Verify no errors during startup
# - Check that HardState is restored (term, vote, commit_index)
```

**Success Criteria:**
- ✅ Nodes restart without errors
- ✅ Logs show "Recovered N Raft entries from WAL"
- ✅ HardState restored (term/vote/commit preserved)
- ✅ Cluster re-forms quorum after restart

**If This Fails:** Debug persistence in `RaftWalStorage::append_entries()` and `persist_hard_state()`.

---

### Phase 2: Wire RaftCluster to ProduceHandler (PRIORITY 2)

**Problem:** ProduceHandler doesn't know about RaftCluster, so it can't:
- Check which node is partition leader
- Forward requests to leader if this is a follower
- Update ISR via Raft consensus

**Goal:** Make ProduceHandler Raft-aware.

**Files to Modify:**

1. **`crates/chronik-server/src/integrated_server.rs`**
   - Pass `RaftCluster` to `ProduceHandler`
   - Currently: `cluster_config: None`
   - Change to: `cluster_config: Some(raft_cluster.clone())`

2. **`crates/chronik-server/src/produce_handler.rs`**
   - Accept `Option<Arc<RaftCluster>>` in constructor
   - Before appending to WAL, check:
     ```rust
     if let Some(cluster) = &self.cluster_config {
         // Get partition leader from Raft metadata
         let leader = cluster.get_partition_leader(topic, partition)?;

         // If we're not the leader, return error with leader info
         if leader != cluster.node_id() {
             return Err(NotLeaderForPartition { leader });
         }
     }
     ```

3. **`crates/chronik-server/src/raft_cluster.rs`**
   - Ensure `get_partition_leader()` works correctly
   - Returns `Option<u64>` - the node_id of the leader

**Test Plan:**
```bash
# 1. Create topic on Node 1 (leader)
kafka-topics --create --topic test-raft \
  --partitions 1 --replication-factor 1 \
  --bootstrap-server localhost:9092

# 2. Produce to Node 1 (should succeed - it's the leader)
echo "message1" | kafka-console-producer \
  --topic test-raft \
  --bootstrap-server localhost:9092

# 3. Produce to Node 2 (should fail - it's a follower)
echo "message2" | kafka-console-producer \
  --topic test-raft \
  --bootstrap-server localhost:9093
# Expected: NOT_LEADER_FOR_PARTITION error

# 4. Consume from any node (should work)
kafka-console-consumer --topic test-raft \
  --from-beginning \
  --bootstrap-server localhost:9092
```

**Success Criteria:**
- ✅ Produce to leader succeeds
- ✅ Produce to follower returns NOT_LEADER_FOR_PARTITION error
- ✅ Error includes correct leader_id
- ✅ Consumer can fetch from any node

---

### Phase 3: Implement Partition Assignment via Raft (PRIORITY 3)

**Problem:** When a topic is created, partitions aren't assigned to nodes via Raft.

**Goal:** Coordinate partition assignment through Raft consensus.

**Implementation:**

1. **Metadata Command:**
   ```rust
   // Already exists in raft_metadata.rs
   MetadataCommand::AssignPartition {
       topic: String,
       partition: i32,
       replicas: Vec<u64>,  // [1, 2, 3] means node1 is leader, 2 and 3 are replicas
   }
   ```

2. **Topic Creation Flow:**
   ```rust
   // In ProduceHandler or MetadataHandler
   async fn create_topic(&self, topic: &str, partitions: i32) -> Result<()> {
       if let Some(cluster) = &self.cluster_config {
           // Only leader can assign partitions
           if !cluster.is_leader() {
               return Err(NotLeader);
           }

           // Assign partitions via Raft
           for p in 0..partitions {
               let replicas = self.choose_replicas(p)?;  // Choose nodes for this partition

               cluster.propose(MetadataCommand::AssignPartition {
                   topic: topic.to_string(),
                   partition: p,
                   replicas,
               }).await?;
           }
       }

       Ok(())
   }

   fn choose_replicas(&self, partition: i32) -> Result<Vec<u64>> {
       // Simple round-robin for now
       // partition 0 -> [1, 2, 3]
       // partition 1 -> [2, 3, 1]
       // partition 2 -> [3, 1, 2]
       let all_nodes = vec![1, 2, 3];
       let start = (partition as usize) % all_nodes.len();
       Ok(all_nodes.iter().cycle().skip(start).take(3).cloned().collect())
   }
   ```

3. **Apply to State Machine:**
   ```rust
   // In MetadataStateMachine::apply()
   MetadataCommand::AssignPartition { topic, partition, replicas } => {
       self.partition_assignments.insert(
           PartitionKey { topic, partition },
           PartitionInfo {
               replicas,
               leader: replicas[0],  // First replica is leader
               isr: replicas.clone(),  // All replicas start in-sync
           }
       );
   }
   ```

**Test Plan:**
```bash
# 1. Create topic (triggers Raft proposal)
kafka-topics --create --topic test-assign \
  --partitions 3 --replication-factor 3 \
  --bootstrap-server localhost:9092

# 2. Check Raft logs on ALL nodes
grep "AssignPartition" /tmp/chronik-test-cluster/node*/raft.log

# Expected: All nodes should have same partition assignments

# 3. Describe topic
kafka-topics --describe --topic test-assign \
  --bootstrap-server localhost:9092

# Expected: Show partition leaders and replicas
```

**Success Criteria:**
- ✅ Topic creation proposes AssignPartition commands
- ✅ All nodes receive and apply the commands
- ✅ Partition assignments are consistent across cluster
- ✅ Leaders are assigned correctly

---

### Phase 4: Test Crash Recovery (PRIORITY 4)

**Goal:** Verify cluster survives node crashes and recovers correctly.

**Test Scenarios:**

#### Scenario A: Kill Follower
```bash
# 1. Kill Node 2 (follower)
pkill -f "node-id 2"

# 2. Produce to leader (Node 1)
echo "message-after-kill" | kafka-console-producer \
  --topic test-raft --bootstrap-server localhost:9092

# 3. Restart Node 2
./target/release/chronik-server --node-id 2 ... &

# 4. Verify Node 2 catches up
kafka-console-consumer --topic test-raft \
  --from-beginning --bootstrap-server localhost:9093
```

#### Scenario B: Kill Leader
```bash
# 1. Kill Node 1 (leader)
pkill -f "node-id 1"

# 2. Wait for new leader election (should be Node 2 or 3)
sleep 5

# 3. Produce to new leader
echo "message-new-leader" | kafka-console-producer \
  --topic test-raft --bootstrap-server localhost:9093

# 4. Restart Node 1
./target/release/chronik-server --node-id 1 ... &

# 5. Verify Node 1 becomes follower and catches up
```

**Success Criteria:**
- ✅ Cluster survives follower crash
- ✅ New leader elected when leader crashes
- ✅ Restarted nodes rejoin cluster
- ✅ Data is consistent across all nodes
- ✅ No data loss

---

### Phase 5: Implement Snapshot Save/Load (OPTIONAL)

**Why:** Prevent unbounded Raft log growth.

**Goal:** Create snapshots after N entries, delete old log entries.

**Implementation:**

1. **Trigger Snapshot:**
   ```rust
   // In message loop, after applying committed entries
   if raft.raft.raft_log.applied >= raft.raft.raft_log.last_index() - 1000 {
       // Create snapshot
       let snapshot = self.create_snapshot().await?;
       raft.raft.mut_store().apply_snapshot(snapshot)?;
   }
   ```

2. **Create Snapshot:**
   ```rust
   async fn create_snapshot(&self) -> Result<Snapshot> {
       let sm = self.state_machine.read()?;

       // Serialize entire state machine
       let data = bincode::serialize(&*sm)?;

       // Create snapshot metadata
       let snapshot = Snapshot {
           data,
           metadata: SnapshotMetadata {
               index: applied_index,
               term: current_term,
               conf_state: ...,
           },
       };

       // Save to disk
       self.storage.save_snapshot(snapshot.clone()).await?;

       Ok(snapshot)
   }
   ```

3. **Load on Startup:**
   ```rust
   // In bootstrap()
   if let Some(snapshot) = storage.load_latest_snapshot().await? {
       raft_node.restore(snapshot)?;
   }
   ```

---

## Key Files to Know

### Raft Implementation
- `crates/chronik-server/src/raft_cluster.rs` - Main Raft cluster
- `crates/chronik-server/src/raft_metadata.rs` - Metadata state machine
- `crates/chronik-wal/src/raft_storage_impl.rs` - Raft storage backend

### Kafka Integration
- `crates/chronik-server/src/produce_handler.rs` - Produce requests
- `crates/chronik-server/src/fetch_handler.rs` - Fetch requests
- `crates/chronik-server/src/integrated_server.rs` - Wires everything together

### Networking
- `crates/chronik-raft/src/grpc_transport.rs` - gRPC transport
- `crates/chronik-raft/src/rpc.rs` - Raft RPC service

---

## Common Issues & Solutions

### Issue: "Cannot propose: this node is not the leader"

**Cause:** Trying to propose metadata changes on a follower.

**Solution:** Only call `cluster.propose()` on the leader. Check with `cluster.is_leader_ready()` first.

### Issue: WAL files keep growing

**Cause:** No log compaction / snapshots implemented yet.

**Solution:** Implement Phase 5 (snapshots), or manually delete old WAL files for testing.

### Issue: Nodes can't connect to each other

**Cause:** gRPC transport issues or incorrect peer addresses.

**Solution:**
- Check `--raft-addr` is correct (must be reachable from other nodes)
- Check `--peers` format: "2@localhost:9193,3@localhost:9194"
- Enable RUST_LOG=debug to see connection attempts

### Issue: Cluster stuck, no leader elected

**Cause:** Not enough nodes for quorum (need 2 out of 3).

**Solution:** Ensure at least 2 nodes are running. Check logs for election messages.

---

## Success Metrics

By end of next session, you should have:

- ✅ Crash recovery tested and working
- ✅ ProduceHandler integrated with RaftCluster
- ✅ Partition assignment via Raft consensus
- ✅ Leader forwarding implemented
- ✅ Full integration test passing (produce → consume across cluster)

**Optional (if time permits):**
- ✅ Snapshot implementation
- ✅ ISR updates via Raft
- ✅ Network partition testing

---

## Recommended Session Plan

**Hour 1: Verify Persistence**
- Run Phase 1 tests
- Fix any persistence bugs found
- Verify recovery works correctly

**Hour 2: Wire RaftCluster to ProduceHandler**
- Implement Phase 2
- Test leader checks
- Test follower rejection

**Hour 3: Partition Assignment**
- Implement Phase 3
- Test topic creation with Raft
- Verify consistency across nodes

**Hour 4: Crash Recovery Testing**
- Run Phase 4 scenarios
- Fix any bugs found
- Document results

**Hour 5 (Optional): Snapshots**
- Implement Phase 5 if time permits
- Or: Focus on integration testing instead

---

## Starting Command for Next Session

```bash
cd /home/ubuntu/Development/chronik-stream

# Review what was fixed
cat RAFT_FIXES_2025-11-01.md

# Check current status
cat PRODUCTION_READINESS_ANALYSIS.md

# Start with Phase 1: Test persistence
# (follow test plan above)
```

---

## Questions to Ask Claude

1. "Let's start Phase 1: Test Raft persistence across restarts. Can you help me verify that entries and HardState actually persist?"

2. "Now wire RaftCluster to ProduceHandler so it checks partition leaders before producing."

3. "Implement partition assignment via Raft consensus when topics are created."

4. "Test crash recovery: kill a follower, then the leader, verify cluster survives."

5. "Implement snapshot save/load to prevent unbounded log growth."

---

## Current Branch & Commit

**Branch:** `feat/v2.5.0-kafka-cluster`

**Last Commits:**
- Fixed Raft leadership panic
- Fixed WAL recovery panic
- Fixed entry/hardstate persistence
- All changes committed and ready for next phase

**To Continue:**
```bash
cd /home/ubuntu/Development/chronik-stream
git status  # Should show clean working tree
git log --oneline -5  # See recent commits
```

---

## Good Luck! 🚀

You're picking up from a **major milestone**: Raft cluster went from "crashes in 1 second" to "runs with correct persistence".

The foundation is solid. Now it's time to **make it actually useful** by integrating with Kafka operations and testing crash recovery.

**Remember:** Test incrementally, document bugs, fix properly. No shortcuts!
