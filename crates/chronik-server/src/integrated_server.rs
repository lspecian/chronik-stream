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
use chronik_common::metadata::{
    file_store::FileMetadataStore,
    traits::MetadataStore,
    ChronikMetaLogStore,
};

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
    /// Use WAL-based metadata store instead of file-based
    pub use_wal_metadata: bool,
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
            use_wal_metadata: true, // Default to WAL-based metadata
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
}

impl Clone for IntegratedKafkaServer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            kafka_handler: self.kafka_handler.clone(),
            metadata_store: self.metadata_store.clone(),
            wal_indexer: self.wal_indexer.clone(),
            metadata_uploader: self.metadata_uploader.clone(),
        }
    }
}

impl IntegratedKafkaServer {
    /// Create a new integrated Kafka server
    pub async fn new(config: IntegratedServerConfig) -> Result<Self> {
        Self::new_with_raft(config, None).await
    }

    /// Create a new integrated Kafka server with optional Raft manager
    #[cfg(feature = "raft")]
    pub async fn new_with_raft(
        config: IntegratedServerConfig,
        raft_manager: Option<Arc<crate::raft_integration::RaftReplicaManager>>,
    ) -> Result<Self> {
        info!("Initializing integrated Kafka server with chronik-ingest components");
        if raft_manager.is_some() {
            info!("Raft integration enabled");
        }

        Self::new_internal(config, raft_manager).await
    }

    /// Create a new integrated Kafka server (without raft feature)
    #[cfg(not(feature = "raft"))]
    pub async fn new_with_raft(
        config: IntegratedServerConfig,
        _raft_manager: Option<()>,
    ) -> Result<Self> {
        info!("Initializing integrated Kafka server with chronik-ingest components");
        Self::new_internal(config, None).await
    }

    /// Internal server initialization
    async fn new_internal(
        config: IntegratedServerConfig,
        #[cfg(feature = "raft")] raft_manager: Option<Arc<crate::raft_integration::RaftReplicaManager>>,
        #[cfg(not(feature = "raft"))] _raft_manager: Option<()>,
    ) -> Result<Self> {
        info!("Starting internal server initialization");

        // Create data directory
        std::fs::create_dir_all(&config.data_dir)?;
        let segments_dir = format!("{}/segments", config.data_dir);
        std::fs::create_dir_all(&segments_dir)?;
        
        // Initialize metadata store based on configuration
        let metadata_store: Arc<dyn MetadataStore> = if let Some(ref cluster_config) = config.cluster_config {
            // Cluster mode: Use Raft-replicated metadata store
            #[cfg(feature = "raft")]
            {
                // CRITICAL VALIDATION: Multi-node Raft clustering requires raft-cluster mode, not standalone
                // The standalone mode lacks distributed bootstrap coordination needed for multi-node clusters
                // ONLY enforce this when raft_manager is None (standalone mode)
                // When raft_manager is Some, we're in raft-cluster mode which properly handles multi-node
                let total_nodes = cluster_config.peers.len();

                if total_nodes > 1 && raft_manager.is_none() {
                    return Err(anyhow::anyhow!(
                        "Multi-node Raft clustering ({} nodes total) is not supported in 'standalone' mode.\n\
                         \n\
                         The standalone mode lacks distributed bootstrap coordination required for multi-node clusters.\n\
                         Each node would create its __meta partition replica independently without quorum agreement,\n\
                         leading to cluster formation failures.\n\
                         \n\
                         Please use one of the following instead:\n\
                         \n\
                         Option 1 (Recommended): Use raft-cluster mode with health-check bootstrap\n\
                         ========================================================================\n\
                         cargo run --features raft --bin chronik-server -- raft-cluster \\\n\
                           --node-id 1 \\\n\
                           --raft-addr 0.0.0.0:9192 \\\n\
                           --peers \"2@node2:9292,3@node3:9392\"\n\
                         \n\
                         Option 2: Use raft-cluster mode (requires manual startup coordination)\n\
                         =========================================================================\n\
                         See docs/CLUSTERING_GUIDE.md for detailed multi-node setup instructions.\n\
                         \n\
                         Note: Single-node Raft is supported in standalone mode for development/testing.",
                        total_nodes
                    ));
                }

                if total_nodes > 1 {
                    info!("Initializing Raft-replicated metadata store for {}-node cluster mode", total_nodes);
                } else {
                    info!("Initializing Raft-replicated metadata store for single-node cluster mode");
                }

                use chronik_raft::RaftMetaLog;
                use chronik_wal::{GroupCommitWal, GroupCommitConfig, RaftWalStorage};

                // Raft manager must be provided in cluster mode
                let raft_mgr = raft_manager.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Raft manager required in cluster mode"))?;

                // Extract ALL peer IDs from cluster config (INCLUDING self)
                // CRITICAL: For Raft bootstrap, all nodes must have identical peer lists
                let peer_ids: Vec<u64> = cluster_config.peers.iter()
                    .map(|p| p.id)
                    .collect();

                // Create WAL-backed storage for __meta partition (standalone --raft mode)
                // NOTE: In raft-cluster mode, this is already created in raft_cluster.rs
                let meta_wal_config = GroupCommitConfig::default();
                let meta_wal_dir = PathBuf::from(&config.data_dir).join("wal/__meta/0");
                tokio::fs::create_dir_all(&meta_wal_dir).await?;
                let meta_wal = Arc::new(GroupCommitWal::new(meta_wal_dir.clone(), meta_wal_config));
                let meta_log_storage = Arc::new(RaftWalStorage::new(meta_wal));

                // Recover any existing Raft log entries from WAL
                meta_log_storage.recover(&PathBuf::from(&config.data_dir)).await?;
                info!("Metadata Raft log storage initialized (WAL-backed with persistence)");

                // CRITICAL FIX: Check if __meta partition replica already exists (created by raft_cluster.rs)
                // If it does, retrieve the SHARED metadata state from RaftReplicaManager
                // If not, create our own state (for standalone --raft mode)
                let (local_state, applied_index, meta_replica) = if let Some(replica) = raft_mgr.get_replica("__meta", 0) {
                    info!("Using existing __meta partition replica (created by raft_cluster.rs)");

                    // Retrieve the shared state from RaftReplicaManager
                    let shared_state = raft_mgr.get_meta_partition_state().await
                        .ok_or_else(|| anyhow::anyhow!("__meta replica exists but shared state not found in RaftReplicaManager"))?;

                    info!("Retrieved shared metadata state from RaftReplicaManager (state will be shared between MetadataStateMachine and RaftMetaLog)");

                    (shared_state.0, shared_state.1, replica)
                } else {
                    // No existing replica - create our own state (standalone --raft mode)
                    info!("No existing __meta replica found, creating new one with fresh state");
                    let local_state = Arc::new(parking_lot::RwLock::new(chronik_raft::MetadataState::default()));
                    let applied_index = Arc::new(std::sync::atomic::AtomicU64::new(0));

                    // CRITICAL FIX: Create topic creation callback to enable Raft replication for data partitions
                    // When a topic is created via metadata, we need to create Raft replicas for each partition
                    let raft_mgr_for_callback = raft_mgr.clone();
                    let peer_ids_for_callback = peer_ids.clone();
                    let data_dir_for_callback = PathBuf::from(&config.data_dir);

                    let topic_created_callback: chronik_raft::TopicCreatedCallback = Arc::new(move |topic_name: &str, topic_meta: &chronik_common::metadata::TopicMetadata| {
                        info!("Topic creation callback fired for '{}'", topic_name);

                        // Skip __meta partition (already created)
                        if topic_name == "__meta" {
                            return;
                        }

                        // Create Raft replicas for each partition asynchronously
                        let topic = topic_name.to_string();
                        let partition_count = topic_meta.config.partition_count;
                        let raft_mgr = raft_mgr_for_callback.clone();
                        let peers = peer_ids_for_callback.clone();
                        let data_dir = data_dir_for_callback.clone();

                        tokio::spawn(async move {
                            info!("Creating Raft replicas for topic '{}' with {} partitions", topic, partition_count);

                            for partition_id in 0..partition_count {
                                // Create WAL-backed storage for this partition
                                let partition_wal_config = chronik_wal::GroupCommitConfig::default();
                                let partition_wal_dir = data_dir.join(format!("wal/{}/{}", topic, partition_id));

                                if let Err(e) = tokio::fs::create_dir_all(&partition_wal_dir).await {
                                    error!("Failed to create WAL directory for {}-{}: {}", topic, partition_id, e);
                                    continue;
                                }

                                let partition_wal = Arc::new(chronik_wal::GroupCommitWal::new(partition_wal_dir.clone(), partition_wal_config));
                                let log_storage = Arc::new(chronik_wal::RaftWalStorage::new(partition_wal));

                                // Recover any existing Raft log entries
                                if let Err(e) = log_storage.recover(&data_dir).await {
                                    error!("Failed to recover Raft log for {}-{}: {}", topic, partition_id, e);
                                    continue;
                                }

                                // Create Raft replica for this partition
                                match raft_mgr.create_replica(
                                    topic.clone(),
                                    partition_id as i32,
                                    log_storage,
                                    peers.clone(),
                                ).await {
                                    Ok(()) => {
                                        info!("Successfully created Raft replica for {}-{} with {} peers", topic, partition_id, peers.len());
                                    }
                                    Err(e) => {
                                        error!("Failed to create Raft replica for {}-{}: {}", topic, partition_id, e);
                                    }
                                }
                            }

                            info!("Completed Raft replica creation for topic '{}'", topic);
                        });
                    });

                    // Create MetadataStateMachine with the topic creation callback
                    let metadata_sm = chronik_raft::MetadataStateMachine::with_callback(
                        local_state.clone(),
                        applied_index.clone(),
                        Some(topic_created_callback),
                    );
                    let state_machine: Arc<tokio::sync::RwLock<dyn chronik_raft::StateMachine>> =
                        Arc::new(tokio::sync::RwLock::new(metadata_sm));

                    // Create __meta partition replica with MetadataStateMachine
                    // All nodes start with SAME peer configuration for proper Raft bootstrap
                    // This allows distributed consensus once all nodes are online
                    info!("Creating __meta partition replica with MetadataStateMachine and {} peers", peer_ids.len());
                    raft_mgr.create_meta_replica(
                        "__meta".to_string(),
                        0,
                        meta_log_storage,
                        peer_ids.clone(),  // All peers from cluster config
                        state_machine,
                    ).await?;

                    // Get the replica we just created
                    let replica = raft_mgr.get_replica("__meta", 0)
                        .ok_or_else(|| anyhow::anyhow!("Failed to get __meta replica after creation"))?;

                    // Return the tuple
                    (local_state, applied_index, replica)
                };

                // Create RaftMetaLog using the shared replica and RaftClient
                // CRITICAL: Pass the SAME local_state and applied_index that we gave to MetadataStateMachine
                // Topic creation callback is set above in MetadataStateMachine::with_callback()
                let raft_meta_log = RaftMetaLog::from_replica(
                    cluster_config.node_id,
                    meta_replica.clone(),
                    Some(raft_mgr.raft_client()),
                    local_state,
                    applied_index,
                ).await?;

                info!("RaftMetaLog initialized for node {} with {} peers (callback set in MetadataStateMachine)", cluster_config.node_id, peer_ids.len());

                // No need to manually add peers via ConfChange for new cluster bootstrap
                // All peers are already in ConfState (initialized in replica.rs)
                // TiKV Raft will elect leader naturally via pre-vote and election
                info!(
                    "New cluster bootstrap: all {} peers initialized in ConfState, natural leader election will occur",
                    cluster_config.peers.len()
                );

                Arc::new(raft_meta_log) as Arc<dyn MetadataStore>
            }
            #[cfg(not(feature = "raft"))]
            {
                return Err(anyhow::anyhow!("Cluster mode requires the 'raft' feature to be enabled. Please rebuild with --features raft"));
            }
        } else if config.use_wal_metadata {
            info!("Initializing WAL-based metadata store with real WAL adapter");

            // Create WAL configuration for metadata
            let wal_config = chronik_wal::config::WalConfig {
                enabled: true,
                data_dir: PathBuf::from(format!("{}/wal_metadata", config.data_dir)),
                segment_size: 50 * 1024 * 1024, // 50MB segments for metadata
                flush_interval_ms: 100, // Flush every 100ms
                flush_threshold: 1024 * 1024, // 1MB buffer threshold
                compression: chronik_wal::config::CompressionType::None,
                checkpointing: chronik_wal::config::CheckpointConfig {
                    enabled: true,
                    interval_records: 1000,
                    interval_bytes: 10 * 1024 * 1024,
                },
                rotation: chronik_wal::config::RotationConfig {
                    max_segment_size: 50 * 1024 * 1024,
                    max_segment_age_ms: 60 * 60 * 1000, // 1 hour
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

            let metalog_store = ChronikMetaLogStore::new(
                wal_adapter,
                PathBuf::from(format!("{}/metalog_snapshots", config.data_dir)),
            ).await?;

            info!("Successfully initialized WAL-based metadata store with real WAL adapter");

            // Ensure __meta topic exists for metadata storage
            let metalog_store_arc = Arc::new(metalog_store);
            if metalog_store_arc.get_topic(chronik_common::metadata::METADATA_TOPIC).await?.is_none() {
                info!("Creating internal metadata topic: {}", chronik_common::metadata::METADATA_TOPIC);
                let meta_config = chronik_common::metadata::TopicConfig {
                    partition_count: 1, // Metadata topic only needs 1 partition
                    replication_factor: 1,
                    retention_ms: None, // Never delete metadata
                    segment_bytes: 50 * 1024 * 1024, // 50MB segments
                    config: {
                        let mut cfg = std::collections::HashMap::new();
                        cfg.insert("compression.type".to_string(), "snappy".to_string());
                        cfg.insert("cleanup.policy".to_string(), "compact".to_string());
                        cfg
                    },
                };
                metalog_store_arc.create_topic(chronik_common::metadata::METADATA_TOPIC, meta_config).await?;
                info!("Successfully created internal metadata topic");
            }

            metalog_store_arc
        } else {
            info!("Initializing file-based metadata store (legacy mode)");
            let metadata_dir = format!("{}/metadata", config.data_dir);
            std::fs::create_dir_all(&metadata_dir)?;

            match FileMetadataStore::new(&metadata_dir).await {
                Ok(store) => {
                    info!("Successfully initialized file-based metadata store (legacy) at {}", metadata_dir);
                    Arc::new(store)
                },
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to initialize file-based metadata store: {:?}. Please ensure the metadata directory {} is writable.", e, metadata_dir));
                }
            }
        };
        
        // Register this broker in metadata
        // CRITICAL FIX (v1.3.66): Wait for broker registration BEFORE accepting connections
        // to prevent "no brokers available" errors from clients during cluster startup
        let broker_metadata = BrokerMetadata {
            broker_id: config.node_id,
            host: config.advertised_host.clone(),
            port: config.advertised_port,
            rack: None,
            status: chronik_common::metadata::traits::BrokerStatus::Online,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        if config.cluster_config.is_some() {
            // Cluster mode: register broker with retry, but BLOCK until successful
            // This ensures metadata is consistent before clients can connect
            info!("Registering broker {} ({}:{}) in Raft metadata (waiting for quorum)...",
                  config.node_id, config.advertised_host, config.advertised_port);

            let mut retry_count = 0;
            let max_retries = 30; // 30 attempts * 2s = 60 seconds max wait
            loop {
                match metadata_store.register_broker(broker_metadata.clone()).await {
                    Ok(_) => {
                        info!("✓ Successfully registered broker {} in Raft metadata", config.node_id);
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
        } else {
            // Standalone mode: register broker synchronously
            info!("Registering broker {} ({}:{}) in metadata store...",
                  config.node_id, config.advertised_host, config.advertised_port);
            metadata_store.register_broker(broker_metadata).await?;
            info!("✓ Successfully registered broker {} in metadata store", config.node_id);
        }

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

        #[cfg(feature = "raft")]
        let wal_manager = if let Some(ref raft_mgr) = raft_manager {
            info!("Reusing WalManager from RaftReplicaManager (ensures shared sealed_segments tracking)");
            raft_mgr.wal_manager()
        } else {
            let wal_manager_recovered = WalManager::recover(&wal_config)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to recover WAL: {}", e))?;
            Arc::new(wal_manager_recovered)
        };

        #[cfg(not(feature = "raft"))]
        let wal_manager = {
            let wal_manager_recovered = WalManager::recover(&wal_config)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to recover WAL: {}", e))?;
            Arc::new(wal_manager_recovered)
        };

        info!("WAL recovery complete - {} partitions loaded", wal_manager.get_partitions().len());

        // Initialize produce handler with inline WAL support (v1.3.47)
        let mut produce_handler_inner = ProduceHandler::new_with_wal(
            produce_config,
            object_store_arc.clone(),
            metadata_store.clone(),
            wal_manager.clone(),
        ).await?;

        // Set Raft manager BEFORE wrapping in Arc (v1.3.66+)
        #[cfg(feature = "raft")]
        if let Some(raft_mgr) = raft_manager {
            info!("Attaching Raft manager to ProduceHandler");
            produce_handler_inner.set_raft_manager(raft_mgr);
        }

        // v2.2.0: Initialize WAL replication manager if followers configured
        if let Ok(followers_str) = std::env::var("CHRONIK_REPLICATION_FOLLOWERS") {
            if !followers_str.is_empty() {
                let followers: Vec<String> = followers_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                if !followers.is_empty() {
                    info!("WAL replication enabled with {} followers: {}", followers.len(), followers.join(", "));
                    let replication_manager = crate::wal_replication::WalReplicationManager::new(followers);
                    produce_handler_inner.set_wal_replication_manager(replication_manager);
                } else {
                    info!("CHRONIK_REPLICATION_FOLLOWERS is empty, WAL replication disabled");
                }
            }
        } else {
            info!("CHRONIK_REPLICATION_FOLLOWERS not set, WAL replication disabled");
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

        let metadata_uploader = if config.enable_metadata_dr && config.use_wal_metadata && is_remote_object_store {
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
            if !config.use_wal_metadata {
                info!("Metadata Uploader disabled (file-based metadata store in use)");
            } else if !is_remote_object_store {
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

        // Clone cluster config before moving config into server
        #[cfg(feature = "raft")]
        let cluster_config_clone = config.cluster_config.clone();

        // Create default topic on startup to ensure clients can connect
        // This solves the chicken-and-egg problem where clients need at least one topic
        // in metadata responses before they can produce messages
        let server = Self {
            config,
            kafka_handler,
            metadata_store: metadata_store.clone(),
            wal_indexer,
            metadata_uploader,
        };

        // Create default topic
        info!("Creating default topic 'chronik-default' for client compatibility");
        if let Err(e) = server.ensure_default_topic().await {
            warn!("Failed to create default topic on startup: {:?}", e);
        }

        // If clustering is enabled, perform initial partition assignment
        #[cfg(feature = "raft")]
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
        if let Ok(receiver_addr) = std::env::var("CHRONIK_WAL_RECEIVER_ADDR") {
            if !receiver_addr.is_empty() {
                info!("WAL receiver enabled on {}", receiver_addr);
                let wal_receiver = crate::wal_replication::WalReceiver::new(
                    receiver_addr.clone(),
                    wal_manager.clone(),
                );

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
    #[cfg(feature = "raft")]
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
                            pending_responses.insert(sequence, (correlation_id, response_data));

                            // Send all consecutive responses starting from next_sequence
                            while let Some((corr_id, resp_data)) = pending_responses.remove(&next_sequence) {
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

                                tracing::debug!("Sent response: sequence={}, correlation_id={}", next_sequence, corr_id);
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

                                    if response.is_flexible {
                                        if response.api_key != chronik_protocol::parser::ApiKey::ApiVersions {
                                            header_bytes.push(0);
                                        }
                                    }

                                    let mut full_response = Vec::with_capacity(header_bytes.len() + response.body.len() + 4);
                                    let size = (header_bytes.len() + response.body.len()) as i32;
                                    full_response.extend_from_slice(&size.to_be_bytes());
                                    full_response.extend_from_slice(&header_bytes);
                                    full_response.extend_from_slice(&response.body);

                                    // Measure channel send delay to detect backpressure
                                    let send_start = std::time::Instant::now();
                                    // Send response with sequence number for ordering
                                    if let Err(e) = response_sender.send((sequence, response.header.correlation_id, full_response)).await {
                                        error!("Failed to send response to writer for addr={}: {}", addr_clone, e);
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
        
        let server = IntegratedKafkaServer::new(config).await.unwrap();
        let stats = server.get_stats().await.unwrap();
        
        assert_eq!(stats.node_id, 1);
        assert_eq!(stats.brokers_count, 1);
    }
}