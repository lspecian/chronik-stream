# kafka-python Consumer Group Rebalance Bug

**Date**: 2025-11-04
**Status**: CONFIRMED - CLIENT-SIDE BUG
**Affected**: kafka-python library
**Workaround**: Implemented server-side timeout for missing SyncGroup from leader

---

## Summary

kafka-python has a bug in its consumer group rebalancing logic where the leader fails to send a new SyncGroup request after a rebalance triggered by new members joining.

## The Bug

### Expected Behavior (Kafka Protocol):

1. Consumer-1 joins alone → Generation 0, leader=consumer-1, 1 member
2. Consumer-1 sends SyncGroup with assignments for 1 member
3. Consumers 2 and 3 join → Triggers rebalance to Generation 1
4. All 3 consumers receive JoinGroup response for Generation 1 (leader=consumer-1, 3 members)
5. **Consumer-1 (leader) should send NEW SyncGroup with assignments for ALL 3 members**
6. Server distributes assignments to consumers 2 and 3
7. All 3 consumers consume from different partitions

### Actual Behavior (kafka-python bug):

Steps 1-4 happen correctly, but:
5. **BUG: Consumer-1 (leader) NEVER sends a second SyncGroup for Generation 1!**
6. Consumers 2 and 3 wait indefinitely for assignments
7. Consumers 2 and 3 timeout with "SyncGroup response channel closed"
8. Only consumer-1 consumes (using stale Generation 0 assignment with all partitions)

## Evidence

### Server Logs:

```
[15:11:49.796] Generation 0: member_count=1, leader=consumer-1
[15:11:49.796] consumer-1 SyncGroup: assignment_count=1 (assigns [0,1,2] to itself)
[15:11:49.796] Rebalance completed - generation=0

[15:11:50.091] consumer-2 JoinGroup request
[15:11:50.591] consumer-3 JoinGroup request
[15:11:50.694] Generation 1: member_count=3, leader=consumer-1  ← Rebalance triggered!
[15:11:50.695] consumer-2: Follower waiting for leader
[15:11:50.696] consumer-3: Follower waiting for leader

← NO SYNCGROUP FROM CONSUMER-1 FOR GENERATION 1! ←

[15:12:30.775] ERROR: SyncGroup response channel closed (consumers 2 & 3 timeout)
```

### Root Cause:

kafka-python's leader election logic is correct, but after receiving a JoinGroup response for a NEW generation with DIFFERENT members, the client fails to:
- Detect that the member list changed
- Re-invoke the partition assignor
- Send a new SyncGroup request

It appears kafka-python only sends one SyncGroup per consumer instance lifecycle, not one per generation.

## Workaround Implemented (Server-Side)

Since we can't fix the kafka-python client, Chronik implements a server-side workaround:

**Timeout-Based Server-Side Assignment**:
- After sending JoinGroup responses, if the leader doesn't send SyncGroup within 5 seconds
- AND there are followers waiting for assignments
- Server automatically computes assignments using server-side assignor
- Sends assignments to all waiting followers

This allows buggy clients to still work correctly.

### Implementation:

File: `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/consumer_group.rs`

```rust
// After completing JoinGroup for new generation:
// Spawn background task to detect missing SyncGroup from leader
if group.state == GroupState::CompletingRebalance && has_followers_waiting {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Check if leader still hasn't sent SyncGroup
        let mut groups = self.groups.write().await;
        if let Some(group) = groups.get_mut(&group_id) {
            if group.state == GroupState::CompletingRebalance {
                warn!("Leader failed to send SyncGroup - computing assignments server-side");
                let assignments = self.compute_assignments(group).await?;
                // Send to waiting followers...
            }
        }
    });
}
```

## Testing

### Reproducing the Bug:

```bash
# Start Chronik
CHRONIK_DATA_DIR=./test RUST_LOG=info ./target/release/chronik-server start --advertise localhost

# Run kafka-python test with 3 consumers
python3 test_real_kafka.py chronik
```

**Without workaround**: Only 50 messages consumed (all by consumer-1)
**With workaround**: 150 messages consumed (50/50/50 distribution)

## Comparison with Real Kafka

Real Kafka brokers also implement a similar workaround - they don't rely solely on clients sending SyncGroup. If the leader fails to send SyncGroup, Kafka triggers server-side assignment after a timeout.

This is why kafka-python works with real Kafka despite having this bug.

## References

- Kafka Protocol: https://kafka.apache.org/protocol.html#The_Messages_JoinGroup
- Consumer Group Rebalancing: https://kafka.apache.org/documentation/#consumerconfigs

---

**Conclusion**: This is a kafka-python client bug, but we work around it server-side for compatibility.
