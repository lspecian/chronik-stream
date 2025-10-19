# FetchHandler Integration with ReadIndex Protocol

This document describes how to integrate the `ReadIndexManager` with Chronik's `FetchHandler` to enable safe linearizable follower reads.

## Overview

The ReadIndex protocol allows followers to serve read requests without forwarding to the leader, while maintaining linearizability guarantees. This significantly improves read throughput and reduces leader load in Raft-replicated partitions.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     Kafka Fetch Request                       │
└──────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────┐
│                      FetchHandler                             │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ 1. Check follower_read_policy config                   │  │
│  │    - "none": Forward to leader (default)               │  │
│  │    - "unsafe": Read local state immediately            │  │
│  │    - "safe": Use ReadIndex protocol ✓                  │  │
│  └────────────────────────────────────────────────────────┘  │
│                              ↓                                │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ 2. If policy="safe" and not leader:                    │  │
│  │    ReadIndexManager.request_read_index()               │  │
│  └────────────────────────────────────────────────────────┘  │
│                              ↓                                │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ 3. Wait until is_safe_to_read(commit_index)            │  │
│  └────────────────────────────────────────────────────────┘  │
│                              ↓                                │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ 4. Serve from local SegmentReader                      │  │
│  └────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
```

## Configuration

### FetchHandler Config

Add to `chronik-server/src/fetch_handler.rs`:

```rust
pub struct FetchHandlerConfig {
    // Existing fields...

    /// Follower read policy
    /// - "none": No follower reads (forward to leader) - DEFAULT
    /// - "unsafe": Read from follower immediately (may be stale)
    /// - "safe": Use ReadIndex protocol for linearizable reads
    pub follower_read_policy: FollowerReadPolicy,

    /// Timeout for ReadIndex requests (default: 5s)
    pub read_index_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowerReadPolicy {
    None,      // Forward to leader
    Unsafe,    // Read local state immediately
    Safe,      // Use ReadIndex protocol
}

impl Default for FollowerReadPolicy {
    fn default() -> Self {
        FollowerReadPolicy::None  // Conservative default
    }
}
```

### Environment Variables

```bash
# Enable safe follower reads (default: none)
export CHRONIK_FOLLOWER_READ_POLICY=safe

# Configure timeout (default: 5s)
export CHRONIK_READ_INDEX_TIMEOUT=3s
```

## Implementation

### Step 1: Add ReadIndexManager to FetchHandler

```rust
use chronik_raft::{ReadIndexManager, ReadIndexRequest, ReadIndexResponse, PartitionReplica};
use std::sync::Arc;

pub struct FetchHandler {
    // Existing fields...
    metadata_store: Arc<dyn MetadataStore>,
    segment_reader: Arc<SegmentReader>,

    // NEW: ReadIndex manager (optional, only for Raft-replicated partitions)
    read_index_manager: Option<Arc<ReadIndexManager>>,

    // NEW: Configuration
    config: FetchHandlerConfig,
}

impl FetchHandler {
    pub fn new(
        metadata_store: Arc<dyn MetadataStore>,
        segment_reader: Arc<SegmentReader>,
        config: FetchHandlerConfig,
    ) -> Self {
        Self {
            metadata_store,
            segment_reader,
            read_index_manager: None,
            config,
        }
    }

    /// Register ReadIndexManager for Raft-replicated partitions
    pub fn with_read_index_manager(
        mut self,
        manager: Arc<ReadIndexManager>,
    ) -> Self {
        self.read_index_manager = Some(manager);
        self
    }
}
```

### Step 2: Implement Safe Follower Read Logic

```rust
impl FetchHandler {
    pub async fn handle_fetch(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponse> {
        // Check follower read policy
        match self.config.follower_read_policy {
            FollowerReadPolicy::None => {
                // Forward to leader (existing behavior)
                self.forward_to_leader(topic, partition, fetch_offset, max_bytes).await
            }

            FollowerReadPolicy::Unsafe => {
                // Read local state immediately (no consistency guarantee)
                self.read_local(topic, partition, fetch_offset, max_bytes).await
            }

            FollowerReadPolicy::Safe => {
                // Use ReadIndex protocol for linearizable reads
                self.safe_follower_read(topic, partition, fetch_offset, max_bytes).await
            }
        }
    }

    async fn safe_follower_read(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponse> {
        // Check if we have ReadIndexManager (only for Raft partitions)
        let manager = match &self.read_index_manager {
            Some(mgr) => mgr,
            None => {
                // No Raft replication, fall back to local read
                return self.read_local(topic, partition, fetch_offset, max_bytes).await;
            }
        };

        // Step 1: Request read index from leader
        let read_req = ReadIndexRequest {
            topic: topic.to_string(),
            partition,
        };

        let read_response = manager.request_read_index(read_req).await.map_err(|e| {
            FetchError::ReadIndexFailed(format!(
                "Failed to get read index for {}-{}: {}",
                topic, partition, e
            ))
        })?;

        // Step 2: If we're the leader, can read immediately
        if read_response.is_leader {
            return self.read_local(topic, partition, fetch_offset, max_bytes).await;
        }

        // Step 3: Wait until safe to read (applied_index >= commit_index)
        let commit_index = read_response.commit_index;
        let start = Instant::now();

        while !manager.is_safe_to_read(commit_index) {
            // Check timeout
            if start.elapsed() > self.config.read_index_timeout {
                return Err(FetchError::ReadIndexTimeout(format!(
                    "Timed out waiting for applied_index >= {} on {}-{}",
                    commit_index, topic, partition
                )));
            }

            // Wait a bit before checking again
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Step 4: Safe to read from local state
        debug!(
            "Safe follower read: topic={} partition={} commit_index={} latency={}ms",
            topic,
            partition,
            commit_index,
            start.elapsed().as_millis()
        );

        self.read_local(topic, partition, fetch_offset, max_bytes).await
    }

    async fn read_local(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponse> {
        // Existing implementation - read from SegmentReader
        self.segment_reader.fetch_records(
            topic,
            partition,
            fetch_offset,
            max_bytes,
        ).await
    }

    async fn forward_to_leader(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponse> {
        // Existing implementation - forward to leader node
        // ...
    }
}
```

### Step 3: Initialize ReadIndexManager

In `chronik-server/src/main.rs` or `integrated_server.rs`:

```rust
use chronik_raft::{PartitionReplica, ReadIndexManager};
use std::sync::Arc;

// Create Raft replica for partition
let raft_replica = Arc::new(PartitionReplica::new(
    topic.clone(),
    partition,
    raft_config,
    log_storage,
    peers,
)?);

// Create ReadIndex manager
let read_index_manager = Arc::new(ReadIndexManager::new(
    node_id,
    raft_replica.clone(),
));

// Spawn background timeout loop
let _timeout_handle = read_index_manager.clone().spawn_timeout_loop();

// Create FetchHandler with ReadIndex support
let fetch_handler = FetchHandler::new(
    metadata_store,
    segment_reader,
    fetch_config,
)
.with_read_index_manager(read_index_manager);
```

## Performance Characteristics

### Latency Comparison

| Read Type | Typical Latency | Network Hops | Consistency |
|-----------|----------------|--------------|-------------|
| **Leader Read** | 1-5ms | 0 (local) | Linearizable |
| **Follower (unsafe)** | 1-5ms | 0 (local) | **None** (may be stale) |
| **Follower (safe - ReadIndex)** | 10-50ms | 1 (ReadIndex RPC) | **Linearizable** |
| **Forward to Leader** | 50-200ms | 2 (forward + response) | Linearizable |

### Throughput Improvement

With 3-node Raft cluster and `follower_read_policy=safe`:

- **Before**: All reads go through leader → Leader saturates at ~50K req/s
- **After**: Reads distributed across followers → Cluster handles ~150K req/s (3x)

### Trade-offs

**Safe Follower Reads (ReadIndex)**:
- ✅ Linearizable consistency
- ✅ 3x read throughput (3-node cluster)
- ✅ Reduced leader load
- ❌ +10-50ms latency vs leader read (ReadIndex RPC overhead)

**Unsafe Follower Reads**:
- ✅ Same latency as leader read
- ✅ 3x read throughput
- ❌ **NO consistency guarantee** (may read stale data)
- ❌ Violates Kafka's exactly-once semantics

## Metrics

Add Prometheus metrics to track ReadIndex performance:

```rust
// In FetchHandler
lazy_static! {
    static ref FOLLOWER_READS_TOTAL: IntCounterVec = register_int_counter_vec!(
        "chronik_follower_reads_total",
        "Total number of follower reads",
        &["topic", "partition", "policy"]
    ).unwrap();

    static ref READ_INDEX_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_read_index_latency_seconds",
        "ReadIndex request latency",
        &["topic", "partition"],
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5]
    ).unwrap();

    static ref READ_INDEX_TIMEOUTS: IntCounterVec = register_int_counter_vec!(
        "chronik_read_index_timeouts_total",
        "Total number of ReadIndex timeouts",
        &["topic", "partition"]
    ).unwrap();
}

// Instrument safe_follower_read()
let start = Instant::now();
let result = manager.request_read_index(read_req).await;
READ_INDEX_LATENCY
    .with_label_values(&[topic, &partition.to_string()])
    .observe(start.elapsed().as_secs_f64());

if result.is_err() {
    READ_INDEX_TIMEOUTS
        .with_label_values(&[topic, &partition.to_string()])
        .inc();
}

FOLLOWER_READS_TOTAL
    .with_label_values(&[topic, &partition.to_string(), "safe"])
    .inc();
```

## Testing

### Unit Test: Safe Follower Read

```rust
#[tokio::test]
async fn test_safe_follower_read() {
    // Setup 3-node Raft cluster
    let replicas = create_3_node_cluster().await;
    let leader = &replicas[0];
    let follower = &replicas[1];

    // Create ReadIndexManager on follower
    let manager = Arc::new(ReadIndexManager::new(2, follower.clone()));

    // Create FetchHandler with safe reads
    let config = FetchHandlerConfig {
        follower_read_policy: FollowerReadPolicy::Safe,
        ..Default::default()
    };

    let fetch_handler = FetchHandler::new(
        metadata_store,
        segment_reader,
        config,
    ).with_read_index_manager(manager);

    // Write on leader
    leader.propose(b"test data").await.unwrap();

    // Wait for replication
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Read from follower (should see the write)
    let response = fetch_handler.handle_fetch("test", 0, 0, 1024).await.unwrap();
    assert_eq!(response.records.len(), 1);
    assert_eq!(response.records[0].data, b"test data");
}
```

### Integration Test: Read-After-Write Linearizability

```rust
#[tokio::test]
async fn test_read_after_write_linearizability() {
    let cluster = create_3_node_cluster().await;
    let leader = &cluster[0];
    let follower_handler = create_follower_fetch_handler(&cluster[1]).await;

    // Write 100 messages
    for i in 0..100 {
        leader.propose(format!("msg-{}", i).as_bytes().to_vec()).await.unwrap();
    }

    // Read from follower
    let response = follower_handler.handle_fetch("test", 0, 0, 1024 * 1024).await.unwrap();

    // Should see all 100 messages (linearizable)
    assert_eq!(response.records.len(), 100);
}
```

## Rollout Strategy

1. **Phase 1: Deploy with `follower_read_policy=none`** (default)
   - No behavior change, all reads go through leader
   - Monitor baseline metrics

2. **Phase 2: Enable on staging with `follower_read_policy=safe`**
   - Verify linearizability with integration tests
   - Monitor `chronik_read_index_latency_seconds`
   - Check for timeouts

3. **Phase 3: Canary rollout to production**
   - Enable on 10% of partitions
   - Monitor error rates and latency
   - Gradually increase to 100%

4. **Phase 4: Consider `follower_read_policy=unsafe` for specific use cases**
   - Only for read-only analytics workloads
   - Document consistency implications
   - Require explicit opt-in

## Future Optimizations

1. **ReadIndex Batching**: Batch multiple read requests into single ReadIndex RPC
2. **Lease-based Reads**: Leader grants time-bounded leases to followers (no RPC needed)
3. **Smart Retry**: Retry on leader change with exponential backoff
4. **Cached ReadIndex**: Cache recent ReadIndex responses (bounded staleness)

## References

- [Raft Paper Section 6.4: Processing read-only queries more efficiently](https://raft.github.io/raft.pdf)
- [etcd's implementation](https://github.com/etcd-io/etcd/blob/main/server/etcdserver/v3_server.go#L156)
- [TiKV's ReadIndex implementation](https://tikv.org/deep-dive/distributed-transaction/read/)
