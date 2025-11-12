# Raft Leader Election Deadlock Fix (v2.5.1)

## Problem

**Symptoms**:
- Raft cluster continuously tries to elect leader but never succeeds
- Term numbers increment rapidly (157 → 181+ in seconds)
- Nodes continuously become candidates and broadcast vote requests
- Vote responses are never received by requesting nodes
- Error logs show: `gRPC error: status: Cancelled, message: "Timeout expired"`

**Root Cause**:
The Raft gRPC message handler (`RaftServiceImpl`) attempts to acquire the `raft_node` RwLock to call `raft.step(msg)`. However, the Raft ready loop already holds this lock for extended periods (during tick processing, ready handling, etc.). This creates a blocking scenario:

1. Raft ready loop acquires `raft_node.write()` lock
2. gRPC handler receives vote response from peer
3. gRPC handler blocks trying to acquire same `raft_node.write()` lock
4. gRPC request times out after 10 seconds (`timeout` in `grpc.rs:105`)
5. Vote response is lost, election fails
6. Node times out, increments term, starts new election
7. Cycle repeats infinitely

**Code Location**:
- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/raft_cluster.rs:1933-1942`
- Message handler directly calls `raft.step(msg)` with blocking lock acquisition

## Solution

Implement **channel-based message queueing** to decouple gRPC message reception from Raft processing:

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Before (Deadlock)                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  gRPC Handler                     Raft Ready Loop                │
│  ┌──────────┐                     ┌───────────┐                 │
│  │ Receive  │                     │ Tick      │                 │
│  │ Message  │──X──blocking───X──→ │ (holds    │                 │
│  │          │    (10s timeout)    │  lock)    │                 │
│  └──────────┘                     └───────────┘                 │
│                                                                   │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                    After (Fixed)                                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  gRPC Handler          Channel            Raft Ready Loop       │
│  ┌──────────┐         ┌───────┐          ┌───────────┐         │
│  │ Receive  │──fast──→│ Queue │──async──→│ Process   │         │
│  │ Message  │ (<1ms)  │       │          │ Messages  │         │
│  └──────────┘         └───────┘          └───────────┘         │
│                                                                   │
│  Non-blocking send    Unbounded          Batch process          │
│  Returns immediately  No backpressure    Inside Raft loop       │
│                                                                   │
└─────────────────────────────────────────────────────────────────┘
```

### Implementation Details

**Step 1**: Add incoming message channel to `RaftCluster` struct
```rust
/// Channel for receiving incoming Raft messages from gRPC server
/// Messages are queued here by gRPC handler (non-blocking)
/// and processed by Raft ready loop (inside lock)
incoming_message_receiver: Arc<Mutex<mpsc::UnboundedReceiver<Message>>>,
```

**Step 2**: Create channel during bootstrap
```rust
// Create channel for incoming Raft messages from gRPC
let (incoming_sender, incoming_receiver) = mpsc::unbounded_channel::<Message>();
```

**Step 3**: Modify gRPC handler to queue instead of blocking
```rust
// OLD (blocking):
let mut raft = cluster.raft_node.write()
    .map_err(|e| format!("Failed to acquire Raft lock: {}", e))?;
raft.step(msg)
    .map_err(|e| format!("Failed to step Raft: {:?}", e))?;

// NEW (non-blocking):
cluster.incoming_sender.send(msg)
    .map_err(|e| format!("Failed to queue message: {}", e))?;
```

**Step 4**: Process queued messages in Raft ready loop
```rust
// Process all queued incoming messages
while let Ok(msg) = incoming_receiver.try_recv() {
    if let Err(e) = raft.step(msg) {
        tracing::error!("Failed to step Raft with queued message: {:?}", e);
    }
}
```

### Benefits

1. **No Deadlock**: gRPC handler never blocks waiting for Raft lock
2. **Fast Response**: gRPC requests complete in <1ms (just queue operation)
3. **Batching**: Multiple messages can be queued and processed together
4. **Backpressure**: Unbounded channel ensures no message loss during election storms
5. **Standard Pattern**: This is the recommended pattern for Raft implementations

### Testing

After applying fix:
1. Restart cluster: `./tests/cluster/stop.sh && ./tests/cluster/start.sh`
2. Watch logs: `tail -f tests/cluster/logs/node*.log | grep "became leader"`
3. Expected: Leader elected within ~1 second, term stabilizes
4. Verify: No "MESSAGE LOST" or "Timeout expired" errors

## Version

Fixed in v2.5.1 (2025-11-12)
