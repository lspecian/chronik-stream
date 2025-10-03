# KSQL Compatibility Testing - Critical Findings

## Test Setup
- **KSQL Version**: confluentinc/ksqldb-server:0.29.0
- **Target**: Full KSQL 7.5.x compatibility with Kafka 3.5.x protocol
- **Date**: 2025-09-29

## Critical Issue #1: Network Connectivity - KSQL Cannot Connect to Chronik

### Problem
KSQLDB container attempts to connect to `host.docker.internal:9092` but Chronik server advertises `127.0.0.1:9092`. This causes connection failures.

### Evidence
**KSQL Logs:**
```
[2025-09-30 04:19:39,450] INFO [AdminClient clientId=adminclient-2] Node -1 disconnected. (org.apache.kafka.clients.NetworkClient:976)
[2025-09-30 04:19:39,451] WARN [AdminClient clientId=adminclient-2] Connection to node -1 (host.docker.internal/192.168.65.254:9092) could not be established. Broker may not be available. (org.apache.kafka.clients.NetworkClient:814)
```

**Chronik Logs:**
```
[33m WARN[0m [2mchronik_server[0m[2m:[0m Using '127.0.0.1' as advertised address - remote clients may not connect
[33m WARN[0m [2mchronik_server[0m[2m:[0m Set CHRONIK_ADVERTISED_ADDR to your hostname/IP for remote access
```

### Root Cause
Chronik server binds to `0.0.0.0:9092` but advertises `127.0.0.1:9092`, making it unreachable from Docker containers that need to connect via `host.docker.internal`.

### Required Fix
Set `CHRONIK_ADVERTISED_ADDR` environment variable to make Chronik accessible from external clients.

## Testing Priority Queue

### Immediate (Blocking KSQL Connection)
1. **Fix advertised address configuration**
2. **Test basic connectivity from KSQL**

### Next (Core Kafka APIs)
3. **ApiVersions request/response compatibility**
4. **Metadata API compatibility with Kafka 3.5.x**
5. **FindCoordinator for consumer groups**

### Missing API Implementation Status

Based on comprehensive analysis, the following APIs are missing or incomplete for full KSQL compatibility:

#### Consumer Group Coordination APIs (Critical)
- [ ] **JoinGroup** - Group rebalancing
- [ ] **SyncGroup** - Partition assignment
- [ ] **Heartbeat** - Keep-alive mechanism
- [ ] **LeaveGroup** - Clean group departure
- [ ] **OffsetCommit** - Persist consumer offsets
- [ ] **OffsetFetch** - Retrieve consumer offsets

#### Topic Management APIs (Critical)
- [ ] **CreateTopics** - Auto-creation of internal topics
- [ ] **DeleteTopics** - Topic cleanup
- [ ] **CreatePartitions** - Partition scaling

#### Transaction APIs (Critical for KSQL)
- [ ] **InitProducerId** - Transactional producer setup
- [ ] **AddPartitionsToTxn** - Transaction coordination
- [ ] **AddOffsetsToTxn** - Include consumer offsets in transactions
- [ ] **EndTxn** - Commit/abort transactions
- [ ] **TxnOffsetCommit** - Transactional offset commits

#### Administrative APIs (Required)
- [ ] **ListOffsets** - Timestamp-based offset lookup
- [ ] **DescribeGroups** - Group state inspection
- [ ] **ListGroups** - Consumer group discovery

#### Schema Registry (Partially Implemented)
- [x] **Basic endpoints** (GET /subjects, POST /subjects/{subject}/versions)
- [ ] **Schema compatibility checks**
- [ ] **Schema evolution support**

## Next Steps
1. Fix network connectivity issue
2. Test basic Kafka client handshake (ApiVersions + Metadata)
3. Implement missing consumer group coordination APIs
4. Test end-to-end KSQL query execution

## Success Criteria
- KSQL successfully connects to Chronik Stream
- KSQL can execute: `CREATE STREAM`, `INSERT`, `SELECT` statements
- Consumer groups function correctly with rebalancing
- Transactional processing works end-to-end