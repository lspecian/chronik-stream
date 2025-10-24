# CRITICAL: Raft Message Ordering Bug

**Status**: ROOT CAUSE IDENTIFIED - Requires immediate fix for v1.3.67
**Priority**: P0 - BLOCKS RC RELEASE
**Date**: 2025-10-24

## Executive Summary

Java `kafka-console-producer` hangs and fails to produce messages due to **massive Raft leader election churn** (thousands of elections). Root cause is **unordered message delivery** in the Raft background loop, violating Raft's strict ordering requirements.

## Symptom: Leader Election Churn

**Observable behavior**:
- Raft replicas constantly flip-flopping between Leader/Candidate/Follower states
- Term numbers in the THOUSANDS (e.g., java-test topic: term=4070, 4075, 4089, 4094, 4117, 4157...)
- Multiple leader changes within 1 second for same partition
- Java clients hang indefinitely or timeout with NOT_LEADER_FOR_FOLLOWER errors
- Python clients work (due to retry logic tolerating transient leader changes)

**Evidence from logs**:
```bash
# __meta partition: 8 elections in just a few seconds
term=255, 256, 257, 258, 259, 260, 262, 263

# java-test partition 0: thousands of elections
Node 3: term=4070, 4075, 4089, 4094, 4157, 4165

# Partition assignments changing constantly (09:08:18 timestamp):
09:08:18.292981 - java-client-test/0 leader is now node 1
09:08:18.334359 - java-client-test/2 leader is now node 1
09:08:18.236791 - java-client-test/2 leader is now node 3  (flip-flop!)
09:08:18.373203 - java-client-test/0 leader is now node 3  (flip-flop!)
```

## Root Cause: Unordered Message Delivery

**Location**: `crates/chronik-server/src/raft_integration.rs:632-642`

**The Bug**:
```rust
// Send messages to peers via gRPC
if !messages.is_empty() {
    info!("Sending {} messages to peers for {}-{}", messages.len(), topic, partition);

    for msg in messages {
        let to = msg.to;
        let topic = topic.clone();
        let client = raft_client.clone();

        tokio::spawn(async move {  // ❌❌❌ SPAWNS UNORDERED ASYNC TASKS!
            if let Err(e) = client.send_message(&topic, partition, to, msg).await {
                error!("Failed to send message to peer {}: {}", to, e);
            }
        });
    }
}
```

**Why this breaks Raft**:

1. **Raft requires strict message ordering** for correctness
   - Vote requests/responses MUST be processed in term order
   - Heartbeats MUST be delivered promptly and in order
   - AppendEntries messages MUST arrive before later messages

2. **`tokio::spawn()` provides NO ORDERING GUARANTEES**
   - Each message becomes an independent async task in the Tokio scheduler
   - Tasks can be delayed arbitrarily (milliseconds to seconds)
   - Tasks can complete out of order (later messages arrive before earlier ones)

3. **The Result: Election Chaos**
   - Node A sends vote request for term=100
   - Node A times out waiting for responses (vote responses delayed in Tokio queue)
   - Node A starts new election for term=101
   - **NOW** vote responses for term=100 finally arrive
   - Node A ignores them (lower term) and logs: `ignored a message with lower term from X, msg term: 100, term: 101`
   - This happens THOUSANDS of times → term numbers explode

**Evidence from logs**:
```
[INFO] raft::raft: ignored a message with lower term from 3, raft_id: 1,
       msg term: 150, msg type: MsgRequestVoteResponse, term: 151, from: 3

[INFO] raft::raft: ignored a message with lower term from 2, raft_id: 1,
       msg term: 7, msg type: MsgRequestVoteResponse, term: 8, from: 2
```

Vote responses arriving ONE TERM LATE because they were delayed in the Tokio spawn queue!

## Impact Analysis

**Why Java clients fail but Python clients work**:
- **Java kafka-console-producer/consumer**: No built-in leader retry logic, gives up quickly
- **Python kafka-python**: Has aggressive retry logic (default: 3 retries), tolerates transient failures

**Severity**:
- ✅ **Cluster is functionally correct** (Raft safety properties maintained, just inefficient)
- ❌ **Completely unusable** for Java clients (hang indefinitely or fail)
- ⚠️ **Degraded performance** for Python clients (excessive retries, high latency)
- ❌ **BLOCKS RC RELEASE** per user requirement: "we won't release with issues"

## The Fix: Ordered Message Delivery

**Option 1: Sequential Sending** (Simplest, recommended for v1.3.67)
```rust
// Send messages to peers via gRPC SEQUENTIALLY
if !messages.is_empty() {
    info!("Sending {} messages to peers for {}-{}", messages.len(), topic, partition);

    for msg in messages {
        let to = msg.to;
        let topic = topic.clone();
        let client = raft_client.clone();

        // Send synchronously (awaiting each before next)
        if let Err(e) = client.send_message(&topic, partition, to, msg).await {
            error!("Failed to send message to peer {}: {}", to, e);
        }
    }
}
```

**Pros**:
- ✅ Guarantees message ordering (Raft correctness)
- ✅ Simple 3-line change
- ✅ No new dependencies or complex logic

**Cons**:
- ⚠️ Slightly higher latency per tick (sequential network calls)
- ⚠️ One slow peer can block other messages

**Option 2: Per-Peer Ordered Channels** (More complex, future optimization)
```rust
// Per-peer FIFO queue ensures ordering to each peer independently
struct PeerMessageQueue {
    peer_id: u64,
    queue: mpsc::UnboundedSender<Message>,
    worker: JoinHandle<()>,  // Background worker sends in order
}
```

**Pros**:
- ✅ Parallelism across peers (send to peer 1 & 2 concurrently)
- ✅ Ordering guarantee per peer
- ✅ Better scalability for large clusters

**Cons**:
- ❌ Complex implementation (queue management, worker lifecycle)
- ❌ Higher memory usage (one channel per peer)
- ❌ Overkill for 3-node test cluster

## Recommended Approach

**For v1.3.67 (immediate fix)**:
- Use **Option 1 (Sequential Sending)**
- Simple, correct, fast to implement and test
- Sufficient for typical cluster sizes (< 10 nodes)

**For v2.x (future optimization)**:
- Consider **Option 2 (Per-Peer Channels)** if profiling shows bottleneck
- Only optimize if cluster size > 10 nodes AND latency is measurable issue
- **Do not premature optimize** - Option 1 works fine for most deployments

## Testing Verification

**Before fix**:
```bash
$ echo "test" | kafka-console-producer --bootstrap-server localhost:9092 --topic java-test
# Hangs indefinitely, must kill with Ctrl+C

$ grep "term=" node1.log | tail -5
term=4070, term=4075, term=4089, term=4094, term=4117  # Churning constantly
```

**After fix (expected)**:
```bash
$ echo "test" | kafka-console-producer --bootstrap-server localhost:9092 --topic java-test
>test
># Success! (no hang)

$ grep "term=" node1.log | tail -5
term=1, term=1, term=1, term=1, term=1  # Stable at term=1
```

## Files Involved

**Primary change**:
- `crates/chronik-server/src/raft_integration.rs:632-642` - Background loop message sending

**Related code** (review for similar issues):
- `crates/chronik-raft/src/replica.rs:709-760` - Message extraction from Ready
- `crates/chronik-raft/src/client.rs` - RaftClient::send_message() implementation

## Historical Context

**Why did we use `tokio::spawn()` originally?**
- Likely: Cargo-cult pattern from Tokio examples (spawn = async)
- Likely: Premature optimization ("parallel sending = faster!")
- **Missed**: Raft's strict ordering requirement wasn't documented in code

**Lesson Learned**:
> When implementing distributed consensus protocols, **message ordering is CRITICAL**.
> Always check protocol invariants before using async primitives like `tokio::spawn()`.
> If in doubt, send sequentially first - optimize later with benchmarks.

## Timeline

- **2025-10-23**: Metadata leader mismatch bug fixed (event-driven sync)
- **2025-10-24 09:00**: Java client testing begins, producer hangs discovered
- **2025-10-24 10:00**: Root cause investigation, massive election churn identified
- **2025-10-24 10:05**: **ROOT CAUSE FOUND** - unordered message delivery via `tokio::spawn()`
- **2025-10-24 10:10**: Fix document created (this file)
- **Next**: Implement Option 1 fix, test, release v1.3.67

## Related Issues

- [docs/testing/METADATA_LEADER_MISMATCH_ISSUE.md](../testing/METADATA_LEADER_MISMATCH_ISSUE.md) - FIXED (event callback)
- [docs/KNOWN_ISSUES_v2.0.0.md](../KNOWN_ISSUES_v2.0.0.md) - Java client hanging issue (THIS BUG)

## References

- **Raft Paper**: Section 5.2 "Leader Election" - strict term ordering required
- **tikv/raft README**: Messages should be sent in the order produced by `Ready`
- **Tokio Docs**: `tokio::spawn()` provides NO ordering guarantees
