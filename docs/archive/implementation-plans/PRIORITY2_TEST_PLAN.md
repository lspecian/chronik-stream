# Priority 2: Zero-Downtime Node Addition - Testing Plan

**Version**: v2.6.0
**Status**: Ready for Testing
**Date**: 2025-11-02

---

## Test Environment Setup

### Prerequisites

1. **Build the binary:**
   ```bash
   cargo build --release --bin chronik-server
   ```

2. **Create cluster config files:**
   ```bash
   # Use the provided example as base
   cp examples/cluster-local-3node.toml /tmp/cluster-node1.toml
   cp examples/cluster-local-3node.toml /tmp/cluster-node2.toml
   cp examples/cluster-local-3node.toml /tmp/cluster-node3.toml

   # Edit each file to set correct node_id (1, 2, 3)
   ```

3. **Clean data directories:**
   ```bash
   rm -rf /tmp/node-{1,2,3,4}
   ```

---

## Test Scenario 1: Basic 3→4 Node Expansion

**Objective**: Verify that a 4th node can be added to a running 3-node cluster without downtime.

### Steps

#### 1.1. Start 3-Node Cluster

```bash
# Terminal 1 - Node 1
./target/release/chronik-server start \
  --config /tmp/cluster-node1.toml \
  --data-dir /tmp/node-1 \
  --node-id 1

# Terminal 2 - Node 2
./target/release/chronik-server start \
  --config /tmp/cluster-node2.toml \
  --data-dir /tmp/node-2 \
  --node-id 2

# Terminal 3 - Node 3
./target/release/chronik-server start \
  --config /tmp/cluster-node3.toml \
  --data-dir /tmp/node-3 \
  --node-id 3
```

**Wait 10 seconds for leader election.**

**Expected Output:**
```
✓ Raft cluster bootstrapped successfully
✓ Raft gRPC server started
✓ Raft message loop started
✓ Admin API available at http://0.0.0.0:10001/admin
✓ Partition Rebalancer started
```

#### 1.2. Verify Cluster Health

```bash
# Check health on all nodes
curl http://localhost:10001/admin/health | jq
curl http://localhost:10002/admin/health | jq
curl http://localhost:10003/admin/health | jq
```

**Expected:**
- One node shows `"is_leader": true`
- Two nodes show `"is_leader": false`
- All nodes show `"cluster_nodes": [1, 2, 3]`

#### 1.3. Create Test Topic

```bash
# Using kafka-python (if available)
python3 << 'EOF'
from kafka.admin import KafkaAdminClient, NewTopic

admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic(name='test-topic', num_partitions=6, replication_factor=3)
admin.create_topics([topic])
print("✓ Created topic 'test-topic' with 6 partitions")
EOF
```

**Or using chronik-server directly (if no kafka tools available):**
```bash
# Produce to trigger auto-topic-creation
echo "test message" | nc localhost 9092  # Will create topic
```

#### 1.4. Prepare Node 4 Config

```bash
# Create config for node 4
cat > /tmp/cluster-node4.toml << 'EOF'
node_id = 4

replication_factor = 3
min_insync_replicas = 2

[bind]
kafka = "0.0.0.0:9095"
wal = "0.0.0.0:9294"
raft = "0.0.0.0:5004"

[advertise]
kafka = "localhost:9095"
wal = "localhost:9294"
raft = "localhost:5004"

[[peers]]
id = 1
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 2
kafka = "localhost:9093"
wal = "localhost:9292"
raft = "localhost:5002"

[[peers]]
id = 3
kafka = "localhost:9094"
wal = "localhost:9293"
raft = "localhost:5003"

[[peers]]
id = 4
kafka = "localhost:9095"
wal = "localhost:9294"
raft = "localhost:5004"
EOF
```

#### 1.5. Start Node 4 (Not Yet in Cluster)

```bash
# Terminal 4 - Node 4
./target/release/chronik-server start \
  --config /tmp/cluster-node4.toml \
  --data-dir /tmp/node-4 \
  --node-id 4
```

**Note**: Node 4 is running but not yet a voting member.

#### 1.6. Add Node 4 to Cluster

```bash
# Terminal 5 - CLI
./target/release/chronik-server cluster add-node 4 \
  --kafka localhost:9095 \
  --wal localhost:9294 \
  --raft localhost:5004 \
  --config /tmp/cluster-node1.toml
```

**Expected Output:**
```
Discovering cluster leader...
✓ Found leader: node X (port 1000X)
Sending add-node request to leader...
✓ Node 4 addition proposed. Waiting for Raft consensus...

Node Details:
  ID:    4
  Kafka: localhost:9095
  WAL:   localhost:9294
  Raft:  localhost:5004

The node will be added to the cluster once Raft achieves consensus.
```

#### 1.7. Verify Node 4 Added

**Wait 5 seconds, then check:**

```bash
# Check all nodes now show 4 members
curl http://localhost:10001/admin/health | jq '.cluster_nodes'
curl http://localhost:10002/admin/health | jq '.cluster_nodes'
curl http://localhost:10003/admin/health | jq '.cluster_nodes'
curl http://localhost:10004/admin/health | jq '.cluster_nodes'
```

**Expected:**
```json
[1, 2, 3, 4]  // All nodes
```

**Check logs for ConfChange application:**
```bash
grep "ConfChange" /tmp/node-*/log  # Should show node 4 added
```

### Success Criteria

- ✅ Node 4 successfully added to Raft voters
- ✅ All nodes report 4 cluster members
- ✅ No leader election during addition
- ✅ No errors in any node logs

---

## Test Scenario 2: Partition Rebalancing Verification

**Objective**: Verify that partitions are automatically rebalanced after node addition.

### Steps

#### 2.1. Check Initial Partition Assignment (Before Node 4)

```bash
# Query partition assignments (via metadata)
# This requires access to Raft metadata - check logs
grep "AssignPartition" /tmp/node-1/log | tail -20
```

**Expected (3 nodes, RF=3):**
```
P0 → [1, 2, 3]
P1 → [2, 3, 1]
P2 → [3, 1, 2]
P3 → [1, 2, 3]
P4 → [2, 3, 1]
P5 → [3, 1, 2]
```

#### 2.2. Wait for Rebalancer (30 seconds)

```bash
# Wait for rebalancer to detect membership change
sleep 35

# Check logs for rebalancing
grep "Rebalancing" /tmp/node-*/log
```

**Expected Output:**
```
Cluster membership changed: 3 → 4 nodes - triggering rebalance
Rebalancing topic 'test-topic' across 4 nodes
✓ Rebalancing complete
```

#### 2.3. Verify New Partition Assignment (After Node 4)

**Expected (4 nodes, RF=3):**
```
P0 → [1, 2, 3]  // No change
P1 → [2, 3, 4]  // Node 4 added!
P2 → [3, 4, 1]  // Node 4 added!
P3 → [4, 1, 2]  // Node 4 added!
P4 → [1, 2, 3]  // No change
P5 → [2, 3, 4]  // Node 4 added!
```

**Verification:**
```bash
# Check Raft proposals for new assignments
grep "Proposed new assignment" /tmp/node-*/log | grep "test-topic"
```

### Success Criteria

- ✅ Rebalancer detected membership change within 30s
- ✅ All partitions reassigned to include node 4
- ✅ Even distribution: node 4 appears in 4-5 partition replica lists
- ✅ No errors during rebalancing

---

## Test Scenario 3: Leader Stability During Addition

**Objective**: Verify that the leader remains stable during node addition (no unnecessary elections).

### Steps

#### 3.1. Identify Current Leader

```bash
# Before adding node 4
for port in 10001 10002 10003; do
  echo -n "Port $port: "
  curl -s http://localhost:$port/admin/health | jq -r '.is_leader'
done
```

**Record the leader node ID.**

#### 3.2. Add Node 4 (Repeat from Scenario 1)

```bash
./target/release/chronik-server cluster add-node 4 \
  --kafka localhost:9095 \
  --wal localhost:9294 \
  --raft localhost:5004 \
  --config /tmp/cluster-node1.toml
```

#### 3.3. Verify Leader Unchanged

```bash
# After adding node 4
for port in 10001 10002 10003 10004; do
  echo -n "Port $port: "
  curl -s http://localhost:$port/admin/health | jq -r '.is_leader'
done
```

**Expected**: Same node is leader as before.

#### 3.4. Check for Elections in Logs

```bash
grep "became leader\|election\|vote" /tmp/node-*/log | tail -20
```

**Expected**: No new election messages after node 4 addition.

### Success Criteria

- ✅ Leader remains the same before and after addition
- ✅ No election messages in logs during addition
- ✅ Raft term does not increase

---

## Test Scenario 4: Client Impact (Zero Downtime)

**Objective**: Verify that clients experience zero downtime during node addition.

### Steps

#### 4.1. Start Continuous Producer

```bash
# Terminal 6 - Continuous producer
while true; do
  timestamp=$(date +%s%N)
  echo "msg-$timestamp" | nc localhost 9092 2>&1 | grep -q "ERROR" && echo "✗ Producer error at $timestamp"
  sleep 0.1
done
```

**Or with Python:**
```python
# continuous_producer.py
from kafka import KafkaProducer
import time

producer = KafkaProducer(bootstrap_servers='localhost:9092')
count = 0

while True:
    try:
        timestamp = int(time.time() * 1000)
        msg = f"msg-{timestamp}".encode()
        producer.send('test-topic', msg)
        count += 1
        if count % 10 == 0:
            print(f"✓ Sent {count} messages", flush=True)
        time.sleep(0.1)
    except Exception as e:
        print(f"✗ Error at {timestamp}: {e}", flush=True)
```

#### 4.2. Add Node 4 While Producer Running

```bash
# Terminal 5
./target/release/chronik-server cluster add-node 4 \
  --kafka localhost:9095 \
  --wal localhost:9294 \
  --raft localhost:5004 \
  --config /tmp/cluster-node1.toml
```

#### 4.3. Monitor Producer Output

**Watch Terminal 6 for any errors.**

**Expected**: No errors, producer continues uninterrupted.

#### 4.4. Verify All Messages Persisted

```bash
# Stop producer (Ctrl+C)

# Count messages produced (from logs)
PRODUCED_COUNT=$(grep "Sent" producer.log | tail -1 | awk '{print $3}')

# Consume all messages
python3 << 'EOF'
from kafka import KafkaConsumer

consumer = KafkaConsumer(
    'test-topic',
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000
)

count = 0
for msg in consumer:
    count += 1

print(f"✓ Consumed {count} messages")
EOF
```

**Expected**: Consumed count ≈ Produced count (within 1-2 messages tolerance).

### Success Criteria

- ✅ No producer errors during node addition
- ✅ No message loss (produced ≈ consumed)
- ✅ Producer latency remains stable (no spikes)
- ✅ Zero client impact

---

## Performance Baseline Measurements

### Before Node Addition (3 nodes)

```bash
# Measure produce throughput
time python3 << 'EOF'
from kafka import KafkaProducer

producer = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(1000):
    producer.send('test-topic', f"msg-{i}".encode())
producer.flush()
EOF
```

**Record time: `_____` seconds**

### After Node Addition (4 nodes)

```bash
# Repeat same test
time python3 << 'EOF'
from kafka import KafkaProducer

producer = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(1000):
    producer.send('test-topic', f"msg-{i}".encode())
producer.flush()
EOF
```

**Record time: `_____` seconds**

**Expected**: Similar or slightly better performance (more nodes to distribute load).

---

## Failure Scenarios

### Test 5.1: Add Already-Existing Node

```bash
# Try to add node 1 (already exists)
./target/release/chronik-server cluster add-node 1 \
  --kafka localhost:9092 \
  --wal localhost:9291 \
  --raft localhost:5001 \
  --config /tmp/cluster-node1.toml
```

**Expected Output:**
```
✗ Failed to add node: Node 1 already exists in cluster
```

### Test 5.2: Add Node from Follower

```bash
# Identify a follower node
FOLLOWER_PORT=$(curl -s http://localhost:10001/admin/health | jq -r 'if .is_leader then empty else 10001 end')

# Try to add node directly via follower's admin API
curl -X POST http://localhost:$FOLLOWER_PORT/admin/add-node \
  -H "Content-Type: application/json" \
  -d '{"node_id": 5, "kafka_addr": "localhost:9096", "wal_addr": "localhost:9295", "raft_addr": "localhost:5005"}'
```

**Expected:**
```json
{
  "success": false,
  "message": "Cannot add node: this node is not the leader"
}
```

### Test 5.3: Invalid Address Format

```bash
./target/release/chronik-server cluster add-node 5 \
  --kafka localhost \
  --wal localhost:9295 \
  --raft localhost:5005 \
  --config /tmp/cluster-node1.toml
```

**Expected:**
```
✗ Failed to add node: Invalid address format
```

---

## Test Summary Template

```
=====================================================
Priority 2 Testing Summary
=====================================================

Test Date: ___________
Tester: ___________
Version: v2.6.0

Scenario 1: Basic 3→4 Node Expansion
  ☐ Node 4 added successfully
  ☐ All nodes report 4 members
  ☐ No leader election
  ☐ No errors

Scenario 2: Partition Rebalancing
  ☐ Rebalancer triggered within 30s
  ☐ Partitions reassigned correctly
  ☐ Node 4 included in assignments
  ☐ No errors

Scenario 3: Leader Stability
  ☐ Leader unchanged during addition
  ☐ No election messages
  ☐ Raft term stable

Scenario 4: Zero Client Impact
  ☐ No producer errors
  ☐ No message loss
  ☐ Latency stable

Performance:
  Before (3 nodes): _____ msg/s
  After (4 nodes):  _____ msg/s

Failure Tests:
  ☐ Duplicate node rejected
  ☐ Follower add rejected
  ☐ Invalid address rejected

Overall: ☐ PASS  ☐ FAIL

Notes:
_______________________________________
_______________________________________
```

---

## Cleanup

```bash
# Kill all nodes
pkill -f chronik-server

# Clean data directories
rm -rf /tmp/node-{1,2,3,4}

# Clean logs
rm -f /tmp/*.log
```

---

**Testing complete! Proceed to Step 5 (Documentation) once all scenarios pass.**
