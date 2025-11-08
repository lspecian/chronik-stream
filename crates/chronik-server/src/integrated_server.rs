//! Integrated Kafka server using chronik-ingest components.
//! 
//! This module properly integrates the complete, production-ready implementation
//! from chronik-ingest instead of reimplementing everything from scratch.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::io::IoSlice;
use tracing::{info, error, debug, warn};
use crate::error_handler::{ErrorHandler, ErrorCode, ErrorRecovery, ServerError};

// Use the local server components (moved from chronik-ingest)
use crate::kafka_handler::KafkaProtocolHandler;
use crate::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use crate::fetch_handler::FetchHandler;
use crate::storage::{StorageConfig as IngestStorageConfig, StorageService};
use crate::wal_integration::WalProduceHandler;

// Storage components
use chronik_storage::{
    ObjectStoreTrait, ObjectStoreConfig, ObjectStoreFactory,
    SegmentReader, SegmentReaderConfig,
    SegmentWriter, SegmentWriterConfig,
    WalIndexer, WalIndexerConfig,
};

// Metadata store
use chronik_common::metadata::traits::MetadataStore;

// Protocol types - BrokerMetadata is in chronik_common
use chronik_common::metadata::traits::BrokerMetadata;

/// Configuration for the integrated Kafka server
#[derive(Clone)]
pub struct IntegratedServerConfig {
    /// Node ID for this broker
    pub node_id: i32,
    /// Hostname to advertise to clients
    pub advertised_host: String,
    /// Port to advertise to clients
    pub advertised_port: i32,
    /// Data directory for storage
    pub data_dir: String,
    /// Enable real-time indexing
    pub enable_indexing: bool,
    /// Enable compression
    pub enable_compression: bool,
    /// Auto-create topics
    pub auto_create_topics: bool,
    /// Default number of partitions
    pub num_partitions: u32,
    /// Default replication factor
    pub replication_factor: u32,
    /// Enable background WAL indexing (WAL → Tantivy → Object Store)
    pub enable_wal_indexing: bool,
    /// WAL indexing interval in seconds
    pub wal_indexing_interval_secs: u64,
    /// Optional object store configuration (overrides default local storage)
    pub object_store_config: Option<ObjectStoreConfig>,
    /// Enable metadata disaster recovery (upload metadata WAL to S3)
    pub enable_metadata_dr: bool,
    /// Metadata upload interval in seconds
    pub metadata_upload_interval_secs: u64,
    /// Optional cluster configuration for Raft clustering
    pub cluster_config: Option<chronik_config::ClusterConfig>,
}

impl Default for IntegratedServerConfig {
    fn default() -> Self {
        Self {
            node_id: 1,  // Changed from 0 to 1 (controller_id of 0 means no controller in Kafka)
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
            data_dir: "./data".to_string(),
            enable_indexing: cfg!(feature = "search"), // Enable when search feature is compiled
            enable_compression: true,
            auto_create_topics: true,
            num_partitions: 3,
            replication_factor: 1,
            enable_wal_indexing: true, // Enable WAL→Tantivy indexing by default
            wal_indexing_interval_secs: 30, // Index every 30 seconds
            object_store_config: None, // Use default local storage unless specified
            enable_metadata_dr: true, // Enable metadata DR by default
            metadata_upload_interval_secs: 60, // Upload metadata every minute
            cluster_config: None, // No clustering by default (standalone mode)
        }
    }
}

/// Integrated Kafka server with full functionality
pub struct IntegratedKafkaServer {
    config: IntegratedServerConfig,
    kafka_handler: Arc<KafkaProtocolHandler>,
    metadata_store: Arc<dyn MetadataStore>,
    wal_indexer: Arc<WalIndexer>,
    metadata_uploader: Option<Arc<chronik_common::metadata::MetadataUploader>>,
    /// v2.5.0 Phase 5: Leader election per partition
    leader_elector: Option<Arc<crate::leader_election::LeaderElector>>,
}

impl Clone for IntegratedKafkaServer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            kafka_handler: self.kafka_handler.clone(),
            metadata_store: self.metadata_store.clone(),
            wal_indexer: self.wal_indexer.clone(),
            metadata_uploader: self.metadata_uploader.clone(),
            leader_elector: self.leader_elector.clone(),
        }
    }
}

impl IntegratedKafkaServer {
    /// Create a new integrated Kafka server
    ///
    /// # Arguments
    /// - `config`: Server configuration
    /// - `raft_cluster`: Optional Raft cluster for metadata coordination (v2.5.0 Phase 3)
    pub async fn new(
        config: IntegratedServerConfig,
        raft_cluster: Option<Arc<crate::raft_cluster::RaftCluster>>,
    ) -> Result<Self> {
        info!("Starting internal server initialization with Raft: {}", raft_cluster.is_some());

        // Create data directory
        std::fs::create_dir_all(&config.data_dir)?;
        let segments_dir = format!("{}/segments", config.data_dir);
        std::fs::create_dir_all(&segments_dir)?;
        
        // v2.2.7 Phase 4: ALWAYS create RaftCluster for metadata (even single-node)
        // Single-node mode uses zero-overhead synchronous apply (<100μs)
        // Multi-node mode uses full Raft consensus (10-50ms)
        info!("Creating RaftCluster for metadata coordination (single-node or multi-node)");

        let raft_cluster_for_metadata = if let Some(ref cluster_cfg) = raft_cluster {
            // Multi-node mode: use existing RaftCluster
            info!("Multi-node mode: using existing RaftCluster with {} peers", cluster_cfg.peer_count());
            cluster_cfg.clone()
        } else {
            // Single-node mode: create RaftCluster with empty peers (zero overhead)
            info!("Single-node mode: creating RaftCluster with zero-overhead synchronous apply");

            let data_dir = PathBuf::from(&config.data_dir);
            Arc::new(crate::raft_cluster::RaftCluster::bootstrap(
                config.node_id as u64,
                Vec::new(),  // Empty peers = single-node mode
                data_dir,
            ).await?)
        };

        // v2.2.7 Phase 4: Create RaftMetadataStore (replaces ChronikMetaLogStore)
        let metadata_store: Arc<dyn MetadataStore> = Arc::new(
            crate::raft_metadata_store::RaftMetadataStore::new(raft_cluster_for_metadata.clone())
        );

        info!("Successfully initialized RaftMetadataStore (v2.2.7 Phase 4)");

        // v2.2.7 Phase 6: System topics no longer needed - metadata lives in Raft state machine
        // The old __meta topic was used by ChronikMetaLogStore, which is now deleted
        // Raft stores metadata in its own WAL (./data/wal/__meta/)
        
        // v2.2.7 Phase 4: Register broker via unified RaftMetadataStore
        // Single-node: synchronous apply (<100μs, no leader election needed)
        // Multi-node: Raft propose (10-50ms, waits for quorum)
        let broker_metadata = BrokerMetadata {
            broker_id: config.node_id,
            host: config.advertised_host.clone(),
            port: config.advertised_port,
            rack: None,
            status: chronik_common::metadata::traits::BrokerStatus::Online,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        info!("Registering broker {} ({}:{}) via RaftMetadataStore...",
              config.node_id, config.advertised_host, config.advertised_port);

        // v2.2.7: RaftMetadataStore.register_broker() automatically handles:
        // - Single-node: synchronous apply to state machine
        // - Multi-node: Raft propose (may fail if no leader elected yet)
        let mut retry_count = 0;
        let max_retries = 30; // 30 attempts * 2s = 60 seconds max wait
        loop {
            match metadata_store.register_broker(broker_metadata.clone()).await {
                Ok(_) => {
                    info!("✓ Successfully registered broker {} via RaftMetadataStore", config.node_id);
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count <= max_retries {
                        warn!("Failed to register broker {} (attempt {}/{}): {:?}, retrying in 2s...",
                              config.node_id, retry_count, max_retries, e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    } else {
                        error!("FATAL: Failed to register broker {} after {} attempts: {:?}",
                               config.node_id, retry_count, e);
                        return Err(anyhow::anyhow!(
                            "Broker registration failed after {} attempts - cluster may not have quorum",
                            max_retries
                        ));
                    }
                }
            }
        }

        // v2.2.7 Phase 4: Broker sync is now handled by Raft directly
        // RaftMetadataStore.register_broker() calls raft.propose() automatically
        // No need for background bidirectional sync task

        // Restore high watermarks from segment metadata on startup
        // This ensures that after WAL deletion, we can still serve data from segments
        info!("Restoring high watermarks from segment metadata...");
        if let Err(e) = Self::restore_high_watermarks_from_segments(&metadata_store).await {
            warn!(error = %e, "Failed to restore high watermarks from segments (continuing anyway)");
        } else {
            info!("Successfully restored high watermarks from segment metadata");
        }

        // Create object store configuration (use custom config if provided, otherwise default to local)
        let object_store_config = if let Some(custom_config) = config.object_store_config.clone() {
            info!("Using custom object store configuration from environment/config");
            custom_config
        } else {
            info!("Using default local filesystem object store at {}", segments_dir);
            ObjectStoreConfig {
                backend: chronik_storage::object_store::StorageBackend::Local {
                    path: segments_dir.clone(),
                },
                bucket: "chronik".to_string(),
                prefix: None,
                connection: Default::default(),
                performance: Default::default(),
                retry: Default::default(),
                auth: chronik_storage::object_store::AuthConfig::None,
                default_metadata: None,
                encryption: None,
            }
        };

        // Create object store
        let object_store = ObjectStoreFactory::create(object_store_config.clone()).await?;
        let object_store_arc: Arc<dyn ObjectStoreTrait> = Arc::from(object_store);
        
        // Create storage service with proper configuration
        let storage_config = IngestStorageConfig {
            object_store_config: object_store_config.clone(),
            segment_writer_config: SegmentWriterConfig {
                data_dir: PathBuf::from(&segments_dir),
                compression_codec: if config.enable_compression { 
                    "snappy".to_string() 
                } else { 
                    "none".to_string() 
                },
                max_segment_size: 256 * 1024 * 1024, // 256MB
                max_segment_age_secs: 30, // 30 seconds - flush on produce for immediate availability
                retention_period_secs: 7 * 24 * 3600, // 7 days  
                enable_cleanup: true,
            },
            segment_reader_config: SegmentReaderConfig::default(),
        };
        
        let storage_service = Arc::new(StorageService::new(storage_config.clone()).await?);
        
        // Initialize segment reader with object store
        let segment_reader = Arc::new(SegmentReader::new(
            SegmentReaderConfig::default(),
            storage_service.object_store(),
        ));
        
        // Configure produce handler with proper indexer config
        let indexer_config = chronik_search::realtime_indexer::RealtimeIndexerConfig {
            index_base_path: PathBuf::from(format!("{}/index", config.data_dir)),
            ..Default::default()
        };
        
        let flush_profile = crate::produce_handler::ProduceFlushProfile::auto_select();
        let produce_config = ProduceHandlerConfig {
            node_id: config.node_id,
            storage_config: storage_config.clone(),
            indexer_config,
            enable_indexing: config.enable_indexing,
            enable_idempotence: true,
            enable_transactions: false, // Start without transactions
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: flush_profile.linger_ms(),
            compression_type: if config.enable_compression {
                chronik_storage::kafka_records::CompressionType::Snappy
            } else {
                chronik_storage::kafka_records::CompressionType::None
            },
            request_timeout_ms: 120000,  // 120 seconds (increased from 30s to handle slow topic auto-creation)
            buffer_memory: flush_profile.buffer_memory(),
            auto_create_topics_enable: config.auto_create_topics,
            num_partitions: config.num_partitions,
            default_replication_factor: config.replication_factor,
            flush_profile,
        };

        // Create WAL configuration with default settings (v1.3.47: moved before ProduceHandler)
        use chronik_wal::{CompressionType, CheckpointConfig, RecoveryConfig, RotationConfig, FsyncConfig, WalConfig, WalManager};
        use chronik_wal::config::AsyncIoConfig;

        let wal_config = WalConfig {
            enabled: true,  // WAL is always enabled now as the default durability mechanism
            data_dir: PathBuf::from(format!("{}/wal", config.data_dir)),
            segment_size: 128 * 1024 * 1024, // 128MB segments
            flush_interval_ms: 100, // Sync to disk every 100ms for durability
            flush_threshold: 1024 * 1024, // 1MB buffer threshold
            compression: CompressionType::None,
            checkpointing: CheckpointConfig::default(),
            recovery: RecoveryConfig::default(),
            rotation: RotationConfig {
                max_segment_age_ms: 30 * 60 * 1000, // 30 minutes
                max_segment_size: 128 * 1024 * 1024, // 128MB
                coordinate_with_storage: true,
            },
            fsync: FsyncConfig {
                enabled: true,
                batch_size: 8,     // Batch up to 8 writes for efficiency
                batch_timeout_ms: 50, // Max 50ms latency for fsync batching
            },
            async_io: AsyncIoConfig::default(),
        };

        // NOTE: Segments are NOW reliable (flush writes to segments on shutdown)
        // Previous logic cleared segments to prevent duplicates, but this also deleted
        // valid flushed data! With flush_partition() now writing to SegmentWriter,
        // segments contain committed data that should NOT be deleted.
        //
        // Recovery strategy:
        // 1. Segments have data from previous flush operations (reliable)
        // 2. WAL has data from produce operations (reliable)
        // 3. Both are needed for complete recovery
        //
        // We DO NOT clear segments on recovery. Consumers will fetch from:
        // - WAL for recent writes (fast)
        // - Segments for older data (persistent)
        //
        // Duplicate prevention is handled by offset tracking in metadata store.
        info!("Skipping segment clearing - segments contain valid flushed data");
        info!("Segments directory will be preserved for recovery: {}", segments_dir);

        // Initialize WAL manager with recovery (v1.3.47+: lock-free WAL architecture)
        // v1.3.66+: Reuse Raft manager's WAL if available (ensures shared sealed_segments DashMap)
        // WalManager uses DashMap internally - no external RwLock needed
        info!("Initializing WAL manager with recovery...");

        // v2.5.0: Always use direct WAL recovery
        let wal_manager_recovered = WalManager::recover(&wal_config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to recover WAL: {}", e))?;
        let wal_manager = Arc::new(wal_manager_recovered);

        info!("WAL recovery complete - {} partitions loaded", wal_manager.get_partitions().len());

        // Initialize produce handler with inline WAL support (v1.3.47)
        let mut produce_handler_inner = ProduceHandler::new_with_wal(
            produce_config,
            object_store_arc.clone(),
            metadata_store.clone(),
            wal_manager.clone(),
        ).await?;

        // v2.5.0 Phase 3: Wire RaftCluster to ProduceHandler for partition metadata
        if let Some(ref cluster) = raft_cluster {
            info!("Setting RaftCluster for ProduceHandler");
            produce_handler_inner.set_raft_cluster(Arc::clone(cluster));
        }

        // v2.5.0 Phase 4: Create ISR ACK tracker for acks=-1 quorum support FIRST
        // CRITICAL: Must be created on ALL nodes (leader + followers) so ACKs can flow bidirectionally
        // AND must be shared with both WalReplicationManager (for ACK reading) and ProduceHandler (for waiting)
        let isr_ack_tracker = crate::isr_ack_tracker::IsrAckTracker::new();
        info!("Created IsrAckTracker for acks=-1 quorum tracking");
        produce_handler_inner.set_isr_ack_tracker(isr_ack_tracker.clone());

        // v2.5.0 Phase 5: Create leader elector if Raft clustering is enabled
        // Must be created BEFORE ProduceHandler so it can record heartbeats
        let leader_elector_for_produce = if let Some(ref raft) = raft_cluster {
            info!("Creating LeaderElector for ProduceHandler heartbeat tracking");
            let elector = Arc::new(crate::leader_election::LeaderElector::new(raft.clone()));

            // Start background monitoring (health checks every 3s, timeout after 10s)
            elector.start_monitoring();

            info!("✓ LeaderElector monitoring started");
            Some(elector)
        } else {
            None
        };

        // Wire leader elector to ProduceHandler for heartbeat recording
        if let Some(ref elector) = leader_elector_for_produce {
            produce_handler_inner.set_leader_elector(elector.clone());
        }

        // v2.5.0 Phase 5: Initialize Raft metadata for existing topics on startup
        // CRITICAL: Without this, restarted servers won't have partition metadata in Raft!
        if let Some(ref raft) = raft_cluster {
            info!("Initializing Raft metadata for existing topics on startup...");

            // CRITICAL: Wait for Raft leader election before proposing metadata
            // Raft requires a leader to accept proposals, so we must wait
            // CRITICAL FIX (v2.2.1): Only the Raft leader should propose metadata
            // Follower nodes must wait and receive metadata via Raft replication
            info!("Waiting for Raft leader election before proposing metadata...");
            let mut leader_ready = false;
            let mut this_node_is_leader = false;
            for attempt in 1..=30 {  // Wait up to 30 seconds
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                // Check if Raft has a leader
                let (is_ready, leader_id, state) = raft.is_leader_ready();

                if is_ready {
                    if leader_id == raft.node_id() {
                        info!("✓ This node is Raft leader (state={}), proceeding with metadata initialization", state);
                        this_node_is_leader = true;
                        leader_ready = true;
                        break;
                    } else {
                        info!("✓ Raft leader elected (leader_id={}, state={}), this node is a follower - waiting for leader to initialize metadata", leader_id, state);
                        leader_ready = true;
                        this_node_is_leader = false;
                        break;
                    }
                }

                if attempt % 5 == 0 {
                    debug!("Still waiting for Raft leader election... ({}s elapsed, state={})", attempt, state);
                }
            }

            if !leader_ready {
                warn!("Raft leader election did not complete within 30 seconds - metadata proposals may fail");
            }

            // CRITICAL FIX (v2.2.1): Only Raft leader proposes partition metadata
            // This prevents "Cannot propose: not the leader" errors and partition leadership conflicts
            if !this_node_is_leader {
                info!("Skipping metadata initialization - this node is a Raft follower (will receive metadata via Raft replication)");
            } else {
                // Only Raft leader initializes partition metadata
                info!("This node is Raft leader - initializing partition metadata for existing topics");

                // Get all existing topics from metadata store
                match metadata_store.list_topics().await {
                    Ok(topics) => {
                        let topic_count = topics.len();
                        info!("Found {} existing topics to initialize in Raft", topic_count);

                        for topic_meta in topics {
                            let topic_name = &topic_meta.name;
                            let partition_count = topic_meta.config.partition_count;

                            info!("Initializing Raft metadata for topic '{}' ({} partitions)", topic_name, partition_count);

                            // Get all nodes in cluster
                            let all_nodes = vec![1_u64, 2_u64, 3_u64];
                            let replication_factor = config.replication_factor.min(all_nodes.len() as u32);

                            for partition in 0..partition_count {
                                // Check if metadata already exists (avoid duplicate proposals)
                                if raft.get_partition_replicas(topic_name, partition as i32).is_some() {
                                    debug!("Raft metadata already exists for {}-{}, skipping", topic_name, partition);
                                    continue;
                                }

                                // Assign replicas (round-robin)
                                let mut replicas = Vec::new();
                                for i in 0..replication_factor {
                                    let node_idx = (partition as usize + i as usize) % all_nodes.len();
                                    replicas.push(all_nodes[node_idx]);
                                }

                                // Propose partition assignment
                                if let Err(e) = raft.propose(crate::raft_metadata::MetadataCommand::AssignPartition {
                                    topic: topic_name.to_string(),
                                    partition: partition as i32,
                                    replicas: replicas.clone(),
                                }).await {
                                    warn!("Failed to propose partition assignment for {}-{}: {:?}", topic_name, partition, e);
                                    continue;
                                }

                                // Set initial leader - prefer Raft leader if it's a replica
                                // CRITICAL FIX (v2.2.3): Prefer Raft leader as partition leader
                                // This ensures partition leaders can handle their own elections locally
                                let raft_leader_id = raft.node_id();
                                let leader = if replicas.contains(&raft_leader_id) {
                                    raft_leader_id  // Raft leader can handle elections locally
                                } else {
                                    replicas[0]  // Fallback if Raft leader not in replica set
                                };
                                if let Err(e) = raft.propose(crate::raft_metadata::MetadataCommand::SetPartitionLeader {
                                    topic: topic_name.to_string(),
                                    partition: partition as i32,
                                    leader,
                                }).await {
                                    warn!("Failed to propose partition leader for {}-{}: {:?}", topic_name, partition, e);
                                }

                                // Set initial ISR (all replicas in-sync initially)
                                if let Err(e) = raft.propose(crate::raft_metadata::MetadataCommand::UpdateISR {
                                    topic: topic_name.to_string(),
                                    partition: partition as i32,
                                    isr: replicas.clone(),
                                }).await {
                                    warn!("Failed to propose ISR for {}-{}: {:?}", topic_name, partition, e);
                                }

                                info!("✓ Proposed Raft metadata for {}-{}: replicas={:?}, leader={}, ISR={:?}",
                                      topic_name, partition, replicas, leader, replicas);
                            }
                        }

                        info!("✓ Completed Raft metadata initialization for {} existing topics", topic_count);
                    }
                    Err(e) => {
                        warn!("Failed to list topics for Raft metadata initialization: {:?}", e);
                    }
                }
            }
        }

        // v2.5.0 Phase 6: Initialize WAL replication with auto-discovery from cluster config
        let manual_followers = if let Ok(followers_str) = std::env::var("CHRONIK_REPLICATION_FOLLOWERS") {
            if !followers_str.is_empty() {
                followers_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Enable WAL replication if either manual followers or cluster config is available
        if !manual_followers.is_empty() || config.cluster_config.is_some() {
            // v2.5.0 Phase 3: Create ISR tracker
            let isr_tracker = Arc::new(crate::isr_tracker::IsrTracker::new(
                10_000,  // max_lag_entries: 10K messages
                10_000,  // max_lag_ms: 10 seconds
            ));
            info!("Created ISR tracker (max_lag: 10K entries / 10s)");

            // v2.5.0 Phase 6: Create replication manager with cluster config for auto-discovery
            // CRITICAL: Use the SAME isr_ack_tracker instance that ProduceHandler uses
            let replication_manager = crate::wal_replication::WalReplicationManager::new_with_dependencies(
                manual_followers,
                raft_cluster.clone(),              // Pass RaftCluster for partition metadata
                Some(isr_tracker),                 // Pass ISR tracker for replica filtering
                Some(isr_ack_tracker.clone()),     // v2.5.0 Phase 4: Pass SAME ACK tracker
                config.cluster_config.clone().map(Arc::new), // v2.5.0 Phase 6: Pass cluster config for auto-discovery
            );
            info!("Created WalReplicationManager with Raft, ISR, and ACK tracking (sharing IsrAckTracker)");

            produce_handler_inner.set_wal_replication_manager(replication_manager);
        } else {
            info!("WAL replication disabled: no cluster config and CHRONIK_REPLICATION_FOLLOWERS not set");
        }

        let produce_handler_base = Arc::new(produce_handler_inner);

        // CRITICAL FIX (v1.3.52): Clear all partition buffers before WAL recovery
        // This prevents duplicate messages by ensuring we start with clean in-memory state.
        // Without this, WAL replay adds recovered data on top of any existing buffers,
        // causing 140%+ message recovery (e.g., 7000/5000 messages).
        info!("Clearing all partition buffers before WAL recovery...");
        if let Err(e) = produce_handler_base.clear_all_buffers().await {
            warn!("Failed to clear partition buffers: {}. Recovery may have duplicates.", e);
        }

        // CRITICAL (v1.3.48): Replay WAL to restore high watermarks after crash
        // Without this, all partitions have high_watermark=0 and consumers get no data
        let recovered_partitions = wal_manager.get_partitions();
        if !recovered_partitions.is_empty() {
            info!("Replaying WAL to restore high watermarks for {} partitions...", recovered_partitions.len());

            for tp in recovered_partitions {
                // Read all WAL records to find highest offset
                match wal_manager.read_from(&tp.topic, tp.partition, 0, usize::MAX).await {
                    Ok(records) if !records.is_empty() => {
                        use chronik_wal::record::WalRecord;
                        use chronik_storage::CanonicalRecord;

                        let mut max_offset: i64 = -1;

                        // Find max offset across all records (handle both V1 and V2 formats)
                        for record in &records {
                            match record {
                                WalRecord::V1 { offset, .. } => {
                                    if *offset > max_offset {
                                        max_offset = *offset;
                                    }
                                },
                                WalRecord::V2 { canonical_data, .. } => {
                                    // Deserialize CanonicalRecord to get offsets
                                    if let Ok(canonical) = bincode::deserialize::<CanonicalRecord>(canonical_data) {
                                        let last_offset = canonical.last_offset();
                                        if last_offset > max_offset {
                                            max_offset = last_offset;
                                        }
                                    }
                                }
                            }
                        }

                        if max_offset >= 0 {
                            // High watermark is max_offset + 1 (next offset to be written)
                            let high_watermark = (max_offset + 1) as u64;

                            // Restore partition state
                            if let Err(e) = produce_handler_base.restore_partition_state(&tp.topic, tp.partition, high_watermark).await {
                                warn!("Failed to restore partition state for {}-{}: {}", tp.topic, tp.partition, e);
                            } else {
                                info!("Restored {}-{}: {} records, high watermark = {}",
                                      tp.topic, tp.partition, records.len(), high_watermark);
                            }
                        }
                    },
                    Ok(_) => {
                        // No records, nothing to restore
                    },
                    Err(e) => {
                        warn!("Failed to read WAL for {}-{}: {}", tp.topic, tp.partition, e);
                    }
                }
            }

            info!("WAL replay complete");
        } else {
            // CRITICAL FIX: WAL is empty (e.g., after deletion or fresh start with persisted segments)
            // Restore partition states from metadata store instead
            info!("WAL is empty, attempting to restore partition states from metadata store...");

            // List all topics from metadata
            match metadata_store.list_topics().await {
                Ok(topics) => {
                    for topic_meta in topics {
                        let partition_count = topic_meta.config.partition_count as i32;
                        for partition_id in 0..partition_count {
                            // Get high watermark from metadata store
                            match metadata_store.get_partition_offset(&topic_meta.name, partition_id as u32).await {
                                Ok(Some((high_watermark, _log_start_offset))) if high_watermark > 0 => {
                                    // Restore partition state
                                    if let Err(e) = produce_handler_base.restore_partition_state(&topic_meta.name, partition_id, high_watermark as u64).await {
                                        warn!("Failed to restore partition state from metadata for {}-{}: {}", topic_meta.name, partition_id, e);
                                    } else {
                                        info!("Restored {}-{} from metadata store: high watermark = {}", topic_meta.name, partition_id, high_watermark);
                                    }
                                }
                                Ok(_) => {
                                    // No offset data or hwm=0, nothing to restore
                                }
                                Err(e) => {
                                    warn!("Failed to get partition offset from metadata for {}-{}: {}", topic_meta.name, partition_id, e);
                                }
                            }
                        }
                    }
                    info!("Metadata store recovery complete");
                }
                Err(e) => {
                    warn!("Failed to list topics from metadata store: {}", e);
                }
            }
        }

        // Wrap with WalProduceHandler for backward compatibility (will be removed in future)
        let wal_handler = Arc::new(WalProduceHandler::new_passthrough(
            wal_manager.clone(),
            produce_handler_base.clone()
        ));

        // WAL manager reference for FetchHandler
        let wal_manager_ref = wal_manager.clone();

        // Create FetchHandler with WAL and ProduceHandler integration (v1.3.39+)
        // FetchHandler needs ProduceHandler to get the real-time high watermark
        let fetch_handler = Arc::new(FetchHandler::new_with_wal(
            segment_reader.clone(),
            metadata_store.clone(),
            object_store_arc.clone(),
            wal_manager_ref,
            produce_handler_base.clone(),
        ));

        // Start background tasks for segment rotation on the base handler
        produce_handler_base.start_background_tasks().await;
        
        // Initialize Kafka protocol handler with all components
        // WAL is MANDATORY - no longer optional
        let kafka_handler = Arc::new(KafkaProtocolHandler::new(
            produce_handler_base.clone(),
            segment_reader,
            metadata_store.clone(),
            object_store_arc.clone(),
            fetch_handler.clone(),
            wal_handler.clone(),  // WAL is mandatory, not optional
            config.node_id,
            config.advertised_host.clone(),
            config.advertised_port,
            config.num_partitions,  // Pass default partition count from config
        ).await?);

        // Initialize WAL Indexer (always enabled for search integration)
        info!("Initializing WAL Indexer for background indexing (WAL → Tantivy → Object Store)");

        let indexer_config = WalIndexerConfig {
            interval_secs: config.wal_indexing_interval_secs,
            min_segment_age_secs: 10,
            max_segments_per_run: 100,
            delete_after_index: true,
            object_store: object_store_config.clone(),
            index_base_path: format!("{}/tantivy_indexes", config.data_dir),
            parallel_indexing: false, // Start with serial processing
            max_concurrent_tasks: 4,
            segment_index_path: Some(std::path::PathBuf::from(format!("{}/segment_index.json", config.data_dir))),
            segment_index_auto_save: true,
        };

        // Get WAL manager from wal_handler
        let wal_manager_ref = wal_handler.wal_manager().clone();

        let wal_indexer = Arc::new(WalIndexer::new(
            indexer_config,
            wal_manager_ref,
            object_store_arc.clone(),
            metadata_store.clone(),
        ));

        // Start the background indexing task
        wal_indexer.start().await
            .map_err(|e| anyhow::anyhow!("Failed to start WAL indexer: {}", e))?;

        info!("WAL Indexer started successfully (interval: {}s)", config.wal_indexing_interval_secs);
        info!("  Segment index will track Tantivy segments for future query optimization");

        // Initialize Metadata Uploader for disaster recovery
        // IMPORTANT: Only enable for remote object stores (S3/GCS/Azure)
        // Local filesystem backend provides no DR benefit (same disk)
        let is_remote_object_store = matches!(
            object_store_config.backend,
            chronik_storage::object_store::StorageBackend::S3 { .. } |
            chronik_storage::object_store::StorageBackend::Gcs { .. } |
            chronik_storage::object_store::StorageBackend::Azure { .. }
        );

        let metadata_uploader = if config.enable_metadata_dr && is_remote_object_store {
            info!("Initializing Metadata Uploader for disaster recovery (metadata WAL → Object Store)");

            let uploader_config = chronik_common::metadata::MetadataUploaderConfig {
                upload_interval_secs: config.metadata_upload_interval_secs,
                wal_base_path: "metadata-wal".to_string(),
                snapshot_base_path: "metadata-snapshots".to_string(),
                delete_after_upload: false, // Keep local WAL for redundancy
                delete_old_snapshots: true,
                keep_local_snapshot_count: 2,
                enabled: true,
            };

            let uploader = Arc::new(crate::metadata_dr::create_metadata_uploader(
                object_store_arc.clone(),
                &config.data_dir,
                uploader_config,
            ));

            // Start the background upload task
            uploader.start().await
                .map_err(|e| anyhow::anyhow!("Failed to start metadata uploader: {}", e))?;

            info!("Metadata Uploader started successfully (interval: {}s)", config.metadata_upload_interval_secs);
            info!("  Metadata WAL and snapshots will be uploaded to object store for disaster recovery");

            Some(uploader)
        } else {
            if !is_remote_object_store {
                info!("Metadata Uploader disabled (local filesystem backend - no DR benefit)");
                info!("  Local metadata already persists to disk and survives restarts");
                info!("  For true DR, configure S3/GCS/Azure object store");
            } else {
                info!("Metadata Uploader disabled by configuration");
            }
            None
        };

        info!("Integrated Kafka server initialized successfully");
        info!("  Node ID: {}", config.node_id);
        info!("  Advertised: {}:{}", config.advertised_host, config.advertised_port);
        info!("  Data dir: {}", config.data_dir);
        info!("  Auto-create topics: {}", config.auto_create_topics);
        info!("  Compression: {}", config.enable_compression);
        info!("  Indexing: {}", config.enable_indexing);
        info!("  WAL Indexing: {}", config.enable_wal_indexing);

        // Clone cluster config and node_id before moving config into server
        let cluster_config_clone = config.cluster_config.clone();
        let node_id = config.node_id;

        // v2.5.0 Phase 6 FIX: Extract WAL receiver address BEFORE moving config
        let wal_receiver_addr = if let Some(ref cluster_cfg) = config.cluster_config {
            // Priority 1: Use cluster config bind.wal address (v2.5.0+)
            cluster_cfg.bind.as_ref().map(|b| b.wal.clone())
        } else {
            // Priority 2: Fall back to env var (backward compatibility)
            std::env::var("CHRONIK_WAL_RECEIVER_ADDR").ok()
        };

        // Create default topic on startup to ensure clients can connect
        // This solves the chicken-and-egg problem where clients need at least one topic
        // in metadata responses before they can produce messages
        let server = Self {
            config,
            kafka_handler,
            metadata_store: metadata_store.clone(),
            wal_indexer,
            metadata_uploader,
            leader_elector: leader_elector_for_produce,
        };

        // Create default topic
        info!("Creating default topic 'chronik-default' for client compatibility");
        if let Err(e) = server.ensure_default_topic().await {
            warn!("Failed to create default topic on startup: {:?}", e);
        }

        // If clustering is enabled, perform initial partition assignment
        if let Some(ref cluster_config) = cluster_config_clone {
            if cluster_config.enabled {
                info!("Cluster mode enabled - performing initial partition assignment");
                if let Err(e) = server.assign_existing_partitions(cluster_config).await {
                    warn!("Failed to assign existing partitions: {:?}", e);
                }

                // CRITICAL FIX (v1.3.66): Verify broker is visible in metadata before accepting connections
                // In Raft cluster mode, allow extra time for consensus and replication
                info!("Verifying broker {} is visible in metadata store (Raft cluster mode)...", server.config.node_id);
                let mut verify_retry = 0;
                let max_verify_retries = 60; // 60 seconds for Raft cluster to stabilize
                loop {
                    match metadata_store.list_brokers().await {
                        Ok(brokers) if !brokers.is_empty() => {
                            let broker_ids: Vec<i32> = brokers.iter().map(|b| b.broker_id).collect();
                            info!("✓ Metadata store has {} broker(s): {:?}", brokers.len(), broker_ids);

                            // Verify THIS broker is in the list
                            if brokers.iter().any(|b| b.broker_id == server.config.node_id) {
                                info!("✓ Broker {} confirmed visible in metadata", server.config.node_id);
                                break;
                            } else {
                                warn!("Broker {} not yet visible in metadata (found: {:?}), retrying...",
                                      server.config.node_id, broker_ids);
                            }
                        }
                        Ok(_) => {
                            warn!("Metadata store has no brokers yet (attempt {}/{}), retrying in 1s...", verify_retry + 1, max_verify_retries);
                        }
                        Err(e) => {
                            warn!("Failed to list brokers (attempt {}/{}): {:?}, retrying in 1s...", verify_retry + 1, max_verify_retries, e);
                        }
                    }

                    verify_retry += 1;
                    if verify_retry >= max_verify_retries {
                        return Err(anyhow::anyhow!(
                            "Broker {} not visible in metadata after {} attempts ({} seconds) - cluster metadata inconsistent",
                            server.config.node_id, verify_retry, verify_retry
                        ));
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        }

        // v2.2.0: Start WAL receiver if enabled (follower mode)
        // v2.5.0 Phase 6 FIX: WAL receiver address already extracted before config move
        if let Some(receiver_addr) = wal_receiver_addr {
            if !receiver_addr.is_empty() {
                info!("WAL receiver enabled on {}", receiver_addr);

                // v2.5.0 Phase 4: Pass IsrAckTracker to WalReceiver so it can send ACKs back to leader
                let wal_receiver = crate::wal_replication::WalReceiver::new_with_isr_tracker(
                    receiver_addr.clone(),
                    wal_manager.clone(),
                    isr_ack_tracker.clone(),
                    node_id as u64,
                );
                info!("WAL receiver configured with IsrAckTracker (node_id={})", node_id);

                // Spawn receiver in background
                tokio::spawn(async move {
                    if let Err(e) = wal_receiver.run().await {
                        error!("WAL receiver failed: {}", e);
                    }
                });

                info!("✅ WAL receiver started on {}", receiver_addr);
            }
        }

        Ok(server)
    }

    /// Get reference to the WAL indexer for search integration
    pub fn get_wal_indexer(&self) -> Arc<WalIndexer> {
        self.wal_indexer.clone()
    }


    /// Assign existing partitions to cluster nodes (Phase 3.4)
    ///
    /// This method is called on cluster startup to ensure all partitions have assignments.
    /// It uses the round-robin strategy from `crates/chronik-common/src/partition_assignment.rs`.
    async fn assign_existing_partitions(&self, cluster_config: &chronik_config::ClusterConfig) -> Result<()> {
        use chronik_common::partition_assignment::PartitionAssignment as AssignmentManager;
        use chronik_common::metadata::PartitionAssignment;

        info!("Starting partition assignment for existing topics");

        // Get list of all cluster node IDs (convert from u64 to u32 for partition_assignment module)
        let node_ids: Vec<u32> = cluster_config.peers.iter().map(|p| p.id as u32).collect();
        if node_ids.is_empty() {
            warn!("No cluster nodes found, skipping partition assignment");
            return Ok(());
        }

        info!("Cluster nodes: {:?}", node_ids);

        // Get all topics
        let topics = self.metadata_store.list_topics().await?;
        if topics.is_empty() {
            info!("No topics found, skipping partition assignment");
            return Ok(());
        }

        info!("Found {} topics to assign", topics.len());

        // For each topic, assign partitions using round-robin
        for topic in topics {
            let topic_name = &topic.name;
            let partition_count = topic.config.partition_count;
            let replication_factor = topic.config.replication_factor.min(node_ids.len() as u32);

            info!(
                "Assigning topic '{}': {} partitions, RF={}",
                topic_name, partition_count, replication_factor
            );

            // Check if partitions are already assigned
            let existing_assignments = self.metadata_store.get_partition_assignments(topic_name).await?;
            if !existing_assignments.is_empty() {
                info!("Topic '{}' already has {} assignments, skipping", topic_name, existing_assignments.len());
                continue;
            }

            // Create assignment manager and assign partitions
            let mut assignment_mgr = AssignmentManager::new();
            assignment_mgr.add_topic(
                topic_name,
                partition_count as i32,
                replication_factor as i32,
                &node_ids,
            )?;

            // Convert to metadata PartitionAssignment and persist
            let topic_assignments = assignment_mgr.get_topic_assignments(topic_name);
            for (partition_id, partition_info) in topic_assignments {
                // For each replica, create a PartitionAssignment
                for (replica_idx, &node_id) in partition_info.replicas.iter().enumerate() {
                    let is_leader = node_id == partition_info.leader;
                    let assignment = PartitionAssignment {
                        topic: topic_name.clone(),
                        partition: partition_id as u32,
                        broker_id: node_id as i32,
                        is_leader,
                    };

                    // Persist assignment via metadata store (will replicate via Raft)
                    self.metadata_store.assign_partition(assignment).await?;

                    if is_leader {
                        info!(
                            "Assigned partition {}/{} to node {} (leader)",
                            topic_name, partition_id, node_id
                        );
                    } else {
                        debug!(
                            "Assigned partition {}/{} to node {} (replica #{})",
                            topic_name, partition_id, node_id, replica_idx
                        );
                    }
                }
            }

            info!(
                "Successfully assigned {} partitions for topic '{}'",
                partition_count, topic_name
            );
        }

        info!("Partition assignment complete");
        Ok(())
    }

    /// Ensure a default topic exists for client connectivity
    async fn ensure_default_topic(&self) -> Result<()> {
        use chronik_common::metadata::TopicConfig;
        
        // Check if any topics exist
        let existing_topics = self.metadata_store.list_topics().await?;
        
        if existing_topics.is_empty() {
            info!("No topics exist, creating default topic 'chronik-default'");
            
            // Create topic config
            let topic_config = TopicConfig {
                partition_count: 1,
                replication_factor: 1,
                retention_ms: Some(604800000), // 7 days
                segment_bytes: 1073741824, // 1GB
                config: Default::default(),
            };
            
            self.metadata_store.create_topic("chronik-default", topic_config).await?;
            
            info!("Successfully created default topic 'chronik-default'");
        } else {
            info!("Topics already exist ({}), skipping default topic creation", existing_topics.len());
        }
        
        Ok(())
    }

    /// Gracefully shutdown the server, flushing all pending data
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down IntegratedKafkaServer...");

        // Step 1: Shutdown WAL handler first to seal all WAL segments to disk
        self.kafka_handler.get_wal_handler().shutdown().await;
        info!("WAL segments sealed to disk");

        // Step 2: Run WalIndexer once to upload sealed segments to object store
        // This ensures data is available for consumption even after WAL deletion
        info!("Running WalIndexer to upload sealed segments...");
        match self.get_wal_indexer().run_once().await {
            Ok(stats) => {
                info!(
                    "WalIndexer run complete: {} segments processed, {} records indexed",
                    stats.segments_processed, stats.records_indexed
                );
            }
            Err(e) => {
                error!("WalIndexer run failed: {}", e);
            }
        }

        info!("IntegratedKafkaServer shutdown complete");
        Ok(())
    }

    /// Get the metadata store (for updating RaftReplicaManager after initialization)
    pub fn metadata_store(&self) -> Arc<dyn MetadataStore> {
        self.metadata_store.clone()
    }

    /// Run the Kafka server (with optional shutdown signal)
    pub async fn run(&self, bind_addr: &str) -> Result<()> {
        let listener = TcpListener::bind(bind_addr).await?;
        info!("Integrated Kafka server listening on {}", bind_addr);
        info!("Ready to accept Kafka client connections");

        // CRITICAL FIX (v1.3.56): Limit concurrent requests to prevent task overload
        // With acks=0, clients can send faster than server can process, causing
        // tokio runtime to spawn thousands of tasks that never get scheduled.
        // Semaphore provides backpressure at TCP level.
        let max_concurrent_requests = 1000; // Kafka default is 500-1000
        let request_semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent_requests));
        info!("Request concurrency limited to {} (prevents acks=0 overload)", max_concurrent_requests);

        loop {
            match listener.accept().await {
                Ok((mut socket, addr)) => {
                    debug!("New connection from {}", addr);

                    // Enable TCP_NODELAY to disable Nagle's algorithm for immediate sending
                    if let Err(e) = socket.set_nodelay(true) {
                        error!("Failed to set TCP_NODELAY for {}: {}", addr, e);
                    }

                    let kafka_handler = self.kafka_handler.clone();
                    let error_handler = Arc::new(ErrorHandler::new());
                    let semaphore = request_semaphore.clone();

                    // CRITICAL FIX (v1.3.60): Channel-based concurrent request processing with response ordering
                    // Split socket into read/write halves for concurrent operation
                    let (socket_reader, mut socket_writer) = socket.into_split();

                    // Elastic response channel capacity for burst traffic handling
                    // 10x semaphore limit provides buffer for burst traffic when handlers complete simultaneously
                    // With 1000 concurrent handlers + 100ms WAL batch window (ultra profile),
                    // handlers complete together and need elastic response buffering
                    // Channel now carries (sequence_number, correlation_id, response_data)
                    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<(u64, i32, Vec<u8>)>(10_000);

                    // Spawn response writer task with ordering guarantee
                    // CRITICAL: Responses MUST be sent in request order (Kafka protocol requirement)
                    tokio::spawn(async move {
                        use std::collections::BTreeMap;

                        // Buffer for out-of-order responses
                        let mut pending_responses: BTreeMap<u64, (i32, Vec<u8>)> = BTreeMap::new();
                        let mut next_sequence: u64 = 0;

                        while let Some((sequence, correlation_id, response_data)) = response_rx.recv().await {
                            // Add response to buffer
                            pending_responses.insert(sequence, (correlation_id, response_data.clone()));

                            tracing::info!(
                                "[RESPONSE PIPELINE] Step 3: Response writer received response, sequence={}, correlation_id={}, size={} bytes, pending_count={}",
                                sequence, correlation_id, response_data.len(), pending_responses.len()
                            );

                            // Send all consecutive responses starting from next_sequence
                            while let Some((corr_id, resp_data)) = pending_responses.remove(&next_sequence) {
                                tracing::info!(
                                    "[RESPONSE PIPELINE] Step 4: Writing response to socket, sequence={}, correlation_id={}, size={} bytes",
                                    next_sequence, corr_id, resp_data.len()
                                );

                                // Write response to socket
                                if let Err(e) = socket_writer.write_all(&resp_data).await {
                                    error!("Failed to write response for sequence={}, correlation_id={}: {}",
                                           next_sequence, corr_id, e);
                                    return;
                                }
                                if let Err(e) = socket_writer.flush().await {
                                    error!("Failed to flush response for sequence={}, correlation_id={}: {}",
                                           next_sequence, corr_id, e);
                                    return;
                                }

                                tracing::info!("[RESPONSE PIPELINE] Step 5: Response sent and flushed successfully, sequence={}, correlation_id={}", next_sequence, corr_id);
                                next_sequence += 1;
                            }
                        }
                    });

                    // Spawn a task to handle this connection with proper error handling
                    tokio::spawn(async move {
                        let mut request_buffer = vec![0; 65536];
                        let mut socket_reader = socket_reader;
                        let mut request_sequence: u64 = 0; // Sequence number for request ordering

                        loop {
                            // Read the size header (4 bytes)
                            let mut size_buf = [0u8; 4];
                            match socket_reader.read_exact(&mut size_buf).await {
                                Ok(_) => {},
                                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                                    debug!("Connection closed by {}", addr);
                                    break;
                                }
                                Err(e) => {
                                    crate::error_handler::handle_connection_error(e, addr).await;
                                    break;
                                }
                            }
                            
                            let request_size = i32::from_be_bytes(size_buf) as usize;

                            // Check for suspicious size values that might indicate protocol mismatch
                            // Common issue: "5.0\0" (0x35, 0x2e, 0x30, 0x00) = 892219392 bytes
                            if request_size > 100_000_000 {
                                // This is likely not a Kafka request - might be version string or other protocol
                                let size_str = String::from_utf8_lossy(&size_buf);
                                warn!("Suspicious request size {} ({} bytes) from {} - might be protocol mismatch. Bytes as string: '{}', hex: {:02x?}",
                                      request_size, request_size, addr, size_str, size_buf);

                                // Try to recover by consuming any remaining data and continuing
                                // Read and discard up to 1KB of data to clear the buffer
                                let mut discard_buf = vec![0u8; 1024];
                                let _ = socket_reader.read(&mut discard_buf).await;

                                // Continue to next request instead of breaking connection
                                continue;
                            }

                            if request_size == 0 || request_size > 10_000_000 {
                                error!("Invalid request size {} from {} (bytes: {:02x?})", request_size, addr, size_buf);

                                // Try to recover instead of breaking connection
                                // Clear the socket buffer and continue
                                let mut discard_buf = vec![0u8; 1024];
                                while let Ok(n) = socket_reader.read(&mut discard_buf).await {
                                    if n == 0 { break; }
                                }

                                // Continue to next request
                                continue;
                            }

                            // Resize buffer if needed
                            if request_buffer.len() < request_size {
                                request_buffer.resize(request_size, 0);
                            }

                            // Read the request body
                            match socket_reader.read_exact(&mut request_buffer[..request_size]).await {
                                Ok(_) => {},
                                Err(e) => {
                                    error!("Error reading request body from {}: {}", addr, e);
                                    break;
                                }
                            }

                            // Debug logging for request identification
                            if request_size >= 8 {
                                let api_key = i16::from_be_bytes([request_buffer[0], request_buffer[1]]);
                                let api_version = i16::from_be_bytes([request_buffer[2], request_buffer[3]]);
                                tracing::debug!(
                                    "Received request: api_key={}, api_version={}, size={} bytes",
                                    api_key, api_version, request_size
                                );

                                if api_key == 0 {
                                    tracing::debug!("Produce request detected: version={}, size={} bytes", api_version, request_size);
                                }
                            }

                            // Parse correlation ID from request for error handling
                            // Kafka protocol: API key (2), API version (2), correlation ID (4)
                            let correlation_id = if request_size >= 8 {
                                i32::from_be_bytes([
                                    request_buffer[4],
                                    request_buffer[5],
                                    request_buffer[6],
                                    request_buffer[7]
                                ])
                            } else {
                                0
                            };

                            // CRITICAL FIX (v1.3.60): Spawn request handler as separate task with sequence number
                            // This enables concurrent request processing while preserving response order
                            // Copy request data and sequence number, then spawn handler
                            let request_data = request_buffer[..request_size].to_vec();
                            let handler_clone = kafka_handler.clone();
                            let response_sender = response_tx.clone();
                            let semaphore_clone = semaphore.clone();
                            let error_handler_clone = error_handler.clone();
                            let addr_clone = addr;
                            let sequence = request_sequence; // Capture sequence number for this request

                            // Increment sequence for next request
                            request_sequence += 1;

                            tokio::spawn(async move {
                                // Acquire semaphore to limit concurrent handlers
                                let _permit = match semaphore_clone.acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => {
                                        error!("Failed to acquire semaphore for request");
                                        return;
                                    }
                                };

                                // Handle request using the integrated handler
                                match handler_clone.handle_request(&request_data).await {
                                Ok(response) => {
                                    // Build complete response with size header
                                    let mut header_bytes = Vec::new();
                                    header_bytes.extend_from_slice(&response.header.correlation_id.to_be_bytes());

                                    // TODO(v2.5.0): CRITICAL BUG - OffsetCommit v8 flexible protocol issue
                                    // Current: Only adds tagged fields for non-ApiVersions flexible responses
                                    // Bug: Consumers get "Protocol read buffer underflow" for OffsetCommit v8
                                    // Need to research correct format from KIP-482 and working APIs
                                    let tagged_byte_added = if response.is_flexible {
                                        if response.api_key != chronik_protocol::parser::ApiKey::ApiVersions {
                                            header_bytes.push(0);
                                            true
                                        } else {
                                            false
                                        }
                                    } else {
                                        false
                                    };

                                    tracing::warn!(
                                        "[RESPONSE DEBUG] API {:?}: is_flexible={}, tagged_byte_added={}, header_bytes_len={}, body_len={}",
                                        response.api_key, response.is_flexible, tagged_byte_added, header_bytes.len(), response.body.len()
                                    );

                                    let mut full_response = Vec::with_capacity(header_bytes.len() + response.body.len() + 4);
                                    let size = (header_bytes.len() + response.body.len()) as i32;
                                    full_response.extend_from_slice(&size.to_be_bytes());
                                    full_response.extend_from_slice(&header_bytes);
                                    full_response.extend_from_slice(&response.body);

                                    // DETAILED LOGGING FOR DEBUGGING
                                    tracing::info!(
                                        "[RESPONSE PIPELINE] Step 1: Built response for API {:?}, correlation_id={}, sequence={}, total_size={} bytes (header={}, body={})",
                                        response.api_key, response.header.correlation_id, sequence, full_response.len(), header_bytes.len(), response.body.len()
                                    );

                                    // CRITICAL DEBUGGING: Log full OffsetCommit response bytes
                                    if matches!(response.api_key, chronik_protocol::parser::ApiKey::OffsetCommit) {
                                        tracing::error!(
                                            "!!! OFFSETCOMMIT FULL RESPONSE (all {} bytes): {:02x?}",
                                            full_response.len(), full_response
                                        );
                                        // Verify structure
                                        if full_response.len() >= 12 {
                                            let msg_size = i32::from_be_bytes([full_response[0], full_response[1], full_response[2], full_response[3]]);
                                            let corr_id = i32::from_be_bytes([full_response[4], full_response[5], full_response[6], full_response[7]]);
                                            let throttle = i32::from_be_bytes([full_response[8], full_response[9], full_response[10], full_response[11]]);
                                            tracing::error!(
                                                "!!! OFFSETCOMMIT STRUCTURE: msg_size={}, correlation_id={}, throttle_time={}",
                                                msg_size, corr_id, throttle
                                            );
                                            if full_response.len() >= 16 {
                                                let topics_len = i32::from_be_bytes([full_response[12], full_response[13], full_response[14], full_response[15]]);
                                                tracing::error!("!!! OFFSETCOMMIT topics_array_len={}", topics_len);
                                            } else {
                                                tracing::error!("!!! OFFSETCOMMIT ERROR: Response too short to include topics array! Only {} bytes", full_response.len());
                                            }
                                        } else {
                                            tracing::error!("!!! OFFSETCOMMIT ERROR: Full response too short! Only {} bytes", full_response.len());
                                        }
                                    }

                                    // Measure channel send delay to detect backpressure
                                    let send_start = std::time::Instant::now();
                                    // Send response with sequence number for ordering
                                    if let Err(e) = response_sender.send((sequence, response.header.correlation_id, full_response.clone())).await {
                                        error!("Failed to send response to writer for addr={}: {}", addr_clone, e);
                                    } else {
                                        tracing::info!(
                                            "[RESPONSE PIPELINE] Step 2: Sent to channel for API {:?}, correlation_id={}, sequence={}",
                                            response.api_key, response.header.correlation_id, sequence
                                        );
                                    }
                                    let send_duration = send_start.elapsed();
                                    if send_duration.as_millis() > 10 {
                                        warn!("Response channel send took {}ms (sequence={}, correlation_id={}) - channel backpressure detected",
                                              send_duration.as_millis(), sequence, response.header.correlation_id);
                                    }
                                }
                                Err(e) => {
                                    // Convert to ServerError for proper handling
                                    let server_error = ErrorHandler::from_anyhow(anyhow::anyhow!(e));
                                    let recovery = error_handler_clone.handle_error(
                                        server_error,
                                        &format!("request from {}", addr_clone)
                                    ).await;

                                    match recovery {
                                        ErrorRecovery::ReturnError(error_code) => {
                                            // Build proper error response with preserved correlation ID
                                            let error_response = error_handler_clone.build_error_response(
                                                error_code,
                                                correlation_id,
                                                0, // Unknown API key
                                                0, // Unknown API version
                                            );

                                            // Measure channel send delay for error responses too
                                            let send_start = std::time::Instant::now();
                                            // Send error response with sequence number for ordering
                                            if let Err(e) = response_sender.send((sequence, correlation_id, error_response)).await {
                                                error!("Failed to send error response: {}", e);
                                            }
                                            let send_duration = send_start.elapsed();
                                            if send_duration.as_millis() > 10 {
                                                warn!("Error response channel send took {}ms (correlation_id={}) - channel backpressure detected",
                                                      send_duration.as_millis(), correlation_id);
                                            }
                                        }
                                        ErrorRecovery::CloseConnection => {
                                            info!("Closing connection to {} due to error", addr_clone);
                                            // Channel will be dropped, closing the connection
                                        }
                                        ErrorRecovery::Throttle(ms) => {
                                            debug!("Throttling client {} for {}ms", addr_clone, ms);
                                            tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            });  // End of spawned handler task
                        }
                        
                        debug!("Connection handler for {} terminated", addr);
                    });
                }
                Err(e) => {
                    error!("Error accepting connection: {}", e);
                }
            }
        }
    }

    /// Flush all partition buffers to ensure data durability before shutdown
    pub async fn flush_all_partitions(&self) -> Result<()> {
        info!("Flushing all partition buffers to storage...");
        self.kafka_handler.flush_all_partitions().await?;
        info!("All partitions flushed successfully");
        Ok(())
    }

    /// Get server statistics
    pub async fn get_stats(&self) -> Result<ServerStats> {
        let topics = self.metadata_store.list_topics().await?;
        let brokers = self.metadata_store.list_brokers().await?;
        
        Ok(ServerStats {
            node_id: self.config.node_id,
            topics_count: topics.len(),
            brokers_count: brokers.len(),
            advertised_address: format!("{}:{}", 
                self.config.advertised_host,
                self.config.advertised_port
            ),
        })
    }

    /// Restore high watermarks from segment metadata on startup
    ///
    /// This is critical for recovery after WAL deletion. Without this, the server
    /// would not know what offsets exist in segments and would report empty partitions.
    async fn restore_high_watermarks_from_segments(
        metadata_store: &Arc<dyn MetadataStore>,
    ) -> Result<()> {
        use std::collections::HashMap;

        // Get all topics
        let topics = metadata_store.list_topics().await
            .map_err(|e| anyhow::anyhow!("Failed to list topics: {}", e))?;

        if topics.is_empty() {
            info!("No topics found, skipping high watermark restoration");
            return Ok(());
        }

        // For each topic, find the maximum offset from segments
        let mut high_watermarks: HashMap<(String, u32), i64> = HashMap::new();

        for topic in topics {
            // List all segments for this topic
            let segments = metadata_store.list_segments(&topic.name, None).await
                .map_err(|e| anyhow::anyhow!("Failed to list segments for topic {}: {}", topic.name, e))?;

            if segments.is_empty() {
                continue;
            }

            // Find maximum end_offset for each partition
            for segment in segments {
                let key = (segment.topic.clone(), segment.partition);
                let current_max = high_watermarks.get(&key).copied().unwrap_or(-1);

                // High watermark should be one past the last offset in the segment
                let segment_high_watermark = segment.end_offset + 1;

                if segment_high_watermark > current_max {
                    high_watermarks.insert(key, segment_high_watermark);
                }
            }
        }

        // Update high watermarks in metadata store
        let mut restored_count = 0;
        for ((topic, partition), high_watermark) in high_watermarks {
            // Check if we already have a high watermark set (from WAL recovery)
            if let Ok(Some((existing_hwm, _))) = metadata_store.get_partition_offset(&topic, partition).await {
                if existing_hwm >= high_watermark {
                    // Already have a higher or equal watermark from WAL, don't overwrite
                    debug!(
                        topic = %topic,
                        partition = partition,
                        existing_hwm = existing_hwm,
                        segment_hwm = high_watermark,
                        "Skipping watermark restore (existing is higher)"
                    );
                    continue;
                }
            }

            // Set the high watermark (log_start_offset = 0 for now)
            metadata_store.update_partition_offset(&topic, partition, high_watermark, 0).await
                .map_err(|e| anyhow::anyhow!("Failed to update high watermark for {}:{}: {}", topic, partition, e))?;

            info!(
                topic = %topic,
                partition = partition,
                high_watermark = high_watermark,
                "Restored high watermark from segments"
            );
            restored_count += 1;
        }

        info!(
            restored_count = restored_count,
            "High watermark restoration complete"
        );

        Ok(())
    }
}

/// Server statistics
#[derive(Debug, Clone)]
pub struct ServerStats {
    pub node_id: i32,
    pub topics_count: usize,
    pub brokers_count: usize,
    pub advertised_address: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_server_creation() {
        let config = IntegratedServerConfig {
            data_dir: "/tmp/chronik-test".to_string(),
            ..Default::default()
        };
        
        let server = IntegratedKafkaServer::new(config, None).await.unwrap();
        let stats = server.get_stats().await.unwrap();
        
        assert_eq!(stats.node_id, 1);
        assert_eq!(stats.brokers_count, 1);
    }
}