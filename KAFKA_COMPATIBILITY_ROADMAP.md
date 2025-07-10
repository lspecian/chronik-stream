# Kafka Protocol Compatibility Roadmap for Chronik-Stream

## Executive Summary

This roadmap outlines the path to achieving full Kafka protocol compatibility for chronik-stream, enabling seamless integration with existing Kafka clients including kafkactl, standard Kafka producers/consumers, and ecosystem tools.

## Current State Analysis

### What Works
- **Basic Protocol Structure**: Request/response framing with length prefixes
- **Core Protocol Types**: Encoding/decoding of primitives (int8-64, strings, arrays, varints)
- **Partial API Support**:
  - ApiVersions (v0-3) - Basic implementation exists
  - Metadata (v0-12) - Returns empty topic list
  - Produce (v0-9) - Accepts messages but no persistence
  - Fetch (v0-13) - Returns empty responses

### Critical Issues
1. **Protocol Encoding Bugs**:
   - ApiVersions response has incorrect field ordering (throttle_time placement)
   - Missing correlation ID in some responses
   - Frame size calculation errors

2. **Missing Essential APIs**:
   - DescribeConfigs (required by kafkactl)
   - CreateTopics/DeleteTopics
   - ListOffsets
   - FindCoordinator (for consumer groups)
   - JoinGroup/SyncGroup (for consumer coordination)

3. **Incomplete Implementations**:
   - Metadata doesn't return actual topic information
   - Produce doesn't persist or index data
   - Fetch doesn't read from storage
   - No actual partition management

## Kafka API Implementation Priority

### Phase 1: Core Client Compatibility (kafkactl & basic producers)
These APIs are essential for basic tool compatibility:

1. **ApiVersions (API Key 18)** - HIGH PRIORITY
   - Fix field ordering bug
   - Support all versions (0-3)
   - Add proper tagged field support for v3

2. **Metadata (API Key 3)** - HIGH PRIORITY
   - Return actual topics from metadata store
   - Include partition information
   - Support topic filtering

3. **DescribeConfigs (API Key 32)** - HIGH PRIORITY
   - Required by kafkactl for cluster inspection
   - Implement for brokers and topics
   - Return basic configuration values

4. **CreateTopics (API Key 19)** - HIGH PRIORITY
   - Enable topic creation via Kafka protocol
   - Integrate with metadata store
   - Support replication factor and partition count

5. **ListOffsets (API Key 2)** - MEDIUM PRIORITY
   - Required for determining topic boundaries
   - Support earliest/latest offset queries

### Phase 2: Producer Support
Enable full producer functionality:

6. **Produce (API Key 0)** - Enhance existing
   - Connect to actual storage layer
   - Implement proper acknowledgment levels
   - Add idempotent producer support
   - Integrate with indexing pipeline

7. **InitProducerId (API Key 22)** - MEDIUM PRIORITY
   - Support idempotent and transactional producers
   - Generate producer IDs

### Phase 3: Consumer Support
Enable consumer functionality:

8. **Fetch (API Key 1)** - Enhance existing
   - Read from actual storage
   - Support incremental fetch sessions
   - Implement proper offset management

9. **FindCoordinator (API Key 10)** - HIGH PRIORITY
   - Locate group coordinators
   - Support both group and transaction coordinators

10. **JoinGroup (API Key 11)** - HIGH PRIORITY
    - Consumer group membership
    - Rebalancing protocol support

11. **Heartbeat (API Key 12)** - HIGH PRIORITY
    - Keep consumer group membership alive
    - Detect failed consumers

12. **OffsetCommit (API Key 8)** - HIGH PRIORITY
    - Store consumer offsets
    - Support manual and automatic commits

13. **OffsetFetch (API Key 9)** - HIGH PRIORITY
    - Retrieve committed offsets
    - Resume consumption from saved positions

### Phase 4: Advanced Features
Extended functionality for full ecosystem support:

14. **AlterConfigs (API Key 33)**
    - Dynamic configuration updates
    - Topic and broker configs

15. **DeleteTopics (API Key 20)**
    - Topic deletion support
    - Cleanup storage and metadata

16. **SaslHandshake (API Key 17)**
    - Authentication support
    - Multiple SASL mechanisms

## Implementation Tasks

### Task 1: Fix ApiVersions Response Encoding
**Technical Details**:
- Move throttle_time_ms after api_versions array for v0
- Place throttle_time_ms at beginning for v1+
- Add proper tagged field encoding for v3+
- Test with real Kafka clients

**Files to modify**:
- `crates/chronik-protocol/src/handler.rs`
- `crates/chronik-protocol/src/parser.rs`

### Task 2: Implement DescribeConfigs API
**Technical Details**:
- Add DescribeConfigs to ApiKey enum
- Create request/response types
- Return configuration for:
  - Broker configs (e.g., `log.retention.hours`)
  - Topic configs (e.g., `retention.ms`, `segment.ms`)
- Store default configs in metadata store

**New files needed**:
- `crates/chronik-protocol/src/apis/describe_configs.rs`

### Task 3: Connect Metadata API to Storage
**Technical Details**:
- Query TiKV metadata store for topics
- Include partition assignments
- Return actual broker information
- Support topic name filtering

**Files to modify**:
- `crates/chronik-protocol/src/handler.rs`
- Add metadata store reference to ProtocolHandler

### Task 4: Implement CreateTopics API
**Technical Details**:
- Parse CreateTopics requests
- Validate topic names and configurations
- Create metadata entries in TiKV
- Support custom partition/replication settings
- Return proper error codes for conflicts

**New files needed**:
- `crates/chronik-protocol/src/apis/create_topics.rs`

### Task 5: Wire Produce Handler to Storage
**Technical Details**:
- Parse record batches properly
- Create segment writer integration
- Implement acknowledgment levels:
  - 0: No acknowledgment
  - 1: Leader acknowledgment
  - -1: All replicas (future)
- Update partition offsets in metadata

**Files to modify**:
- `crates/chronik-ingest/src/produce_handler.rs`
- `crates/chronik-protocol/src/handler.rs`

### Task 6: Implement Consumer Coordinator APIs
**Technical Details**:
- FindCoordinator: Hash group to coordinator
- JoinGroup: Implement rebalance protocol
- Store group membership in metadata
- Handle session timeouts

**New files needed**:
- `crates/chronik-protocol/src/coordinator/mod.rs`
- `crates/chronik-protocol/src/coordinator/group_manager.rs`

## Testing Strategy

### 1. Protocol Compliance Tests
- Use kafka-protocol-rs test vectors
- Compare responses with real Kafka broker
- Test all API versions we claim to support

### 2. Client Compatibility Tests
- **kafkactl**: All commands should work
  ```bash
  kafkactl get topics
  kafkactl create topic test-topic
  kafkactl produce test-topic --value "test message"
  kafkactl consume test-topic --from-beginning
  ```

- **Official Kafka Clients**:
  - Java client (most strict)
  - Python (kafka-python)
  - Go (Sarama)
  - Node.js (kafkajs)

### 3. Wire Protocol Tests
- Capture real Kafka traffic with tcpdump
- Replay against chronik-stream
- Compare responses byte-for-byte

### 4. Load Tests
- Concurrent producers
- Multiple consumer groups
- Rebalancing under load

## Success Metrics

1. **Phase 1 Complete**: kafkactl all commands work
2. **Phase 2 Complete**: Standard producers work (Java, Python, Go)
3. **Phase 3 Complete**: Consumer groups fully functional
4. **Phase 4 Complete**: Full ecosystem compatibility

## Timeline Estimate

- Phase 1: 2-3 weeks (Critical for MVP)
- Phase 2: 1-2 weeks 
- Phase 3: 3-4 weeks (Most complex)
- Phase 4: 2-3 weeks

Total: 8-12 weeks for full Kafka protocol compatibility

## Risk Mitigation

1. **Protocol Complexity**: Start with minimal versions, add features incrementally
2. **Client Variations**: Test with multiple clients early and often
3. **Performance**: Design for async/concurrent operations from the start
4. **Compatibility**: Maintain test suite against real Kafka

## Next Steps

1. Fix ApiVersions response encoding (immediate priority)
2. Implement DescribeConfigs (unblocks kafkactl)
3. Connect Metadata to actual topic data
4. Set up comprehensive protocol testing framework