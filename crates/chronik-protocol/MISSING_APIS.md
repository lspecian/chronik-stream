# Missing Kafka APIs in Chronik Stream

This document lists all Kafka APIs that are currently not implemented in Chronik Stream but are advertised in ApiVersions response for librdkafka compatibility.

## Status

These APIs are currently **NOT IMPLEMENTED** - they return version 0-0 in ApiVersions response purely for compatibility with librdkafka clients which expect a minimum set of APIs to be present.

## Broker Management APIs

### LeaderAndIsr (4)
- **Purpose**: Internal broker-to-broker API for leader election and ISR management
- **Used by**: Kafka brokers internally
- **Priority**: Low - not needed for client operations

### StopReplica (5)
- **Purpose**: Internal broker API to stop replica synchronization
- **Used by**: Kafka controller
- **Priority**: Low - internal broker operation

### UpdateMetadata (6)
- **Purpose**: Internal broker API to update metadata cache
- **Used by**: Kafka controller to propagate metadata
- **Priority**: Low - internal broker operation

### ControlledShutdown (7)
- **Purpose**: Graceful broker shutdown coordination
- **Used by**: Kafka brokers during shutdown
- **Priority**: Low - operational tool

## Transaction APIs

### InitProducerId (22)
- **Purpose**: Initialize a transactional producer with a producer ID
- **Used by**: Transactional producers
- **Priority**: High - needed for exactly-once semantics

### AddPartitionsToTxn (24)
- **Purpose**: Add partitions to an ongoing transaction
- **Used by**: Transactional producers
- **Priority**: High - core transaction functionality

### AddOffsetsToTxn (25)
- **Purpose**: Add consumer offsets to transaction for atomic consume-process-produce
- **Used by**: Transactional consumers/producers
- **Priority**: High - enables transactional patterns

### EndTxn (26)
- **Purpose**: Commit or abort a transaction
- **Used by**: Transactional producers
- **Priority**: High - core transaction functionality

### WriteTxnMarkers (27)
- **Purpose**: Internal API to write transaction markers to partitions
- **Used by**: Transaction coordinator
- **Priority**: Medium - internal transaction mechanism

### TxnOffsetCommit (28)
- **Purpose**: Commit offsets as part of a transaction
- **Used by**: Transactional consumers
- **Priority**: High - transactional offset management

### DescribeTransactions (65)
- **Purpose**: Get information about ongoing transactions
- **Used by**: Admin tools, monitoring
- **Priority**: Medium - operational visibility

### ListTransactions (66)
- **Purpose**: List all transactions in the cluster
- **Used by**: Admin tools, monitoring
- **Priority**: Medium - operational visibility

### AllocateProducerIds (67)
- **Purpose**: Allocate a block of producer IDs
- **Used by**: Brokers internally
- **Priority**: Low - internal mechanism

## ACL (Access Control List) APIs

### DescribeAcls (29)
- **Purpose**: List ACLs matching filters
- **Used by**: Admin tools, security management
- **Priority**: Medium - security feature

### CreateAcls (30)
- **Purpose**: Create new ACL entries
- **Used by**: Admin tools, security management
- **Priority**: Medium - security feature

### DeleteAcls (31)
- **Purpose**: Delete ACL entries
- **Used by**: Admin tools, security management
- **Priority**: Medium - security feature

## Admin & Configuration APIs

### DeleteRecords (21)
- **Purpose**: Delete records before a given offset
- **Used by**: Admin tools for data cleanup
- **Priority**: Medium - data management

### OffsetForLeaderEpoch (23)
- **Purpose**: Get offset for a specific leader epoch
- **Used by**: Consumers for exact offset management
- **Priority**: Medium - advanced consumer feature

### AlterReplicaLogDirs (34)
- **Purpose**: Move partition replicas between log directories
- **Used by**: Admin tools for storage management
- **Priority**: Low - operational tool

### DescribeLogDirs (35)
- **Purpose**: Get information about broker log directories
- **Used by**: Admin tools, monitoring
- **Priority**: Low - operational visibility

### CreatePartitions (37)
- **Purpose**: Add partitions to existing topics
- **Used by**: Admin tools
- **Priority**: Medium - topic management

### DeleteGroups (42)
- **Purpose**: Delete consumer groups
- **Used by**: Admin tools
- **Priority**: Medium - group management

### ElectLeaders (43)
- **Purpose**: Trigger leader election for partitions
- **Used by**: Admin tools for operational tasks
- **Priority**: Low - operational tool

### IncrementalAlterConfigs (44)
- **Purpose**: Incrementally update broker/topic configurations
- **Used by**: Admin tools
- **Priority**: Medium - configuration management

### AlterPartitionReassignments (45)
- **Purpose**: Start partition reassignment
- **Used by**: Admin tools for rebalancing
- **Priority**: Low - operational tool

### ListPartitionReassignments (46)
- **Purpose**: List ongoing partition reassignments
- **Used by**: Admin tools, monitoring
- **Priority**: Low - operational visibility

### OffsetDelete (47)
- **Purpose**: Delete consumer group offsets
- **Used by**: Admin tools
- **Priority**: Medium - offset management

## Security APIs

### SaslAuthenticate (36)
- **Purpose**: SASL authentication handshake
- **Used by**: Clients during authentication
- **Priority**: High - required for SASL auth

### CreateDelegationToken (38)
- **Purpose**: Create delegation tokens for authentication
- **Used by**: Security tools
- **Priority**: Low - advanced security feature

### RenewDelegationToken (39)
- **Purpose**: Renew expiring delegation tokens
- **Used by**: Security tools
- **Priority**: Low - advanced security feature

### ExpireDelegationToken (40)
- **Purpose**: Expire delegation tokens
- **Used by**: Security tools
- **Priority**: Low - advanced security feature

### DescribeDelegationToken (41)
- **Purpose**: Get information about delegation tokens
- **Used by**: Security tools
- **Priority**: Low - advanced security feature

### DescribeUserScramCredentials (50)
- **Purpose**: Get SCRAM credentials information
- **Used by**: Security management tools
- **Priority**: Low - SCRAM authentication

### AlterUserScramCredentials (51)
- **Purpose**: Update SCRAM credentials
- **Used by**: Security management tools
- **Priority**: Low - SCRAM authentication

## Quota Management APIs

### DescribeClientQuotas (48)
- **Purpose**: Get client quota configurations
- **Used by**: Admin tools
- **Priority**: Low - quota management

### AlterClientQuotas (49)
- **Purpose**: Update client quota configurations
- **Used by**: Admin tools
- **Priority**: Low - quota management

## KRaft Consensus APIs

### Vote (52)
- **Purpose**: KRaft consensus voting
- **Used by**: KRaft controllers
- **Priority**: Low - KRaft-specific

### BeginQuorumEpoch (53)
- **Purpose**: Start a new quorum epoch
- **Used by**: KRaft controllers
- **Priority**: Low - KRaft-specific

### EndQuorumEpoch (54)
- **Purpose**: End current quorum epoch
- **Used by**: KRaft controllers
- **Priority**: Low - KRaft-specific

### DescribeQuorum (55)
- **Purpose**: Get quorum status
- **Used by**: KRaft monitoring
- **Priority**: Low - KRaft-specific

### AlterPartition (56)
- **Purpose**: Update partition metadata in KRaft mode
- **Used by**: KRaft brokers
- **Priority**: Low - KRaft-specific

### UpdateFeatures (57)
- **Purpose**: Update cluster feature flags
- **Used by**: Admin tools
- **Priority**: Low - advanced feature

### Envelope (58)
- **Purpose**: Wrap requests for forwarding
- **Used by**: Brokers for request forwarding
- **Priority**: Low - internal mechanism

### FetchSnapshot (59)
- **Purpose**: Fetch metadata snapshots in KRaft
- **Used by**: KRaft brokers
- **Priority**: Low - KRaft-specific

### DescribeCluster (60)
- **Purpose**: Get cluster metadata and controller info
- **Used by**: Admin tools, monitoring
- **Priority**: Medium - cluster visibility

### DescribeProducers (61)
- **Purpose**: Get active producer information for partitions
- **Used by**: Admin tools, debugging
- **Priority**: Low - debugging tool

### BrokerRegistration (62)
- **Purpose**: Register broker with KRaft controller
- **Used by**: KRaft brokers
- **Priority**: Low - KRaft-specific

### BrokerHeartbeat (63)
- **Purpose**: Broker heartbeat to KRaft controller
- **Used by**: KRaft brokers
- **Priority**: Low - KRaft-specific

### UnregisterBroker (64)
- **Purpose**: Unregister broker from KRaft controller
- **Used by**: KRaft brokers
- **Priority**: Low - KRaft-specific

## Consumer Group Coordination v2

### ConsumerGroupHeartbeat (68)
- **Purpose**: New consumer group protocol heartbeat
- **Used by**: New consumer protocol (KIP-848)
- **Priority**: Medium - future consumer protocol

## Implementation Priority

### High Priority (consider implementing)
1. **Transaction APIs** - Critical for exactly-once semantics
2. **SaslAuthenticate** - Required for SASL authentication

### Medium Priority (nice to have)
1. **Admin APIs** - Useful for topic/group management
2. **ACL APIs** - Important for security
3. **DescribeCluster** - Useful for monitoring
4. **ConsumerGroupHeartbeat** - Future consumer protocol

### Low Priority (not needed for core functionality)
1. **KRaft APIs** - Only needed for KRaft mode
2. **Internal Broker APIs** - Not used by clients
3. **Delegation Token APIs** - Advanced security feature
4. **Quota APIs** - Advanced management feature

## Notes

- These APIs are advertised with version 0-0 to satisfy librdkafka's expectations
- Actual implementation would require significant work for each API
- Most client applications only use the core APIs that are already implemented
- Priority should be given to transaction APIs if exactly-once semantics are needed