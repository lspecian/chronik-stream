# Consumer Group Coordinator Implementation

## Overview

Task #11 implements a distributed consumer group coordination system for Chronik Stream using the controller service with Raft consensus. This replaces the local implementation with a fault-tolerant, distributed approach that ensures all group state is managed through consensus.

## Architecture

### Components

1. **Controller-side Group Coordinator** (`crates/chronik-controller/src/group_coordinator.rs`)
   - Manages consumer groups through Raft consensus
   - Handles join/sync/heartbeat/leave operations
   - Manages offset commits and fetches
   - Integrates with controller state machine

2. **gRPC Service** (`crates/chronik-controller/src/grpc_service.rs`)
   - Exposes group coordination operations via gRPC
   - Bridges requests to the group coordinator
   - Handles protocol conversions

3. **Controller Client** (`crates/chronik-ingest/src/controller_client.rs`)
   - Client library for ingest nodes
   - Wraps gRPC calls with error handling
   - Manages connection to controller

4. **Controller-based Group Manager** (`crates/chronik-ingest/src/controller_group_manager.rs`)
   - Drop-in replacement for local GroupManager
   - Delegates all operations to controller via gRPC
   - Maintains same interface for compatibility

### Key Design Decisions

1. **Backward Compatibility**: The implementation supports both local and controller-based coordination through the `use_controller_groups` configuration flag.

2. **Protocol Compliance**: Maintains full Kafka protocol compatibility including KIP-848 incremental rebalance support.

3. **State Management**: All group state is managed through Raft consensus, ensuring fault tolerance and consistency.

4. **Abstraction Layer**: The `GroupManagerImpl` enum in the handler allows seamless switching between local and controller-based implementations.

## Implementation Details

### Proto Definitions

```protobuf
service ControllerService {
    rpc JoinGroup(JoinGroupRequest) returns (JoinGroupResponse);
    rpc SyncGroup(SyncGroupRequest) returns (SyncGroupResponse);
    rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
    rpc LeaveGroup(LeaveGroupRequest) returns (LeaveGroupResponse);
    rpc CommitOffsets(CommitOffsetsRequest) returns (CommitOffsetsResponse);
    rpc FetchOffsets(FetchOffsetsRequest) returns (FetchOffsetsResponse);
    rpc DescribeGroup(DescribeGroupRequest) returns (DescribeGroupResponse);
    rpc ListGroups(ListGroupsRequest) returns (ListGroupsResponse);
}
```

### Configuration

In `HandlerConfig`:
```rust
pub struct HandlerConfig {
    pub node_id: i32,
    pub host: String,
    pub port: i32,
    pub controller_addrs: Vec<String>,
    pub use_controller_groups: bool,  // Enable controller-based coordination
}
```

### Group Manager Selection

```rust
let group_manager = if config.use_controller_groups {
    // Use controller-based group coordination
    let controller_manager = ControllerGroupManager::new(
        controller_addr,
        metadata_store.clone(),
    ).await?;
    GroupManagerImpl::Controller(Arc::new(controller_manager))
} else {
    // Use local group coordination
    GroupManagerImpl::Local(Arc::new(GroupManager::new(metadata_store.clone())))
};
```

## Testing Strategy

### Unit Tests

1. **Group Coordinator Tests** (`tests/group_coordinator_test.rs`)
   - Test group lifecycle (join, sync, heartbeat, leave)
   - Test multiple member coordination
   - Test offset management
   - Test rebalance scenarios
   - Test state persistence

2. **Controller Group Manager Tests** (`tests/controller_group_manager_test.rs`)
   - Test gRPC client functionality
   - Test error handling and retries
   - Test concurrent operations

3. **Integration Tests** (`tests/handler_group_integration_test.rs`)
   - Test handler with both local and controller modes
   - Test protocol compliance
   - Test state transitions

### Test Coverage

- Group creation and management
- Member join/leave operations
- Rebalance coordination
- Offset commit and fetch
- Error conditions and recovery
- Concurrent operations
- State persistence across restarts

## Future Enhancements

1. **KIP-848 Full Support**: Complete implementation of incremental cooperative rebalancing with member epochs and owned partitions.

2. **Performance Optimizations**: 
   - Connection pooling for controller clients
   - Batch offset commits
   - Caching of group metadata

3. **Monitoring**: 
   - Metrics for group operations
   - Rebalance metrics
   - Lag monitoring

4. **Security**: 
   - TLS support for controller connections
   - ACL integration for group operations

## Usage

### Enable Controller-based Coordination

```bash
# In docker-compose.yml or environment
INGEST_USE_CONTROLLER_GROUPS=true
INGEST_CONTROLLER_ADDRS=controller:9090
```

### Verify Operation

```bash
# Check if using controller coordination
curl http://localhost:9092/status | jq .group_coordination

# Monitor group operations
docker logs chronik-controller | grep "group:"
```

## Conclusion

This implementation provides a robust, distributed consumer group coordination system that maintains Kafka protocol compatibility while leveraging Raft consensus for fault tolerance. The design allows for easy migration from local to distributed coordination without changing client code.