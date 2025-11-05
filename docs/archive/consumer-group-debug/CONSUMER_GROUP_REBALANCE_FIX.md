# Consumer Group Rebalancing Fix - Implementation Plan

## Problem Summary

**Root Cause**: The server immediately returns JoinGroup responses to each consumer as they arrive, rather than waiting for ALL consumers to join before completing the join phase. This violates the Kafka consumer group protocol and causes only one consumer to get partitions.

**Symptoms**:
- 3 consumers start in same group
- Only Consumer 1 (first to join) consumes messages
- Consumers 2 & 3 receive 0 messages
- Server logs show Consumer 1 rapidly rejoining with empty assignments

**Technical Details**:
1. Consumer 1 joins → Server gives immediate response (generation=0, leader=true) ❌
2. Consumer 1 calls SyncGroup → Computes assignments for only 1 member ❌
3. Consumers 2 & 3 join → Server gives immediate response (generation=1) ❌
4. Group transitions to Stable before all members sync ❌
5. Cascade of rebalances, none ever complete properly ❌

## Solution Design

### The Kafka Protocol Requirement

**Correct Flow**:
1. Consumers join → Server puts them in `PreparingRebalance` state
2. Server **BLOCKS** JoinGroup responses until ALL expected members join
3. When quorum reached, server sends JoinGroup responses to **everyone simultaneously**
4. Leader is told "you're the leader + here are all members"
5. Followers are told "you're a follower + wait for SyncGroup"
6. Leader computes assignments and calls SyncGroup
7. Server distributes assignments via SyncGroup responses
8. Group transitions to `Stable` state

### Implementation Architecture

We need to add async waiting using `tokio::sync::oneshot` channels to hold JoinGroup responses until it's safe to return them.

## Implementation Steps

### Step 1: Add Waiting Mechanism to ConsumerGroup

**File**: `crates/chronik-server/src/consumer_group.rs`

Add to ConsumerGroup struct (around line 150):

```rust
use tokio::sync::oneshot;

#[derive(Debug, Serialize, Deserialize)]
pub struct ConsumerGroup {
    // ... existing fields ...

    // NEW: Async waiting mechanism for JoinGroup responses
    #[serde(skip)]
    pub pending_join_futures: Arc<Mutex<HashMap<String, oneshot::Sender<JoinGroupResponse>>>>,
}
```

Update `ConsumerGroup::new()` (around line 158):

```rust
pub fn new(group_id: String, protocol_type: String) -> Self {
    Self {
        // ... existing fields ...
        pending_join_futures: Arc::new(Mutex::new(HashMap::new())),
    }
}
```

### Step 2: Add Helper Methods to ConsumerGroup

Add these methods to ConsumerGroup impl block:

```rust
/// Check if all expected members have joined for this generation
pub fn all_members_joined(&self) -> bool {
    // During PreparingRebalance, we expect members to keep arriving
    // We consider "all joined" when no new members join for rebalance_timeout
    // OR when we've seen at least session_timeout since rebalance start

    if self.state != GroupState::PreparingRebalance {
        return false;
    }

    // Simple heuristic: if no pending_members (all have been added), we're ready
    // This works because pending_members tracks members who haven't synced yet
    !self.members.is_empty() && self.pending_members.is_empty()
}

/// Complete the join phase - send responses to all waiting members
pub async fn complete_join_phase(&mut self, pending_futures: &mut HashMap<String, oneshot::Sender<JoinGroupResponse>>) {
    if self.members.is_empty() {
        return;
    }

    // Select leader (first member)
    let leader_id = self.members.keys().next().unwrap().clone();
    self.leader_id = Some(leader_id.clone());

    // Transition to CompletingRebalance
    self.state = GroupState::CompletingRebalance;

    info!(
        group_id = %self.group_id,
        generation = self.generation_id,
        member_count = self.members.len(),
        leader = %leader_id,
        "Completing join phase - sending responses to all members"
    );

    // Send responses to all waiting members
    for (member_id, member) in &self.members {
        let is_leader = member_id == &leader_id;

        // Build member list for leader
        let members = if is_leader {
            self.members.iter().map(|(id, m)| GroupMemberMetadata {
                member_id: id.clone(),
                client_id: m.client_id.clone(),
                client_host: m.client_host.clone(),
                metadata: vec![], // Protocol metadata
            }).collect()
        } else {
            vec![]
        };

        let response = JoinGroupResponse {
            error_code: 0,
            generation_id: self.generation_id,
            protocol_type: Some(self.protocol_type.clone()),
            protocol_name: self.protocol.clone(),
            leader_id: leader_id.clone(),
            member_id: member_id.clone(),
            members,
        };

        // Send response via oneshot channel
        if let Some(tx) = pending_futures.remove(member_id) {
            if tx.send(response).is_err() {
                warn!(member_id = %member_id, "Failed to send JoinGroup response - receiver dropped");
            }
        }
    }
}
```

### Step 3: Modify GroupManager::join_group

**File**: `crates/chronik-server/src/consumer_group.rs` (around line 555)

Replace the `join_group` method implementation:

```rust
pub async fn join_group(
    &self,
    group_id: String,
    member_id: Option<String>,
    client_id: String,
    client_host: String,
    session_timeout: Duration,
    rebalance_timeout: Duration,
    protocol_type: String,
    protocols: Vec<(String, Vec<u8>)>,
    static_member_id: Option<String>,
) -> Result<JoinGroupResponse> {
    // Ensure group exists
    self.get_or_create_group(group_id.clone(), protocol_type.clone()).await?;

    let mut groups = self.groups.write().await;
    let group = groups.get_mut(&group_id)
        .ok_or_else(|| Error::Internal("Group not found after creation".into()))?;

    // Handle member ID generation (existing logic)
    let member_id = if let Some(static_id) = &static_member_id {
        if let Some(existing_member_id) = group.static_members.get(static_id) {
            existing_member_id.clone()
        } else {
            let new_member_id = member_id.unwrap_or_else(|| {
                format!("{}-{}", client_id, uuid::Uuid::new_v4())
            });
            group.static_members.insert(static_id.clone(), new_member_id.clone());
            new_member_id
        }
    } else {
        member_id.unwrap_or_else(|| {
            format!("{}-{}", client_id, uuid::Uuid::new_v4())
        })
    };

    // Check if this is a rejoin
    let is_rejoin = group.members.contains_key(&member_id);

    // Parse subscription
    let subscription = if let Some((_, metadata)) = protocols.first() {
        parse_subscription_metadata(metadata)?
    } else {
        vec![]
    };

    // Create or update member
    let mut member = GroupMember::new(
        member_id.clone(),
        client_id.clone(),
        client_host,
        session_timeout,
        rebalance_timeout,
        subscription,
        protocols.clone(),
    );

    // Preserve owned partitions for incremental rebalance
    if is_rejoin && group.assignment_strategy == AssignmentStrategy::CooperativeSticky {
        if let Some(existing) = group.members.get(&member_id) {
            member.owned_partitions = existing.owned_partitions.clone();
        }
    }

    group.add_member(member);

    // **KEY FIX**: Don't return immediately during PreparingRebalance!
    // Instead, wait for all members to join

    if group.state == GroupState::PreparingRebalance || group.state == GroupState::Empty {
        // Create a oneshot channel for this member's response
        let (tx, rx) = oneshot::channel();

        {
            let mut pending_futures = group.pending_join_futures.lock().await;
            pending_futures.insert(member_id.clone(), tx);
        }

        // Check if all expected members have joined
        if group.all_members_joined() {
            // Complete join phase - send responses to everyone
            let mut pending_futures = group.pending_join_futures.lock().await;
            group.complete_join_phase(&mut *pending_futures).await;
        }

        // Drop the write lock before awaiting
        drop(groups);

        // Wait for response (will be sent by complete_join_phase)
        match tokio::time::timeout(rebalance_timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(Error::Internal("JoinGroup response channel closed".into())),
            Err(_) => Err(Error::Internal("JoinGroup timeout waiting for rebalance".into())),
        }
    } else if group.state == GroupState::Stable {
        // Group is stable - rejoin triggers rebalance
        if !is_rejoin {
            group.trigger_rebalance();
        }

        // Return immediate response for stable group
        let is_leader = group.leader_id.as_ref() == Some(&member_id);

        Ok(JoinGroupResponse {
            error_code: 0,
            generation_id: group.generation_id,
            protocol_type: Some(group.protocol_type.clone()),
            protocol_name: group.protocol.clone(),
            leader_id: group.leader_id.clone().unwrap_or_default(),
            member_id,
            members: if is_leader {
                group.members.iter().map(|(id, m)| GroupMemberMetadata {
                    member_id: id.clone(),
                    client_id: m.client_id.clone(),
                    client_host: m.client_host.clone(),
                    metadata: vec![],
                }).collect()
            } else {
                vec![]
            },
        })
    } else {
        Err(Error::InvalidInput(format!("Invalid group state: {:?}", group.state)))
    }
}
```

### Step 4: Add Required Types

Add to top of file or in appropriate location:

```rust
#[derive(Debug, Clone)]
pub struct GroupMemberMetadata {
    pub member_id: String,
    pub client_id: String,
    pub client_host: String,
    pub metadata: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct JoinGroupResponse {
    pub error_code: i16,
    pub generation_id: i32,
    pub protocol_type: Option<String>,
    pub protocol_name: Option<String>,
    pub leader_id: String,
    pub member_id: String,
    pub members: Vec<GroupMemberMetadata>,
}
```

## Testing Plan

1. **Unit Test**: Test `all_members_joined()` logic
2. **Unit Test**: Test `complete_join_phase()` broadcasts responses correctly
3. **Integration Test**: Start 3 consumers, verify all get partition assignments
4. **Integration Test**: Start consumers with staggered timing (0s, 2s, 4s delays)
5. **Stress Test**: 10 consumers joining simultaneously

## Known Limitations

This implementation uses a simple heuristic for "all members joined":
- Checks if `pending_members` is empty

**Future Improvements**:
- Add configurable member count expectation
- Add delay-based detection (no new members for N seconds)
- Add admin API to manually trigger rebalance completion

## References

- Kafka Protocol: https://kafka.apache.org/protocol#The_Messages_JoinGroup
- KIP-848: https://cwiki.apache.org/confluence/display/KAFKA/KIP-848
- Current implementation: [crates/chronik-server/src/consumer_group.rs](../crates/chronik-server/src/consumer_group.rs)
