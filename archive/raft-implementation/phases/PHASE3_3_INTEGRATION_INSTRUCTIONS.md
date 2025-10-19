# Phase 3.3: Integration Instructions for RaftMetaLog

## Overview

This document provides step-by-step instructions for integrating `RaftMetaLog` into `IntegratedKafkaServer` to enable metadata replication in clustered mode.

## File Locations

### New Files Created
- `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-common/src/metadata/raft_meta_log.rs` - Raft-replicated metadata store wrapper
- `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-common/src/metadata/raft_state_machine.rs` - Raft state machine for metadata
- `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-common/src/metadata/mod.rs` - Updated to export new modules

### Files to Modify
- `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-server/src/integrated_server.rs` - Main integration point

## Integration Steps

### Step 1: Update Imports in `integrated_server.rs`

Add these imports at the top of the file:

```rust
// Raft metadata replication (when raft feature enabled)
#[cfg(feature = "raft")]
use chronik_common::metadata::{
    RaftMetaLog,
    MetadataStateMachine,
    METADATA_PARTITION,
    RAFT_METADATA_TOPIC,
};

#[cfg(feature = "raft")]
use crate::raft_integration::{create_raft_log_storage, RaftReplicaManager};
```

### Step 2: Modify Metadata Store Creation Logic

Locate the section in `IntegratedKafkaServer::new_internal()` where the metadata store is created (around line 166). Replace the existing logic with:

```rust
// Initialize metadata store based on configuration
let metadata_store: Arc<dyn MetadataStore> = if config.use_wal_metadata {
    info!("Initializing WAL-based metadata store");

    // Create WAL configuration for metadata
    let wal_config = chronik_wal::config::WalConfig {
        enabled: true,
        data_dir: PathBuf::from(format!("{}/wal_metadata", config.data_dir)),
        segment_size: 50 * 1024 * 1024, // 50MB segments for metadata
        flush_interval_ms: 100,
        flush_threshold: 1024 * 1024,
        compression: chronik_wal::config::CompressionType::None,
        checkpointing: chronik_wal::config::CheckpointConfig {
            enabled: true,
            interval_records: 1000,
            interval_bytes: 10 * 1024 * 1024,
        },
        rotation: chronik_wal::config::RotationConfig {
            max_segment_size: 50 * 1024 * 1024,
            max_segment_age_ms: 60 * 60 * 1000,
            coordinate_with_storage: false,
        },
        fsync: chronik_wal::config::FsyncConfig {
            enabled: true,
            batch_size: 1,
            batch_timeout_ms: 0,
        },
        ..Default::default()
    };

    // Create the real WAL adapter
    use chronik_storage::WalMetadataAdapter;
    let wal_adapter = Arc::new(WalMetadataAdapter::new(wal_config).await?);

    // Create inner ChronikMetaLogStore
    let inner_metalog_store = Arc::new(ChronikMetaLogStore::new(
        wal_adapter,
        PathBuf::from(format!("{}/metalog_snapshots", config.data_dir)),
    ).await?);

    info!("Successfully initialized WAL-based metadata store");

    // Check if clustering is enabled
    #[cfg(feature = "raft")]
    let final_metadata_store: Arc<dyn MetadataStore> = if let Some(cluster_config) = &config.cluster_config {
        // CLUSTERED MODE: Wrap with RaftMetaLog
        info!("Clustering enabled - initializing Raft-replicated metadata store");

        // Ensure raft_manager is available
        let raft_mgr = raft_manager.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Raft manager required for clustered mode"))?;

        // Create metadata Raft group
        info!("Creating metadata Raft replica: {}:{}", RAFT_METADATA_TOPIC, METADATA_PARTITION);

        let log_storage = create_raft_log_storage(
            &std::path::Path::new(&config.data_dir),
            RAFT_METADATA_TOPIC,
            METADATA_PARTITION,
        ).await?;

        // Create metadata state machine
        let metadata_sm = Arc::new(tokio::sync::RwLock::new(
            MetadataStateMachine::new(inner_metalog_store.clone())
        ));

        // Get peer node IDs
        let peers: Vec<u64> = cluster_config.nodes.iter()
            .filter(|n| n.node_id != config.node_id as u64)
            .map(|n| n.node_id)
            .collect();

        info!("Metadata Raft group peers: {:?}", peers);

        // Create Raft replica for metadata
        raft_mgr.create_replica(
            RAFT_METADATA_TOPIC.to_string(),
            METADATA_PARTITION,
            log_storage,
            peers,
        ).await?;

        info!("Created metadata Raft replica successfully");

        // Wrap with RaftMetaLog
        Arc::new(RaftMetaLog::new(
            inner_metalog_store,
            raft_mgr.clone(),
            config.node_id as u64,
        )) as Arc<dyn MetadataStore>
    } else {
        // STANDALONE MODE: Use inner store directly
        info!("Standalone mode - using local metadata store (no Raft)");
        inner_metalog_store as Arc<dyn MetadataStore>
    };

    #[cfg(not(feature = "raft"))]
    let final_metadata_store: Arc<dyn MetadataStore> = {
        info!("Raft feature not enabled - using local metadata store");
        inner_metalog_store as Arc<dyn MetadataStore>
    };

    // Ensure __meta topic exists
    if final_metadata_store.get_topic(chronik_common::metadata::METADATA_TOPIC).await?.is_none() {
        info!("Creating internal metadata topic: {}", chronik_common::metadata::METADATA_TOPIC);
        let meta_config = chronik_common::metadata::TopicConfig {
            partition_count: 1,
            replication_factor: 1,
            retention_ms: None,
            segment_bytes: 50 * 1024 * 1024,
            config: {
                let mut cfg = std::collections::HashMap::new();
                cfg.insert("compression.type".to_string(), "snappy".to_string());
                cfg.insert("cleanup.policy".to_string(), "compact".to_string());
                cfg
            },
        };
        final_metadata_store.create_topic(chronik_common::metadata::METADATA_TOPIC, meta_config).await?;
        info!("Successfully created internal metadata topic");
    }

    final_metadata_store
} else {
    // File-based metadata (legacy mode)
    info!("Initializing file-based metadata store (legacy mode)");
    let metadata_dir = format!("{}/metadata", config.data_dir);
    std::fs::create_dir_all(&metadata_dir)?;

    match FileMetadataStore::new(&metadata_dir).await {
        Ok(store) => {
            info!("Successfully initialized file-based metadata store at {}", metadata_dir);
            Arc::new(store) as Arc<dyn MetadataStore>
        },
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to initialize file-based metadata store: {:?}", e));
        }
    }
};
```

### Step 3: Add Raft Manager Parameter

The `new_internal()` function already accepts `raft_manager` parameter. Ensure it's properly typed:

```rust
async fn new_internal(
    config: IntegratedServerConfig,
    #[cfg(feature = "raft")] raft_manager: Option<Arc<crate::raft_integration::RaftReplicaManager>>,
    #[cfg(not(feature = "raft"))] _raft_manager: Option<()>,
) -> Result<Self>
```

### Step 4: Update Raft Integration Module

In `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-server/src/raft_integration.rs`, ensure `RaftReplicaManager` implements the trait required by `RaftMetaLog`:

```rust
// Add this implementation to RaftReplicaManager
#[cfg(feature = "raft")]
impl chronik_common::metadata::RaftReplicaManager for crate::raft_integration::RaftReplicaManager {
    async fn propose_and_wait(
        &self,
        topic: &str,
        partition: i32,
        data: Vec<u8>,
    ) -> std::result::Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        self.propose(topic, partition, data)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    fn is_leader(&self, topic: &str, partition: i32) -> bool {
        self.is_leader(topic, partition)
    }

    fn get_leader(&self, topic: &str, partition: i32) -> Option<u64> {
        self.get_leader(topic, partition)
    }
}
```

## Testing the Integration

### Unit Test (Standalone Mode)

Create a test in `crates/chronik-server/src/integrated_server.rs`:

```rust
#[tokio::test]
async fn test_standalone_metadata_store() {
    let config = IntegratedServerConfig {
        cluster_config: None, // Standalone mode
        ..Default::default()
    };

    let server = IntegratedKafkaServer::new(config).await.unwrap();

    // Verify metadata store works
    let topics = server.metadata_store.list_topics().await.unwrap();
    assert!(topics.is_empty() || topics.iter().any(|t| t.name == "__meta"));
}
```

### Integration Test (Clustered Mode)

Create `tests/integration/raft_metadata_replication.rs`:

```rust
#[tokio::test]
async fn test_3node_metadata_replication() {
    // Setup 3-node cluster
    // ...

    // Create topic on leader
    let leader = find_leader();
    leader.create_topic("test-topic", config).await.unwrap();

    // Wait for replication
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify topic exists on all followers
    for follower in followers {
        let topic = follower.get_topic("test-topic").await.unwrap();
        assert!(topic.is_some());
    }
}
```

## Verification Steps

### 1. Compile Check
```bash
# Without raft feature (standalone mode)
cargo check --bin chronik-server

# With raft feature (clustered mode)
cargo check --bin chronik-server --features raft
```

### 2. Run Unit Tests
```bash
cargo test --lib --bins
```

### 3. Run Integration Tests
```bash
cargo test --test raft_metadata_replication --features raft
```

### 4. Manual Testing with 3-Node Cluster

Start 3 nodes:
```bash
# Node 1 (leader)
cargo run --bin chronik-server --features raft -- \
  --advertised-addr node1:9092 \
  --raft \
  standalone

# Node 2
cargo run --bin chronik-server --features raft -- \
  --advertised-addr node2:9092 \
  --raft \
  standalone

# Node 3
cargo run --bin chronik-server --features raft -- \
  --advertised-addr node3:9092 \
  --raft \
  standalone
```

Create topic on leader:
```bash
kafka-topics --create \
  --topic test-metadata-replication \
  --partitions 3 \
  --replication-factor 1 \
  --bootstrap-server node1:9092
```

Verify on followers:
```bash
# Check node2
kafka-topics --list --bootstrap-server node2:9092

# Check node3
kafka-topics --list --bootstrap-server node3:9092
```

All three should show `test-metadata-replication`.

## Troubleshooting

### Issue: "Raft manager required for clustered mode"

**Cause**: `cluster_config` is set but `raft_manager` is None.

**Solution**: Ensure `RaftReplicaManager` is created and passed to `new_with_raft()`.

### Issue: "Not leader for metadata group"

**Cause**: Trying to create topic on follower node.

**Solution**:
1. Check which node is leader: Look for "role=Leader" in logs
2. Retry request on leader node
3. Or implement client-side redirection to leader

### Issue: Metadata not replicating

**Cause**: Raft messages not being sent between nodes.

**Solution**:
1. Check Raft gRPC connectivity between nodes
2. Verify peer addresses in cluster config
3. Check firewall rules
4. Enable debug logging: `RUST_LOG=chronik_raft=debug,chronik_common::metadata=debug`

### Issue: Compile errors with RaftReplicaManager trait

**Cause**: Trait implementation mismatch.

**Solution**: Ensure `RaftReplicaManager` in `raft_integration.rs` implements all methods from `chronik_common::metadata::RaftReplicaManager` trait.

## Rollback Plan

If integration fails, rollback steps:

1. **Revert `integrated_server.rs`**:
   ```bash
   git checkout HEAD -- crates/chronik-server/src/integrated_server.rs
   ```

2. **Remove new files**:
   ```bash
   rm crates/chronik-common/src/metadata/raft_meta_log.rs
   rm crates/chronik-common/src/metadata/raft_state_machine.rs
   ```

3. **Revert `mod.rs`**:
   ```bash
   git checkout HEAD -- crates/chronik-common/src/metadata/mod.rs
   ```

4. **Verify standalone mode still works**:
   ```bash
   cargo test --lib --bins
   ```

## Success Criteria

- ✅ Compiles with and without `raft` feature
- ✅ Standalone mode unchanged (no Raft overhead)
- ✅ Clustered mode creates metadata Raft replica
- ✅ Topic creation on leader replicates to followers
- ✅ All existing tests pass
- ✅ Manual 3-node cluster test passes

## Next Steps After Integration

1. **Phase 3.4**: Implement client-side redirection to leader for metadata operations
2. **Phase 4.1**: Test partition assignment replication
3. **Phase 4.2**: Test consumer group state replication
4. **Phase 5**: End-to-end cluster testing with real Kafka clients

## References

- Implementation Plan: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/PHASE3_3_IMPLEMENTATION_PLAN.md`
- Raft Integration: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-server/src/raft_integration.rs`
- Metadata Store Trait: `/Users/lspecian/Development/chronik-stream/.conductor/lahore/crates/chronik-common/src/metadata/traits.rs`
