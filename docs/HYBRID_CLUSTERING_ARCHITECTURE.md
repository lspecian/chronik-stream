# Chronik Hybrid Clustering Architecture

**Goal**: Fully Kafka-compatible cluster with WAL-based storage

**Last Updated**: 2025-11-02
**Version**: v2.5.0

---

## Core Design Principle: Hybrid Clustering System

Chronik uses a **hybrid approach** combining two complementary systems for different purposes:

```
┌─────────────────────────────────────────────────────────────────┐
│                  CHRONIK HYBRID CLUSTERING                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌──────────────────┐              ┌─────────────────────────┐ │
│  │  RAFT CONSENSUS  │              │  WAL STREAMING          │ │
│  │  (Metadata Only) │              │  (Message Data)         │ │
│  └──────────────────┘              └─────────────────────────┘ │
│          │                                      │               │
│          │                                      │               │
│    Manages:                              Replicates:            │
│    • Leader election                     • Kafka messages      │
│    • ISR tracking                        • Record batches      │
│    • Partition assignments               • High throughput     │
│    • Cluster membership                  • Zero-copy design    │
│                                                                 │
│    Storage:                              Storage:               │
│    data/wal/__meta/                      data/wal/{topic}/{p}/ │
│    (RaftWalStorage)                      (GroupCommitWal)      │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## 1. Raft Consensus - For Metadata Only

### Purpose
**Cluster coordination and metadata management** (NOT data replication)

### Location
- **Code**: `crates/chronik-server/src/raft_cluster.rs`
- **State Machine**: `crates/chronik-server/src/raft_metadata.rs`
- **Storage**: `data/wal/__meta/` (separate from message data)
- **WAL Type**: `RaftWalStorage` (chronik-wal crate)

### What Raft Manages
- ✅ **Cluster membership**: Which nodes are alive and part of the cluster
- ✅ **Partition assignments**: partition-0 → [node1, node2, node3]
- ✅ **Partition leader election**: partition-0 leader = node1
- ✅ **ISR tracking**: In-sync replicas per partition

### What Raft Does NOT Manage
- ❌ **Message data replication** (too slow for high throughput)
- ❌ **Producer requests** (WAL streaming handles this)
- ❌ **Consumer fetch** (direct reads from local WAL)

### Key Insight
Raft has its **own WAL** (`RaftWalStorage`) for persisting the Raft log, which contains metadata commands, NOT message data.

### Performance Characteristics
- **Throughput**: Suitable for low-volume metadata ops (100s of commands/sec)
- **Latency**: Consensus adds overhead (~5-20ms per operation)
- **Reliability**: Strong consistency, automatic leader election, split-brain protection

---

## 2. WAL Streaming Replication - For Message Data

### Purpose
**High-performance message data replication** (PostgreSQL-style)

### Location
- **Code**: `crates/chronik-server/src/wal_replication.rs`
- **Storage**: `data/wal/{topic}/{partition}/` (per-partition message data)
- **WAL Type**: `GroupCommitWal` (chronik-wal crate)

### How It Works
- **Leader** → TCP streams serialized `CanonicalRecord` to followers
- **Followers** → Write to local WAL and send ACK back to leader
- **Zero-copy design**: Direct bincode serialization, no re-parsing
- **Fire-and-forget**: Never blocks the produce path (acks=0/1)
- **Quorum wait**: Only blocks for acks=-1 (ISR quorum)

### Configuration
**Leader**:
```bash
CHRONIK_REPLICATION_FOLLOWERS=localhost:9291,localhost:9292
```

**Follower**:
```bash
CHRONIK_WAL_RECEIVER_ADDR=0.0.0.0:9291
```

### Performance Characteristics
- **Throughput**: 60K+ msg/s (proven in v2.2.0)
- **Latency**: < 5ms overhead for streaming
- **Reliability**: ACK-based confirmation, ISR quorum support

---

## Data Flow

### Produce Path (acks=1)
```
Producer → Leader Node
           ↓
       Write to local WAL (immediate durability)
           ↓
       Return success to producer (FAST!)
           ↓
       [Background] Stream to followers via TCP (WalReplicationManager)
           ↓
       Followers write to their local WAL
```

### Produce Path (acks=-1, ISR quorum)
```
Producer → Leader Node
           ↓
       Write to local WAL
           ↓
       Stream to ISR followers (tracked by Raft metadata)
           ↓
       Wait for follower ACKs (max 30s timeout)
           ↓
       Once quorum reached → Return success to producer
```

### Metadata Operations (via Raft)
```
Admin Command (e.g., "elect new leader for partition-0")
           ↓
       Leader proposes MetadataCommand to Raft
           ↓
       Raft replicates to quorum
           ↓
       Raft commits → Apply to MetadataStateMachine
           ↓
       Metadata change visible to all nodes
```

---

## Two Separate WAL Systems

### 1. Raft Metadata WAL
- **Location**: `data/wal/__meta/`
- **Storage**: `RaftWalStorage` (chronik-wal crate)
- **Contents**: Raft log entries (`MetadataCommand` serialized)
- **Purpose**: Persist Raft consensus log for crash recovery
- **Volume**: Low (100s of entries/sec)

### 2. Message Data WAL
- **Location**: `data/wal/{topic}/{partition}/`
- **Storage**: `GroupCommitWal` (chronik-wal crate)
- **Contents**: Kafka message batches (`CanonicalRecord` serialized)
- **Purpose**: Persist message data with zero-loss guarantee
- **Volume**: High (60K+ messages/sec)

**CRITICAL**: These are completely separate WAL instances with different lifecycles and purposes.

---

## Why This Hybrid Approach?

### Performance
- **Raft for metadata**: Low volume, strong consistency, automatic leader election
- **WAL streaming for data**: High throughput, zero-copy, minimal overhead

### Kafka Compatibility
- Matches Kafka's design: ZooKeeper/KRaft for metadata, partition replication for data
- Supports all acks modes (0, 1, -1)
- ISR tracking and quorum semantics

### Operational Benefits
- Automatic partition leader election (Raft handles this)
- Manual data replication control (WAL streaming, proven performance)
- Clear separation of concerns

---

## What NOT to Do

### ❌ Do NOT replicate message data through Raft

**Reason**: Raft has consensus overhead, too slow for high-throughput data

**Performance**:
- Raft data replication: ~2-5K msg/s
- WAL streaming: 60K+ msg/s

**Example of WRONG approach** (dead code in produce_handler.rs):
```rust
// WRONG - Do NOT do this:
raft_manager.propose(topic, partition, serialized_data).await
```

### ❌ Do NOT use Raft for anything except metadata

Raft is for cluster coordination, NOT the data plane.

### ✅ Do use WAL streaming for all message data replication

**Proven performance**: 52K msg/s standalone, 28K+ with replication

---

## Code Locations

### Active Code (Keep)
| Component | Location | Purpose |
|-----------|----------|---------|
| WAL Streaming | `crates/chronik-server/src/wal_replication.rs` | Data replication |
| Raft Cluster | `crates/chronik-server/src/raft_cluster.rs` | Metadata coordination |
| Metadata State Machine | `crates/chronik-server/src/raft_metadata.rs` | Raft state machine |
| ISR Tracker | `crates/chronik-server/src/isr_tracker.rs` | ISR tracking |
| ISR ACK Tracker | `crates/chronik-server/src/isr_ack_tracker.rs` | acks=-1 quorum |

### Dead Code (To Remove)
| Location | Reason |
|----------|--------|
| `crates/chronik-server/src/produce_handler.rs` lines 1233-1356 | Old Raft data replication (WRONG approach, too slow) |
| Behind `#[cfg(feature = "raft")]` | Feature flag not enabled by default |

**Action Required**: Remove the entire `#[cfg(feature = "raft")]` block in produce_handler.rs

---

## Current Status (v2.5.0)

### What Works
- ✅ WAL streaming replication (bug fixed in commit 31e945d)
- ✅ Raft metadata coordination
- ✅ acks=0/1/−1 support
- ✅ ISR tracking
- ✅ Performance optimizations (65.5K msg/s standalone)

### Known Issues
- ⚠️ node_id not passed correctly to WalReceiver (followers report as node_id=1)
- ⚠️ Some ISR quorum timeouts due to node_id issue
- 🧹 Dead Raft data replication code needs removal

---

## Performance Targets

| Mode | Target | Current |
|------|--------|---------|
| Standalone (no replication) | >= 70K msg/s | 65.5K msg/s |
| acks=0/1 (async replication) | >= 55K msg/s | 61K msg/s ✅ |
| acks=-1 (ISR quorum) | >= 40K msg/s | Testing |

**Baseline**: v2.1.0 = 78.5K msg/s (goal: match or exceed)

---

## Summary

**Chronik = Raft (metadata) + WAL Streaming (data)**

This hybrid approach gives us:
- ✅ Strong consistency for cluster coordination (Raft)
- ✅ High performance for message replication (WAL streaming)
- ✅ Full Kafka compatibility
- ✅ Clean separation of concerns

**Golden Rule**: If code tries to replicate message data through Raft, it's WRONG and should be removed.

---

## References

- **Clean Implementation Plan**: `docs/CLEAN_RAFT_IMPLEMENTATION.md`
- **WAL Streaming Protocol**: `docs/WAL_STREAMING_PROTOCOL.md`
- **Performance Analysis**: `docs/V2.2_V2.3_PERFORMANCE_REGRESSION_ANALYSIS.md`
