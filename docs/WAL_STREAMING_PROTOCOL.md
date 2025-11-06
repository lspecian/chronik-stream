# PostgreSQL-Style WAL Streaming Protocol Design (v2.2.0)

## Overview

This document specifies Chronik's WAL streaming replication protocol, inspired by PostgreSQL's physical replication but adapted for Chronik's architecture.

## Design Principles

1. **Fire-and-forget from leader** - Never block produce path waiting for followers
2. **Push-based streaming** - Leader pushes WAL records as they're written
3. **TCP-based direct connections** - No intermediate brokers or message queues
4. **Ordered delivery per partition** - FIFO guarantees within each (topic, partition) tuple
5. **Eventual consistency** - Followers eventually catch up, but lag is acceptable
6. **Simple failure handling** - Disconnect and reconnect, no complex recovery protocol

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                   Leader Node (Primary)                          │
├─────────────────────────────────────────────────────────────────┤
│  Producer → ProduceHandler → WAL Write (GroupCommitWal)         │
│                  ↓                                                │
│        WalReplicationManager.replicate_async()                   │
│                  ↓                                                │
│        Push to SegQueue (lock-free MPMC queue)                   │
│                  ↓                                                │
│        Background Worker (tokio::spawn)                          │
│                  ↓                                                │
│        Serialize WAL record → Frame with header                  │
│                  ↓                                                │
│        TCP send to Follower 1, Follower 2, ... (parallel)       │
└─────────────────────────────────────────────────────────────────┘
                          ↓ TCP Stream
┌─────────────────────────────────────────────────────────────────┐
│                   Follower Node (Replica)                        │
├─────────────────────────────────────────────────────────────────┤
│  WAL Receiver (TCP listener on port 5432)                       │
│                  ↓                                                │
│  Deserialize frame → Extract WAL record                          │
│                  ↓                                                │
│  Write to local WAL (GroupCommitWal)                            │
│                  ↓                                                │
│  Update high watermark in metadata                               │
│                  ↓                                                │
│  Data available for local consumers                              │
└─────────────────────────────────────────────────────────────────┘
```

## Wire Protocol

### Connection Handshake

**Leader → Follower (on connect):**
```
HANDSHAKE {
    protocol_version: u16 = 1
    node_id: u64
    replication_mode: u8  // 0 = async, 1 = sync
}
```

**Follower → Leader (response):**
```
HANDSHAKE_ACK {
    status: u8  // 0 = OK, 1 = ERROR
    last_wal_position: u64  // For resume support (future)
    error_message: String  // If status == ERROR
}
```

### WAL Record Frame Format

Each WAL record is sent as a length-prefixed frame:

```
┌──────────────┬──────────────┬────────────────────────┐
│ Frame Header │ WAL Metadata │    WAL Record Data     │
│   (8 bytes)  │  (24 bytes)  │    (variable length)   │
└──────────────┴──────────────┴────────────────────────┘

Frame Header (8 bytes):
  - magic: u16 = 0x5741  // 'WA' in hex
  - version: u16 = 1
  - total_length: u32 = metadata length + data length

WAL Metadata (24 bytes):
  - topic_len: u16
  - topic: String (variable, specified by topic_len)
  - partition: i32
  - base_offset: i64
  - record_count: u32
  - timestamp_ms: i64
  - checksum: u32 (CRC32 of data)

WAL Record Data (variable):
  - Bincode-serialized CanonicalRecord
```

### Heartbeat Frame

Sent every 10 seconds to detect dead connections:

```
HEARTBEAT {
    magic: u16 = 0x4842  // 'HB' in hex
    timestamp_ms: i64
}
```

Follower responds with:
```
HEARTBEAT_ACK {
    timestamp_ms: i64 (echo back)
}
```

## Connection Lifecycle

### Leader Side

1. **Initialization** (on WalReplicationManager::new())
   - Parse follower addresses from config (e.g., "follower1:5432,follower2:5432")
   - Spawn background worker task
   - Create SegQueue for WAL records

2. **Connection Establishment**
   - Attempt TCP connect to each follower
   - Send HANDSHAKE frame
   - Wait for HANDSHAKE_ACK (with 5s timeout)
   - On success: Add TcpStream to active connections
   - On failure: Log error, retry after 30s

3. **Steady State**
   - `replicate_async()` pushes records to SegQueue
   - Background worker:
     - Dequeues records from SegQueue
     - Serializes to frame format
     - Sends to ALL active followers (fan-out)
     - Does NOT wait for ACK (fire-and-forget)
   - Heartbeat loop (separate task):
     - Send HEARTBEAT every 10s
     - If no HEARTBEAT_ACK within 30s → close connection, reconnect

4. **Error Handling**
   - TCP write error → close connection, remove from active set, schedule reconnect
   - SegQueue full (> 100,000 records) → drop oldest records, log warning
   - All followers disconnected → keep accepting writes, log warning

### Follower Side

1. **Initialization**
   - Bind TCP listener on replication port (default 5432)
   - Spawn accept loop

2. **Connection Acceptance**
   - Accept incoming TCP connection from leader
   - Receive HANDSHAKE frame
   - Validate protocol_version == 1
   - Send HANDSHAKE_ACK with status = OK

3. **Steady State**
   - Read frames from TCP stream
   - Deserialize WAL record
   - Write to local WAL (WalManager::append())
   - Update metadata high watermark
   - Respond to HEARTBEAT with HEARTBEAT_ACK

4. **Error Handling**
   - TCP read error → close connection, wait for leader reconnect
   - Deserialization error → log error, skip record, continue
   - WAL write error → log error, close connection (critical failure)

## Configuration

### Leader Config

```rust
pub struct WalReplicationConfig {
    /// Comma-separated list of follower addresses
    /// Example: "10.0.1.2:5432,10.0.1.3:5432"
    pub followers: Vec<String>,

    /// Replication mode: Async (default) or Sync
    pub mode: ReplicationMode,

    /// Max queue size before dropping old records
    pub max_queue_size: usize, // Default: 100,000

    /// Heartbeat interval (seconds)
    pub heartbeat_interval_secs: u64, // Default: 10

    /// Heartbeat timeout (seconds)
    pub heartbeat_timeout_secs: u64, // Default: 30

    /// Reconnect delay (seconds)
    pub reconnect_delay_secs: u64, // Default: 30
}
```

### Follower Config

```rust
pub struct WalReceiverConfig {
    /// Bind address for replication listener
    pub bind_addr: String, // Default: "0.0.0.0:5432"

    /// Enable WAL receiver (follower mode)
    pub enabled: bool, // Default: false (standalone)
}
```

## Environment Variables

**Leader:**
```bash
CHRONIK_REPLICATION_FOLLOWERS="follower1:5432,follower2:5432"
CHRONIK_REPLICATION_MODE="async"  # or "sync"
CHRONIK_REPLICATION_MAX_QUEUE_SIZE="100000"
```

**Follower:**
```bash
CHRONIK_WAL_RECEIVER_ENABLED="true"
CHRONIK_WAL_RECEIVER_BIND_ADDR="0.0.0.0:5432"
```

## Performance Considerations

### Leader Overhead

- **SegQueue push**: O(1), lock-free
- **Serialization**: ~1-2 μs per record (bincode is fast)
- **TCP send**: Batched writes, ~10-50 μs per batch
- **Total overhead**: < 5 μs added to produce path

### Network Bandwidth

- Typical WAL record: ~500 bytes (256 byte message + metadata + overhead)
- At 80,000 msg/s: 40 MB/s per follower
- 3 followers = 120 MB/s total outbound bandwidth
- Assumes 1 Gbps network = 125 MB/s → 96% utilization at peak

### Lag and Catch-up

- Normal lag: < 100ms (network latency + processing)
- Heavy load lag: 1-5 seconds (queue backlog)
- Follower failure: Queue grows to max_queue_size, then drops old records
- On reconnect: Follower is permanently behind (no automatic catch-up in v2.2.0)

## Failure Modes and Recovery

### Leader Failure

- **What happens**: All followers become standalone (no more WAL records)
- **Detection**: Heartbeat timeout (follower doesn't receive HEARTBEAT)
- **Recovery**: Manual failover to follower (requires operator intervention in v2.2.0)
- **Future**: Automatic leader election (v2.3.0+)

### Follower Failure

- **What happens**: Leader disconnects, continues serving clients
- **Detection**: TCP write error or heartbeat timeout
- **Recovery**: Automatic reconnect after 30s
- **Data loss**: Records sent while disconnected are lost (queue overflow)

### Network Partition

- **What happens**: Follower isolated from leader
- **Detection**: Heartbeat timeout on both sides
- **Recovery**: Automatic reconnection when network heals
- **Split-brain**: Possible if clients can reach both (no fencing in v2.2.0)

### Queue Overflow

- **What happens**: Leader's SegQueue reaches max_queue_size
- **Behavior**: Drop oldest records, log warning
- **Impact**: Follower permanently lags, must resync from backup

## Future Enhancements (v2.3.0+)

1. **Automatic catch-up**: Follower requests missing WAL range on reconnect
2. **Leader election**: Distributed consensus for automatic failover
3. **Synchronous replication**: Option to wait for N followers before ACK to client
4. **Compression**: Compress WAL records before network transmission
5. **Batch ACKs**: Follower sends periodic ACKs for monitoring lag
6. **Metrics**: Track replication lag, bytes sent, connection state

## Implementation Phases

**Phase 3.1: Basic Streaming (this phase)**
- Implement WalReplicationManager with SegQueue
- Implement background worker with TCP connections
- Implement frame serialization/deserialization
- Implement follower WAL receiver
- Test with 2-node cluster (1 leader + 1 follower)

**Phase 3.2: Multi-Follower Support**
- Test with 3-node cluster (1 leader + 2 followers)
- Verify fan-out performance
- Test follower failure and reconnection

**Phase 3.3: Production Hardening**
- Add comprehensive error handling
- Add metrics and monitoring
- Add configuration validation
- Performance testing at scale

## References

- PostgreSQL Physical Replication: https://www.postgresql.org/docs/current/warm-standby.html
- Kafka Replication Protocol: https://kafka.apache.org/protocol#The_Messages_Replication
- Chronik WAL Design: docs/WRITE_AHEAD_LOG.md
