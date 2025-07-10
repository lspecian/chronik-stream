# Kafka Protocol - Unimplemented APIs Report

## Overview
This report lists all Kafka APIs that are not implemented or are returning stub/error responses in the chronik-protocol handler.

## API Implementation Status

### ✅ Implemented APIs (11)
1. **ApiVersions** (API key: 18) - Fully implemented for versions 0-3
2. **Metadata** (API key: 3) - Fully implemented for versions 0-12
3. **Produce** (API key: 0) - Basic implementation for versions 0-9
4. **Fetch** (API key: 1) - Basic implementation for versions 0-13
5. **DescribeConfigs** (API key: 32) - Fully implemented for versions 0-4
6. **CreateTopics** (API key: 19) - Basic implementation for versions 0-5
7. **ListOffsets** (API key: 2) - Basic implementation for versions 0-4
8. **FindCoordinator** (API key: 10) - Basic implementation for versions 0-4
9. **JoinGroup** (API key: 11) - Basic implementation for versions 0-7
10. **SyncGroup** (API key: 14) - Basic implementation for versions 0-5
11. **Heartbeat** (API key: 12) - Basic implementation for versions 0-4

### ⚠️ Partially Implemented APIs (1)
1. **SaslHandshake** (API key: 17) - Returns authentication failed error (no SASL support)

### ❌ Unimplemented APIs (21)

#### Consumer Group APIs (5)
These APIs return "unimplemented" errors:
1. **OffsetCommit** (API key: 8) - For committing consumer offsets
2. **OffsetFetch** (API key: 9) - For fetching consumer offsets
3. **LeaveGroup** (API key: 13) - For leaving consumer groups
4. **DescribeGroups** (API key: 15) - For describing consumer groups
5. **ListGroups** (API key: 16) - For listing consumer groups

#### Administrative APIs (2)
These APIs return "unimplemented" errors:
1. **DeleteTopics** (API key: 20) - For deleting topics
2. **AlterConfigs** (API key: 33) - For altering configurations

#### Broker-to-Broker APIs (4)
These APIs return UNSUPPORTED_VERSION error:
1. **LeaderAndIsr** (API key: 4)
2. **StopReplica** (API key: 5)
3. **UpdateMetadata** (API key: 6)
4. **ControlledShutdown** (API key: 7)

#### Transaction APIs (6)
These APIs return UNSUPPORTED_VERSION error:
1. **InitProducerId** (API key: 22)
2. **AddPartitionsToTxn** (API key: 24)
3. **AddOffsetsToTxn** (API key: 25)
4. **EndTxn** (API key: 26)
5. **WriteTxnMarkers** (API key: 27)
6. **TxnOffsetCommit** (API key: 28)

#### ACL APIs (3)
These APIs return UNSUPPORTED_VERSION error:
1. **DescribeAcls** (API key: 29)
2. **CreateAcls** (API key: 30)
3. **DeleteAcls** (API key: 31)

#### Other APIs (2)
These APIs return UNSUPPORTED_VERSION error:
1. **DeleteRecords** (API key: 21) - Returns UNSUPPORTED_VERSION error
2. **OffsetForLeaderEpoch** (API key: 23) - Returns UNSUPPORTED_VERSION error

## Missing Newer Kafka APIs
The current implementation only supports APIs up to key 33 (AlterConfigs). Newer Kafka versions have additional APIs that are not even defined in the ApiKey enum:

- API keys 34+ are not recognized and would return an "Unknown API key" error

Some notable missing newer APIs include:
- AlterReplicaLogDirs (34)
- DescribeLogDirs (35)
- SaslAuthenticate (36)
- CreatePartitions (37)
- CreateDelegationToken (38)
- RenewDelegationToken (39)
- ExpireDelegationToken (40)
- DescribeDelegationToken (41)
- DeleteGroups (42)
- ElectLeaders (43)
- IncrementalAlterConfigs (44)
- AlterPartitionReassignments (45)
- ListPartitionReassignments (46)
- OffsetDelete (47)
- DescribeClientQuotas (48)
- AlterClientQuotas (49)
- DescribeUserScramCredentials (50)
- AlterUserScramCredentials (51)
- Vote (52)
- BeginQuorumEpoch (53)
- EndQuorumEpoch (54)
- DescribeQuorum (55)
- AlterPartition (56)
- UpdateFeatures (57)
- Envelope (58)
- FetchSnapshot (59)
- DescribeCluster (60)
- DescribeProducers (61)
- BrokerRegistration (62)
- BrokerHeartbeat (63)
- UnregisterBroker (64)
- DescribeTransactions (65)
- ListTransactions (66)
- AllocateProducerIds (67)
- ConsumerGroupHeartbeat (68)

## Priority Implementation Recommendations

### High Priority (Essential for basic Kafka compatibility)
1. **Consumer Group APIs** - Required for consumer functionality:
   - FindCoordinator
   - JoinGroup
   - SyncGroup
   - Heartbeat
   - OffsetCommit
   - OffsetFetch

2. **ListOffsets** - Required for consumers to find starting positions

3. **Administrative APIs** - For topic management:
   - CreateTopics
   - DeleteTopics

### Medium Priority (Common use cases)
1. **Consumer Group Management**:
   - LeaveGroup
   - DescribeGroups
   - ListGroups

2. **Configuration Management**:
   - AlterConfigs

### Low Priority (Advanced features)
1. **Transaction APIs** - Only needed for exactly-once semantics
2. **ACL APIs** - Only needed for security features
3. **Broker-to-Broker APIs** - Not needed for client compatibility

## Implementation Notes
- All unimplemented APIs currently return error code 35 (UNSUPPORTED_VERSION)
- The handler properly checks version ranges from `supported_api_versions()` before routing
- The implementation uses a match statement in `handle_request()` to route to specific handlers
- Each API that returns "unimplemented" logs an info message with the API name